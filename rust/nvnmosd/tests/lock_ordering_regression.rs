// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Regression tests for the `state` ↔ `model` lock inversion; see
//! `doc/designs/nvnmosd/lock-ordering.md`.
//!
//! These tests drive an **in-band** IS-05 activation (HTTP PATCH of `/staged`)
//! so the libnvnmos activation thread holds `model` and blocks on the client
//! ack, then issue a concurrent gRPC mutation on the same Node. They must
//! pass after the "no FFI under `state`" fix; they hang or time out on the
//! pre-fix daemon.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AddSenderRequest, CloseSessionRequest, NodeConfig, OpenSessionRequest,
    SubscribeActivationsRequest, Transport as ProtoTransport,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream as AsyncTcpStream, UnixStream};
use tokio::sync::oneshot;
use tokio_stream::StreamExt;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

/// Upper bound the concurrent RPC is allowed to take. On the pre-fix daemon the
/// RPC deadlocks and never returns, so any finite budget fails it. Post-fix the
/// RPC must complete: `CloseSession` returns promptly (it aborts the parked
/// activation), while `AddSender` waits — correctly — for libnvnmos's `model`
/// lock, which the parked activation holds until its `ACTIVATION_ACK_TIMEOUT`
/// (5 s) elapses. So the budget must exceed that ack timeout with margin.
const CONCURRENT_RPC_BUDGET: Duration = Duration::from_secs(15);
const PATCH_HTTP_BUDGET: Duration = Duration::from_secs(30);

struct DaemonHarness {
    _dir: TempDir,
    uds: PathBuf,
    child: Child,
}

impl DaemonHarness {
    fn spawn() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let uds = dir.path().join("nvnmosd.sock");
        nvnmosd::uds::prepare_listen_path(&uds).expect("prepare UDS path");
        let bin = env!("CARGO_BIN_EXE_nvnmosd");
        let lib_dir = find_libnvnmos_dir();
        let ld_library_path = prepend_ld_library_path(&lib_dir);
        let child = Command::new(bin)
            .arg("--uds")
            .arg(&uds)
            .env("NVNMOSD_SESSION_GC", "0")
            .env("NVNMOSD_MALLOC_TRIM", "0")
            .env("RUST_LOG", "error")
            .env("LD_LIBRARY_PATH", &ld_library_path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn nvnmosd");
        Self {
            _dir: dir,
            uds,
            child,
        }
    }

    async fn ready(&mut self) {
        wait_for_daemon(&self.uds, &mut self.child).await;
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn find_libnvnmos_dir() -> String {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("NVNMOS_LIB_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build"));
    for candidate in candidates {
        if let Some(abs) = absolutize_lib_dir(&candidate) {
            return abs;
        }
    }
    panic!("could not find libnvnmos.so; set NVNMOS_LIB_DIR");
}

fn absolutize_lib_dir(path: &Path) -> Option<String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let abs = abs.canonicalize().ok()?;
    if abs.join("libnvnmos.so").is_file() {
        Some(abs.to_string_lossy().into_owned())
    } else {
        None
    }
}

fn prepend_ld_library_path(dir: &str) -> String {
    match std::env::var("LD_LIBRARY_PATH") {
        Ok(existing) if !existing.is_empty() => format!("{dir}:{existing}"),
        _ => dir.to_string(),
    }
}

