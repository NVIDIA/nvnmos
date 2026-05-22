// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nvnmosd-example` — minimal regression client for `nvnmosd`.
//!
//! Modelled on the C `nvnmos-example` in `src/main.c`. This commit
//! exercises both Node lifetimes plus the resource lifecycle end-to-end.
//!
//! **Session-refcounted Node** (`--node-seed`):
//!
//! 1. `OpenSession` — creates the Node, refcount 0→1.
//! 2. `OpenSession` on the same seed — attaches; refcount 1→2; the
//!    returned `node_id` must match (1).
//! 3. `CloseSession` of the first handle — refcount 2→1, Node alive.
//! 4. `CloseSession` of the second handle — refcount 1→0, Node destroyed.
//!
//! **Persistent Node** (`<seed>-persistent`):
//!
//! 5. `AddNode` — creates the persistent Node.
//! 6. `OpenSession` (twice) on its seed — attaches without affecting
//!    lifetime; same `node_id` as (5).
//! 7. `CloseSession` (twice) — Node survives the last close because it
//!    is persistent.
//! 8. `RemoveNode` — tears the Node down explicitly.
//!
//! **Resource lifecycle** (`<seed>-resources`):
//!
//! 9.  `OpenSession` — fresh session-refcounted Node for the resource phase.
//! 10. `AddSender` — register a sender, assert `resource_id` matches
//!     `nvnmos::make_sender_id(seed, internal_id)`.
//! 11. `AddReceiver` — register a receiver, assert `resource_id` matches
//!     `nvnmos::make_receiver_id(seed, internal_id)`.
//! 12. `AddSender` with `internal_id` ≠ SDP's `x-nvnmos-id` — expect
//!     `INVALID_ARGUMENT` (daemon-side mismatch detection).
//! 13. `RemoveResource` — drop the sender; the receiver survives.
//! 14. `CloseSession` — drops the surviving receiver through libnvnmos
//!     and tears down the Node.
//!
//! Subsequent commits will grow this binary into a full regression
//! harness mirroring the C example's interactive flow (observe
//! activations, deactivate, tear down).
//!
//! See `doc/designs/nvnmosd/README.md` for the full design and the
//! current rollout plan.

use std::net::UdpSocket;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use hyper_util::rt::TokioIo;
use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AddNodeRequest, AddNodeResponse, AddReceiverRequest, AddResourceResponse, AddSenderRequest,
    CloseSessionRequest, NodeConfig, OpenSessionRequest, OpenSessionResponse, RemoveNodeRequest,
    RemoveResourceRequest, Transport as ProtoTransport,
};
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::Code;
use tower::service_fn;

#[derive(Parser, Debug)]
#[command(version, about = "nvnmosd example/regression client")]
struct Args {
    /// Path to the UDS socket where nvnmosd is listening.
    #[arg(long, env = "NVNMOSD_UDS", default_value = "/tmp/nvnmosd.sock")]
    uds: PathBuf,

    /// NvNmos node seed to attach to. Created on demand if no other
    /// session is currently attached.
    #[arg(long, default_value = "nvnmosd-example")]
    node_seed: String,

    /// Interface IP to weave into the resource-phase SDPs. libnvnmos
    /// rejects SDPs whose connection IP doesn't match a real interface,
    /// so this needs to be a real outbound address. When unset, the
    /// example autodetects via the routing-table trick (UDP-connect to a
    /// public address, read back the local IP).
    #[arg(long, env = "NVNMOSD_EXAMPLE_INTERFACE_IP")]
    interface_ip: Option<String>,
}

