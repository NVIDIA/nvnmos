// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared helpers for nvnmosd integration tests that spawn the daemon.

// Each integration test binary pulls in this module but uses a different
// subset, so items unused by a given binary are expected, not dead.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{NodeConfig, OpenSessionRequest};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream as AsyncTcpStream, UnixStream};
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

const HTTP_BUDGET: Duration = Duration::from_secs(30);

pub struct DaemonHarness {
    _dir: TempDir,
    pub uds: PathBuf,
    child: Child,
}

impl DaemonHarness {
    /// Spawn `nvnmosd` on a temp UDS. Default env: session GC off, malloc trim
    /// off, `NVNMOS_EXPERIMENTAL_SETTINGS` cleared. `extra_env` overlays that
    /// (and can re-enable Settings or session GC).
    pub fn spawn(extra_env: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().expect("tempdir");
        let uds = dir.path().join("nvnmosd.sock");
        nvnmosd::uds::prepare_listen_path(&uds).expect("prepare UDS path");
        let bin = env!("CARGO_BIN_EXE_nvnmosd");
        let lib_dir = find_libnvnmos_dir();
        let ld_library_path = prepend_ld_library_path(&lib_dir);
        let mut command = Command::new(bin);
        command
            .arg("--uds")
            .arg(&uds)
            .env("NVNMOSD_SESSION_GC", "0")
            .env("NVNMOSD_MALLOC_TRIM", "0")
            .env("RUST_LOG", "error")
            .env("LD_LIBRARY_PATH", &ld_library_path)
            .env_remove("NVNMOS_EXPERIMENTAL_SETTINGS")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for &(key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().expect("spawn nvnmosd");
        Self {
            _dir: dir,
            uds,
            child,
        }
    }

    pub async fn ready(&mut self) {
        wait_for_daemon(&self.uds, &mut self.child).await;
    }

    /// Panic if the daemon has already exited, including its wait status and
    /// remaining stderr. No-op while the process is still running.
    pub fn assert_running(&mut self) {
        if let Some((status, stderr)) = self.exited() {
            panic!("nvnmosd exited ({status}); stderr:\n{stderr}");
        }
    }

    fn exited(&mut self) -> Option<(std::process::ExitStatus, String)> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some((status, child_stderr(&mut self.child))),
            _ => None,
        }
    }

    /// Send SIGTERM and wait until the process has exited. Returns stderr.
    /// The daemon shuts down on SIGTERM: in-flight RPCs finish, then it flushes
    /// the annotation checkpoint. Drop open gRPC streams first.
    pub fn terminate(&mut self) -> String {
        if self.child.try_wait().ok().flatten().is_none() {
            // kill(1) -s TERM is SIGTERM. Child::kill() sends SIGKILL.
            let pid = self.child.id().to_string();
            let _ = Command::new("kill").args(["-s", "TERM", &pid]).status();
            let _ = self.child.wait();
        }
        child_stderr(&mut self.child)
    }

    /// Wait until the process exits on its own, without signalling it.
    /// Used when startup is expected to fail.
    pub fn wait_for_exit(&mut self) -> (std::process::ExitStatus, String) {
        let status = self.child.wait().expect("wait for nvnmosd");
        (status, child_stderr(&mut self.child))
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                if self.child.stderr.is_some() {
                    let stderr = child_stderr(&mut self.child);
                    eprintln!("nvnmosd already exited ({status}); stderr:\n{stderr}");
                }
            }
            _ => {
                // SIGKILL skips shutdown, including the annotation checkpoint flush.
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

fn find_libnvnmos_dir() -> String {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("NVNMOS_LIB_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build"));
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

pub async fn connect(uds: &Path) -> NvnmosDaemonClient<Channel> {
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

pub async fn open_session_with_port(
    client: &mut NvnmosDaemonClient<Channel>,
    seed: &str,
    http_port: u16,
) -> String {
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
    assert_eq!(resp.http_port as u16, http_port);
    resp.session_handle
}

pub fn autodetect_iface_ip() -> String {
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

pub fn ephemeral_http_port() -> u16 {
    std::net::TcpListener::bind("0.0.0.0:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("addr")
        .port()
}

pub async fn http_request(
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

    tokio::time::timeout(HTTP_BUDGET, async {
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
    .map_err(|_| format!("{method} {path} timed out after {HTTP_BUDGET:?}"))?
}

pub async fn http_get(host: &str, port: u16, path: &str) -> Result<(u16, String), String> {
    http_request("GET", host, port, path, None).await
}

pub async fn http_patch(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
) -> Result<(u16, String), String> {
    http_request("PATCH", host, port, path, Some(body)).await
}

/// PATCH `/staged` with `activation.mode = activate_immediate` and
/// `master_enable: true`, copying transport binding from `/active`.
///
/// `transport_file` is SDP `data` (`type` is `application/sdp`). When
/// `mxl_flow_id` is set (MXL receivers), transport_params also get
/// `mxl_domain_id` `"auto"`.
pub async fn http_patch_activate_immediate(
    host: &str,
    port: u16,
    staged_path: &str,
    transport_file: Option<&str>,
    mxl_flow_id: Option<&str>,
) -> Result<(), String> {
    let active_path = staged_path.trim_end_matches("/staged");
    let (status, body) = http_get(host, port, &format!("{active_path}/active")).await?;
    if !(200..300).contains(&status) {
        return Err(format!("GET /active returned HTTP {status}: {body}"));
    }
    let json_start = body
        .find('{')
        .ok_or_else(|| format!("no JSON in GET /active: {body}"))?;
    let active: serde_json::Value =
        serde_json::from_str(&body[json_start..]).map_err(|e| e.to_string())?;
    let mut patch = serde_json::json!({
        "master_enable": true,
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
    if let Some(sdp) = transport_file {
        patch["transport_file"] = serde_json::json!({
            "type": "application/sdp",
            "data": sdp,
        });
    }
    if let Some(flow_id) = mxl_flow_id {
        let legs = patch["transport_params"]
            .as_array_mut()
            .filter(|legs| !legs.is_empty())
            .ok_or_else(|| "no transport_params to set mxl_flow_id".to_string())?;
        for leg in legs {
            leg["mxl_flow_id"] = serde_json::Value::String(flow_id.to_string());
            leg["mxl_domain_id"] = serde_json::Value::String("auto".to_string());
        }
    }
    let (status, resp) = http_patch(host, port, staged_path, &patch.to_string()).await?;
    if !(200..300).contains(&status) {
        return Err(format!("PATCH /staged returned HTTP {status}: {resp}"));
    }
    Ok(())
}

pub async fn http_get_json(http_port: u16, path: &str) -> serde_json::Value {
    let (status, resp) = http_get("127.0.0.1", http_port, path)
        .await
        .unwrap_or_else(|e| panic!("GET {path}: {e}"));
    assert!((200..300).contains(&status), "GET {path} failed: {resp}");
    let start = resp
        .find(['{', '['])
        .unwrap_or_else(|| panic!("no JSON in GET {path}: {resp}"));
    serde_json::from_str(resp[start..].trim())
        .unwrap_or_else(|e| panic!("JSON {path}: {e}: {resp}"))
}