async fn wait_for_daemon(uds: &Path, child: &mut Child) {
    for _ in 0..200 {
        if child.try_wait().ok().flatten().is_some() {
            panic!("nvnmosd exited before binding UDS");
        }
        if UnixStream::connect(uds).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("nvnmosd did not become ready on {}", uds.display());
}

async fn connect(uds: &Path) -> NvnmosDaemonClient<Channel> {
    let uds = uds.to_path_buf();
    let endpoint = Endpoint::try_from("http://[::1]:50051").expect("endpoint uri");
    let channel = endpoint
        .connect_with_connector(service_fn(move |_: Uri| {
            let uds = uds.clone();
            async move {
                let stream = UnixStream::connect(uds).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await
        .expect("connect UDS");
    NvnmosDaemonClient::new(channel)
}

fn autodetect_iface_ip() -> String {
    use std::net::UdpSocket;
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return "127.0.0.1".to_string(),
    };
    if sock.connect("8.8.8.8:80").is_err() {
        return "127.0.0.1".to_string();
    }
    sock.local_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn sender_sdp(name: &str, iface_ip: &str) -> String {
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {iface_ip}\r\n\
         s=lock-ordering-regression\r\n\
         t=0 0\r\n\
         a=x-nvnmos-name:{name}\r\n\
         m=video 5020 RTP/AVP 96\r\n\
         c=IN IP4 233.252.0.0/64\r\n\
         a=source-filter: incl IN IP4 233.252.0.0 {iface_ip}\r\n\
         a=x-nvnmos-iface-ip:{iface_ip}\r\n\
         a=rtpmap:96 raw/90000\r\n\
         a=fmtp:96 sampling=YCbCr-4:2:2; width=1920; height=1080; \
         exactframerate=50; depth=10; TCS=SDR; colorimetry=BT709; \
         PM=2110GPM; SSN=ST2110-20:2017; TP=2110TPN;\r\n\
         a=mediaclk:direct=0\r\n\
         a=x-nvnmos-src-port:5004\r\n\
         a=ts-refclk:localmac=CA-FE-01-CA-FE-02\r\n"
    )
}

fn os_port_free(port: u16) -> bool {
    TcpListener::bind(("0.0.0.0", port)).is_ok()
}

async fn connection_http(
    method: &str,
    host: &str,
    port: u16,
    path: &str,
    body: Option<&str>,
) -> Result<(u16, String), String> {
    let addr = format!("{host}:{port}");
    let request = match body {
        Some(body) => format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {len}\r\n\
             Connection: close\r\n\
             \r\n\
             {body}",
            len = body.len(),
        ),
        None => format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Connection: close\r\n\
             \r\n",
        ),
    };

    tokio::time::timeout(PATCH_HTTP_BUDGET, async {
        let mut sock = AsyncTcpStream::connect(&addr)
            .await
            .map_err(|e| e.to_string())?;
        sock.write_all(request.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut buf = Vec::with_capacity(4096);
        sock.read_to_end(&mut buf)
            .await
            .map_err(|e| e.to_string())?;
        let resp = String::from_utf8_lossy(&buf).into_owned();
        let status_line = resp.lines().next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("bad HTTP status in {status_line:?}"))?;
        Ok((status, resp))
    })
    .await
    .map_err(|_| format!("{method} {path} timed out after {:?}", PATCH_HTTP_BUDGET))?
}

async fn connection_get(host: &str, port: u16, path: &str) -> Result<(u16, String), String> {
    connection_http("GET", host, port, path, None).await
}

async fn connection_patch(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
) -> Result<(u16, String), String> {
    connection_http("PATCH", host, port, path, Some(body)).await
}

/// Build a PATCH body that preserves transport binding from `/active`, mirroring
/// `gst-nmos-rs-demo.sh` `patch_master_enable`.
async fn patch_activate_immediate(
    host: &str,
    port: u16,
    staged_path: &str,
    enable: bool,
) -> Result<(), String> {
    let active_path = staged_path.trim_end_matches("/staged");
    let (status, body) = connection_get(host, port, &format!("{active_path}/active")).await?;
    if !(200..300).contains(&status) {
        return Err(format!("GET /active returned HTTP {status}: {body}"));
    }
    let json_start = body
        .find('{')
        .ok_or_else(|| format!("no JSON in GET /active: {body}"))?;
    let active: serde_json::Value =
        serde_json::from_str(&body[json_start..]).map_err(|e| e.to_string())?;
    let mut patch = serde_json::json!({
        "master_enable": enable,
        "activation": { "mode": "activate_immediate" }
    });
    if let Some(tp) = active.get("transport_params") {
        patch["transport_params"] = tp.clone();
    }
    if let Some(id) = active.get("sender_id") {
        patch["sender_id"] = id.clone();
    }
    if let Some(id) = active.get("receiver_id") {
        patch["receiver_id"] = id.clone();
    }
    let patch_body = patch.to_string();
    let (status, resp) = connection_patch(host, port, staged_path, &patch_body).await?;
    if !(200..300).contains(&status) {
        return Err(format!("PATCH /staged returned HTTP {status}: {resp}"));
    }
    Ok(())
}

struct ParkedActivation {
    _parker: tokio::task::JoinHandle<()>,
}

/// On the first `ActivationEvent`, signal `parked` and hold the ack (no
/// `AckActivation`) until this handle is dropped.
fn park_first_activation_on_stream(
    mut stream: tonic::Streaming<nvnmos_rpc::v1::ActivationEvent>,
) -> (ParkedActivation, oneshot::Receiver<()>) {
    let (parked_tx, parked_rx) = oneshot::channel();
    let parker = tokio::spawn(async move {
        let msg = stream
            .next()
            .await
            .expect("activation stream ended before first event")
            .expect("activation stream error");
        let _ = msg.activation_handle;
        let _ = parked_tx.send(());
        std::future::pending::<()>().await;
    });
    (ParkedActivation { _parker: parker }, parked_rx)
}

async fn open_session_with_port(
    client: &mut NvnmosDaemonClient<Channel>,
    seed: &str,
    http_port: u16,
) -> (String, u16) {
    let resp = client
        .open_session(OpenSessionRequest {
            node_config: Some(NodeConfig {
                seed: seed.to_string(),
                http_port: u32::from(http_port),
                host_addresses: vec!["127.0.0.1".to_string()],
                ..Default::default()
            }),
        })
        .await
        .expect("OpenSession")
        .into_inner();
    (resp.session_handle, resp.http_port as u16)
}

async fn subscribe_activations(
    client: &mut NvnmosDaemonClient<Channel>,
    session_handle: &str,
) -> tonic::Streaming<nvnmos_rpc::v1::ActivationEvent> {
    client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session_handle.to_string(),
        })
        .await
        .expect("SubscribeActivations")
        .into_inner()
}