/// Autodetect a routable local IP via the standard "connect a UDP socket
/// to a public destination, then read its local address" trick. The OS
/// fills in the source IP via its routing table without ever sending a
/// packet (UDP `connect` only sets the default destination).
fn autodetect_interface_ip() -> anyhow::Result<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").context("UdpSocket::bind(0.0.0.0:0)")?;
    sock.connect("8.8.8.8:80")
        .context("UdpSocket::connect for routing-table probe")?;
    Ok(sock.local_addr()?.ip().to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let channel = connect_uds(&args.uds)
        .await
        .with_context(|| format!("connecting to nvnmosd at {}", args.uds.display()))?;
    let mut client = NvnmosDaemonClient::new(channel);

    // (1) First OpenSession: creates the Node.
    let a = open(&mut client, &args.node_seed, "first session (creates Node)").await?;

    // (2) Second OpenSession on the same seed: attaches to the existing
    // Node. `node_config` is intentionally omitted because the daemon
    // ignores it when the Node already exists.
    let b = open(&mut client, &args.node_seed, "second session (refcount bump)").await?;

    anyhow::ensure!(
        a.node_id == b.node_id,
        "OpenSession reused the Node but returned a different node_id: {} vs {}",
        a.node_id,
        b.node_id,
    );
    anyhow::ensure!(
        a.session_handle != b.session_handle,
        "two sessions ended up with the same session_handle: {}",
        a.session_handle,
    );

    // (3) Close the first session: refcount 2→1, Node remains.
    close(&mut client, &a.session_handle, "first close (refcount to 1)").await?;

    // (4) Close the second session: refcount 1→0, Node destroyed.
    close(&mut client, &b.session_handle, "second close (Node destroyed)").await?;

    // -------- Persistent Node lifecycle --------
    let persistent_seed = format!("{}-persistent", args.node_seed);

    // (5) AddNode: create a persistent Node.
    let added = add_node(&mut client, &persistent_seed).await?;

    // (6) Two OpenSessions on the persistent seed: attach to it without
    // affecting its lifetime. Both must return the persistent Node's id.
    let c = open(&mut client, &persistent_seed, "first session on persistent Node").await?;
    let d = open(&mut client, &persistent_seed, "second session on persistent Node").await?;
    anyhow::ensure!(
        c.node_id == added.node_id && d.node_id == added.node_id,
        "OpenSession on the persistent seed returned the wrong node_id: \
         added={} session1={} session2={}",
        added.node_id,
        c.node_id,
        d.node_id,
    );

    // (7) Close both sessions. The Node must survive the last close
    // because it is persistent.
    close(
        &mut client,
        &c.session_handle,
        "first close (persistent, Node alive)",
    )
    .await?;
    close(
        &mut client,
        &d.session_handle,
        "last close (persistent, Node still alive)",
    )
    .await?;

    // (8) RemoveNode: now the Node is actually destroyed.
    remove_node(&mut client, &persistent_seed).await?;

    // -------- Resource lifecycle --------
    let resource_seed = format!("{}-resources", args.node_seed);

    // (9) OpenSession: fresh session-refcounted Node for the resource phase.
    let r = open(
        &mut client,
        &resource_seed,
        "resource phase session (creates Node)",
    )
    .await?;

    let iface_ip = match args.interface_ip.clone() {
        Some(ip) => ip,
        None => autodetect_interface_ip().context("autodetect interface IP")?,
    };
    tracing::info!(%iface_ip, "resource phase using interface IP");

    // (10) AddSender on the happy path. Cross-check the daemon's returned
    // resource_id against the pure helper.
    let sender_internal_id = "video-sender-a";
    let sender_resp = add_sender(
        &mut client,
        &r.session_handle,
        sender_internal_id,
        &build_video_sdp(sender_internal_id, true, &iface_ip),
    )
    .await?;
    let expected_sender_id = nvnmos::make_sender_id(&resource_seed, sender_internal_id)
        .context("make_sender_id")?;
    anyhow::ensure!(
        sender_resp.resource_id == expected_sender_id,
        "AddSender returned resource_id {} but make_sender_id({:?}, {:?}) says {}",
        sender_resp.resource_id,
        resource_seed,
        sender_internal_id,
        expected_sender_id,
    );

    // (11) AddReceiver, same cross-check.
    let receiver_internal_id = "video-receiver-a";
    let receiver_resp = add_receiver(
        &mut client,
        &r.session_handle,
        receiver_internal_id,
        &build_video_sdp(receiver_internal_id, false, &iface_ip),
    )
    .await?;
    let expected_receiver_id =
        nvnmos::make_receiver_id(&resource_seed, receiver_internal_id)
            .context("make_receiver_id")?;
    anyhow::ensure!(
        receiver_resp.resource_id == expected_receiver_id,
        "AddReceiver returned resource_id {} but make_receiver_id({:?}, {:?}) says {}",
        receiver_resp.resource_id,
        resource_seed,
        receiver_internal_id,
        expected_receiver_id,
    );

    // (12) Mismatch: claim internal_id="claimed" but build the SDP with a
    // different x-nvnmos-id ("real"). The daemon detects this by asking
    // libnvnmos to look up the claimed id after the add and returns
    // INVALID_ARGUMENT when the lookup misses.
    let mismatch_sdp = build_video_sdp("real-id", true, &iface_ip);
    let mismatch_resp = client
        .add_sender(AddSenderRequest {
            session_handle: r.session_handle.clone(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: mismatch_sdp,
            internal_id: "claimed-id".to_string(),
        })
        .await;
    match mismatch_resp {
        Err(status) if status.code() == Code::InvalidArgument => {
            tracing::info!(
                grpc_message = %status.message(),
                "AddSender correctly rejected internal_id / x-nvnmos-id mismatch"
            );
        }
        Err(status) => anyhow::bail!(
            "AddSender mismatch returned code={:?} (expected InvalidArgument): {}",
            status.code(),
            status.message(),
        ),
        Ok(_) => anyhow::bail!("AddSender mismatch unexpectedly succeeded"),
    }

    // (13) RemoveResource for the sender. The receiver must survive.
    remove_resource(
        &mut client,
        &r.session_handle,
        &sender_resp.resource_handle,
        "remove sender (receiver survives)",
    )
    .await?;

    // (14) CloseSession: drops the surviving receiver through libnvnmos
    // and tears down the (session-refcounted) Node.
    close(
        &mut client,
        &r.session_handle,
        "resource phase close (drops receiver, destroys Node)",
    )
    .await?;

    tracing::info!("done");
    Ok(())
}

async fn open(
    client: &mut NvnmosDaemonClient<Channel>,
    node_seed: &str,
    label: &str,
) -> anyhow::Result<OpenSessionResponse> {
    tracing::info!(node_seed = %node_seed, "OpenSession ({label})");
    let resp = client
        .open_session(OpenSessionRequest {
            node_seed: node_seed.to_string(),
            node_config: Some(NodeConfig {
                seed: node_seed.to_string(),
                ..Default::default()
            }),
        })
        .await
        .with_context(|| format!("OpenSession ({label}) failed"))?
        .into_inner();
    tracing::info!(
        session_handle = %resp.session_handle,
        node_id = %resp.node_id,
        "session open ({label})",
    );
    Ok(resp)
}

async fn close(
    client: &mut NvnmosDaemonClient<Channel>,
    session_handle: &str,
    label: &str,
) -> anyhow::Result<()> {
    tracing::info!(session_handle = %session_handle, "CloseSession ({label})");
    client
        .close_session(CloseSessionRequest {
            session_handle: session_handle.to_string(),
        })
        .await
        .with_context(|| format!("CloseSession ({label}) failed"))?;
    Ok(())
}

async fn add_node(
    client: &mut NvnmosDaemonClient<Channel>,
    node_seed: &str,
) -> anyhow::Result<AddNodeResponse> {
    tracing::info!(node_seed = %node_seed, "AddNode (create persistent Node)");
    let resp = client
        .add_node(AddNodeRequest {
            node_seed: node_seed.to_string(),
            node_config: Some(NodeConfig {
                seed: node_seed.to_string(),
                ..Default::default()
            }),
        })
        .await
        .context("AddNode failed")?
        .into_inner();
    tracing::info!(node_id = %resp.node_id, "AddNode succeeded");
    Ok(resp)
}

async fn remove_node(
    client: &mut NvnmosDaemonClient<Channel>,
    node_seed: &str,
) -> anyhow::Result<()> {
    tracing::info!(node_seed = %node_seed, "RemoveNode (destroy persistent Node)");
    client
        .remove_node(RemoveNodeRequest {
            node_seed: node_seed.to_string(),
        })
        .await
        .context("RemoveNode failed")?;
    Ok(())
}

async fn add_sender(
    client: &mut NvnmosDaemonClient<Channel>,
    session_handle: &str,
    internal_id: &str,
    transport_file: &str,
) -> anyhow::Result<AddResourceResponse> {
    tracing::info!(session_handle, internal_id, "AddSender");
    let resp = client
        .add_sender(AddSenderRequest {
            session_handle: session_handle.to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: transport_file.to_string(),
            internal_id: internal_id.to_string(),
        })
        .await
        .context("AddSender failed")?
        .into_inner();
    tracing::info!(
        resource_handle = %resp.resource_handle,
        resource_id = %resp.resource_id,
        "AddSender succeeded",
    );
    Ok(resp)
}

async fn add_receiver(
    client: &mut NvnmosDaemonClient<Channel>,
    session_handle: &str,
    internal_id: &str,
    transport_file: &str,
) -> anyhow::Result<AddResourceResponse> {
    tracing::info!(session_handle, internal_id, "AddReceiver");
    let resp = client
        .add_receiver(AddReceiverRequest {
            session_handle: session_handle.to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: transport_file.to_string(),
            internal_id: internal_id.to_string(),
        })
        .await
        .context("AddReceiver failed")?
        .into_inner();
    tracing::info!(
        resource_handle = %resp.resource_handle,
        resource_id = %resp.resource_id,
        "AddReceiver succeeded",
    );
    Ok(resp)
}

async fn remove_resource(
    client: &mut NvnmosDaemonClient<Channel>,
    session_handle: &str,
    resource_handle: &str,
    label: &str,
) -> anyhow::Result<()> {
    tracing::info!(session_handle, resource_handle, "RemoveResource ({label})");
    client
        .remove_resource(RemoveResourceRequest {
            session_handle: session_handle.to_string(),
            resource_handle: resource_handle.to_string(),
        })
        .await
        .with_context(|| format!("RemoveResource ({label}) failed"))?;
    Ok(())
}

/// Minimal ST 2110-20 SDP. Cloned from `rust/nvnmos/examples/node.rs`'s
/// `build_video_sdp` (same parameter set; same encoding parameters); the
/// example client is intentionally self-contained, so duplication is
/// preferable to inventing a shared "test fixtures" crate at this stage.
fn build_video_sdp(internal_id: &str, sender: bool, iface_ip: &str) -> String {
    const MULTICAST_IP: &str = "233.252.0.0";
    const SOURCE_IP: &str = "192.0.2.0";
    const DESTINATION_PORT: u16 = 5020;
    const SOURCE_PORT: u16 = 5004;
    const PAYLOAD_TYPE: u8 = 96;
    const ENCODING: &str = "raw/90000";
    const FMTP: &str = "sampling=YCbCr-4:2:2; width=1920; height=1080; \
        exactframerate=50; depth=10; TCS=SDR; colorimetry=BT709; \
        PM=2110GPM; SSN=ST2110-20:2017; TP=2110TPN; ";
    const TS_REFCLK: &str = "a=ts-refclk:localmac=CA-FE-01-CA-FE-02\r\n";

    let mut out = format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {iface_ip}\r\n\
         s=nvnmosd-example video {direction} {internal_id}\r\n\
         t=0 0\r\n\
         a=x-nvnmos-id:{internal_id}\r\n\
         m=video {DESTINATION_PORT} RTP/AVP {PAYLOAD_TYPE}\r\n\
         c=IN IP4 {MULTICAST_IP}/64\r\n\
         a=source-filter: incl IN IP4 {MULTICAST_IP} {filter_src}\r\n\
         a=x-nvnmos-iface-ip:{iface_ip}\r\n\
         a=rtpmap:{PAYLOAD_TYPE} {ENCODING}\r\n\
         a=fmtp:{PAYLOAD_TYPE} {FMTP}\r\n\
         a=mediaclk:direct=0\r\n",
        direction = if sender { "sender" } else { "receiver" },
        filter_src = if sender { iface_ip } else { SOURCE_IP },
    );
    if sender {
        use std::fmt::Write;
        let _ = write!(out, "a=x-nvnmos-src-port:{SOURCE_PORT}\r\n");
        out.push_str(TS_REFCLK);
    }
    out
}

async fn connect_uds(uds: &Path) -> anyhow::Result<Channel> {
    let uds = uds.to_path_buf();
    // The HTTP URI is a placeholder — tonic insists on a Uri to set
    // authority/scheme on outgoing requests, but the custom connector
    // ignores it and dials the UDS path instead.
    let endpoint = Endpoint::try_from("http://[::1]:50051")?;
    let channel = endpoint
        .connect_with_connector(service_fn(move |_: Uri| {
            let uds = uds.clone();
            async move {
                let stream = UnixStream::connect(uds).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await?;
    Ok(channel)
}
