// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for implicit `CloseSession` (session GC).

use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AddSenderRequest, CloseSessionRequest, NodeConfig, OpenSessionRequest, RemoveResourceRequest,
    SubscribeActivationsRequest, Transport as ProtoTransport,
};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::Code;
use tower::service_fn;

struct DaemonHarness {
    _dir: TempDir,
    uds: PathBuf,
    child: Child,
}

impl DaemonHarness {
    fn spawn(subscribe_sec: u64, resubscribe_sec: u64) -> Self {
        let dir = TempDir::new().expect("tempdir");
        let uds = dir.path().join("nvnmosd.sock");
        nvnmosd::uds::prepare_listen_path(&uds).expect("prepare UDS path");
        let bin = env!("CARGO_BIN_EXE_nvnmosd");
        let lib_dir = find_libnvnmos_dir();
        let ld_library_path = prepend_ld_library_path(&lib_dir);
        let child = Command::new(bin)
            .arg("--uds")
            .arg(&uds)
            .env("NVNMOSD_SESSION_GC", "1")
            .env(
                "NVNMOSD_SESSION_SUBSCRIBE_TIMEOUT_SEC",
                subscribe_sec.to_string(),
            )
            .env(
                "NVNMOSD_SESSION_RESUBSCRIBE_TIMEOUT_SEC",
                resubscribe_sec.to_string(),
            )
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

/// Directory containing `libnvnmos.so` for the spawned child process.
fn find_libnvnmos_dir() -> String {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("NVNMOS_LIB_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build"),
    );
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("../build"));
        candidates.push(cwd.join("build"));
    }

    for candidate in candidates {
        if let Some(abs) = absolutize_lib_dir(&candidate) {
            return abs;
        }
    }

    panic!(
        "could not find libnvnmos.so; build the C++ library (cmake --build build) \
         or set NVNMOS_LIB_DIR to the directory containing libnvnmos.so"
    );
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

fn child_stderr(child: &mut Child) -> String {
    use std::io::Read;
    child
        .stderr
        .take()
        .and_then(|mut stderr| {
            let mut buf = String::new();
            stderr.read_to_string(&mut buf).ok()?;
            Some(buf)
        })
        .unwrap_or_default()
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn wait_for_daemon(uds: &Path, child: &mut Child) {
    for _ in 0..200 {
        if let Ok(Some(status)) = child.try_wait() {
            let stderr = child_stderr(child);
            panic!(
                "nvnmosd exited before binding UDS (status={status}); \
                 ensure libnvnmos.so is in LD_LIBRARY_PATH (build the C++ \
                 library under ../../build or set NVNMOS_LIB_DIR). stderr:\n{stderr}"
            );
        }
        if UnixStream::connect(uds).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let stderr = child_stderr(child);
    panic!(
        "nvnmosd did not become ready on {}; stderr:\n{stderr}",
        uds.display()
    );
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
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(sock) => sock,
        Err(_) => return "127.0.0.1".to_string(),
    };
    if sock.connect("8.8.8.8:80").is_err() {
        return "127.0.0.1".to_string();
    }
    sock.local_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn minimal_sender_sdp(name: &str, iface_ip: &str) -> String {
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {iface_ip}\r\n\
         s=session-gc-test\r\n\
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

async fn open_session(client: &mut NvnmosDaemonClient<Channel>, seed: &str) -> String {
    client
        .open_session(OpenSessionRequest {
            node_config: Some(NodeConfig {
                seed: seed.to_string(),
                // Let the daemon allocate from NVNMOSD_HTTP_PORT_MIN..MAX.
                // A fixed port races when these tests run in parallel (default
                // `cargo test` uses multiple threads).
                http_port: 0,
                ..Default::default()
            }),
        })
        .await
        .expect("OpenSession")
        .into_inner()
        .session_handle
}

fn expect_code(err: tonic::Status, expected: Code) {
    assert_eq!(
        err.code(),
        expected,
        "unexpected gRPC status: {err}"
    );
}

/// Case A — subscribe before add.
#[tokio::test]
async fn subscribe_required_before_add() {
    let mut harness = DaemonHarness::spawn(5, 2);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let seed = "session-gc-a";
    let iface = autodetect_iface_ip();
    let session = open_session(&mut client, seed).await;

    let err = client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "sender-a".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: minimal_sender_sdp("sender-a", &iface),
        })
        .await
        .expect_err("AddSender without subscribe");
    expect_code(err, Code::FailedPrecondition);

    let _sub = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeActivations")
        .into_inner();

    client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "sender-a".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: minimal_sender_sdp("sender-a", &iface),
        })
        .await
        .expect("AddSender after subscribe");

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
}

/// Case B — resubscribe within timeout; watchdog cancelled while stream open.
#[tokio::test]
async fn resubscribe_cancels_watchdog() {
    let mut harness = DaemonHarness::spawn(5, 2);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let seed = "session-gc-b";
    let iface = autodetect_iface_ip();
    let session = open_session(&mut client, seed).await;

    let _sub = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("first subscribe")
        .into_inner();

    let add = client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "sender-b".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: minimal_sender_sdp("sender-b", &iface),
        })
        .await
        .expect("AddSender")
        .into_inner();

    drop(_sub);
    tokio::time::sleep(Duration::from_secs(1)).await;

    let _sub2 = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("resubscribe")
        .into_inner();

    tokio::time::sleep(Duration::from_secs(3)).await;

    client
        .remove_resource(RemoveResourceRequest {
            session_handle: session.clone(),
            resource_handle: add.resource_handle,
        })
        .await
        .expect("RemoveResource after long hold");

    client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await
        .expect("CloseSession");
}

/// Case C — resubscribe timeout triggers implicit CloseSession.
#[tokio::test]
async fn resubscribe_timeout_closes_session() {
    let mut harness = DaemonHarness::spawn(5, 2);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let seed = "session-gc-c";
    let iface = autodetect_iface_ip();
    let session = open_session(&mut client, seed).await;

    let _sub = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("subscribe")
        .into_inner();

    client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "sender-c".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: minimal_sender_sdp("sender-c", &iface),
        })
        .await
        .expect("AddSender");

    drop(_sub);
    tokio::time::sleep(Duration::from_secs(3)).await;

    let err = client
        .close_session(CloseSessionRequest {
            session_handle: session.clone(),
        })
        .await
        .expect_err("old session should be gone");
    expect_code(err, Code::NotFound);

    let session2 = open_session(&mut client, seed).await;
    let _sub2 = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session2.clone(),
        })
        .await
        .expect("subscribe on new session")
        .into_inner();

    client
        .add_sender(AddSenderRequest {
            session_handle: session2.clone(),
            name: "sender-c".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: minimal_sender_sdp("sender-c", &iface),
        })
        .await
        .expect("reuse name after implicit close");

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session2,
        })
        .await;
}

/// Case D — subscribe timeout after OpenSession.
#[tokio::test]
async fn subscribe_timeout_closes_session() {
    let mut harness = DaemonHarness::spawn(5, 2);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let seed = "session-gc-d";
    let session = open_session(&mut client, seed).await;

    tokio::time::sleep(Duration::from_secs(6)).await;

    let err = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect_err("session should be gone");
    expect_code(err, Code::NotFound);

    open_session(&mut client, seed).await;
}