fn staged_sender_path(resource_id: &str) -> String {
    format!("/x-nmos/connection/v1.1/single/senders/{resource_id}/staged")
}

/// Start an in-band activate_immediate PATCH and wait until the daemon has
/// delivered the activation event on `stream` (client ack withheld).
async fn start_parked_in_band_activation_on_stream(
    stream: tonic::Streaming<nvnmos_rpc::v1::ActivationEvent>,
    http_port: u16,
    resource_id: &str,
) -> ParkedActivation {
    let (parker, parked_rx) = park_first_activation_on_stream(stream);
    let staged_path = staged_sender_path(resource_id);
    let host = "127.0.0.1";
    tokio::spawn(async move {
        if let Err(e) = patch_activate_immediate(host, http_port, &staged_path, true).await {
            eprintln!("[lock-ordering test] PATCH /staged failed: {e}");
        }
    });
    tokio::time::timeout(Duration::from_secs(10), parked_rx)
        .await
        .expect("timed out waiting for in-band activation event")
        .expect("activation parker dropped before signalling");
    parker
}

/// While an in-band IS-05 activation is parked on the client ack, adding
/// another sender on the same Node must not wedge the daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_band_activation_does_not_deadlock_add_sender() {
    let mut harness = DaemonHarness::spawn();
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();

    let port = {
        let l = TcpListener::bind("0.0.0.0:0").expect("ephemeral");
        l.local_addr().expect("addr").port()
    };

    let (session, http_port) = open_session_with_port(&mut client, "lock-add", port).await;
    assert_eq!(http_port, port);

    let stream = subscribe_activations(&mut client, &session).await;

    let s1 = client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "s1".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: sender_sdp("s1", &iface),
        })
        .await
        .expect("AddSender s1")
        .into_inner();

    let parker =
        start_parked_in_band_activation_on_stream(stream, http_port, &s1.resource_id).await;

    let mut add_client = client.clone();
    let add_session = session.clone();
    let add_iface = iface.clone();
    let add_result = tokio::time::timeout(CONCURRENT_RPC_BUDGET, async move {
        add_client
            .add_sender(AddSenderRequest {
                session_handle: add_session,
                name: "s2".to_string(),
                transport: ProtoTransport::Rtp as i32,
                transport_file: sender_sdp("s2", &add_iface),
            })
            .await
    })
    .await;

    drop(parker);

    assert!(
        add_result.is_ok(),
        "AddSender s2 must complete within {:?} while s1 activation is pending \
         (daemon deadlocked?)",
        CONCURRENT_RPC_BUDGET,
    );
    add_result
        .expect("AddSender s2 join")
        .expect("AddSender s2 RPC");

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
}

/// While an in-band IS-05 activation is parked, CloseSession must complete and
/// release the Node HTTP port (no stranded LISTEN socket).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_band_activation_does_not_deadlock_close_session() {
    let mut harness = DaemonHarness::spawn();
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();

    let port = {
        let l = TcpListener::bind("0.0.0.0:0").expect("ephemeral");
        l.local_addr().expect("addr").port()
    };

    let (session, http_port) = open_session_with_port(&mut client, "lock-close", port).await;
    assert_eq!(http_port, port);

    let stream = subscribe_activations(&mut client, &session).await;

    let s1 = client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "s1".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: sender_sdp("s1", &iface),
        })
        .await
        .expect("AddSender s1")
        .into_inner();

    let parker =
        start_parked_in_band_activation_on_stream(stream, http_port, &s1.resource_id).await;

    let mut close_client = client.clone();
    let close_session = session.clone();
    let close_result = tokio::time::timeout(CONCURRENT_RPC_BUDGET, async move {
        close_client
            .close_session(CloseSessionRequest {
                session_handle: close_session,
            })
            .await
    })
    .await;

    drop(parker);

    assert!(
        close_result.is_ok(),
        "CloseSession must complete within {:?} while activation is pending \
         (daemon deadlocked?)",
        CONCURRENT_RPC_BUDGET,
    );
    close_result
        .expect("CloseSession join")
        .expect("CloseSession RPC");

    assert!(
        os_port_free(port),
        "http port {port} must be bindable after CloseSession (LISTEN leaked?)"
    );
}
