// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nvnmosd-example` — minimal regression client for `nvnmosd`.
//!
//! Modelled on the C `nvnmos-example` in `src/main.c`. Exercises both
//! Node lifetimes plus the resource lifecycle end-to-end.
//!
//! **Session-refcounted Node** (`--node-seed`):
//!
//! 1. `OpenSession` — creates the Node, refcount 0→1; asserts
//!    `created_node = true`.
//! 2. `OpenSession` on the same seed — attaches; refcount 1→2; the
//!    returned `node_id` must match (1) and `created_node = false`.
//! 3. `CloseSession` of the first handle — refcount 2→1, Node alive.
//! 4. `CloseSession` of the second handle — refcount 1→0, Node destroyed.
//!
//! **Persistent Node** (`<seed>-persistent`):
//!
//! 5. `AddNode` — creates the persistent Node.
//! 6. `OpenSession` (twice) on its seed — attaches without affecting
//!    lifetime; same `node_id` as (5); both must report
//!    `created_node = false`.
//! 7. `CloseSession` (twice) — Node survives the last close because it
//!    is persistent.
//! 8. `RemoveNode` — tears the Node down explicitly.
//!
//! **Resource lifecycle** (`<seed>-resources`):
//!
//! 9.  `OpenSession` — fresh session-refcounted Node for the resource phase.
//!     Every `OpenSession` and `AddNode` in this example sets BCP-002-02
//!     `asset_tags` (manufacturer / product / instance_id / functions);
//!     they should appear under `/self.tags` in IS-04 for any of the
//!     Nodes this client touches.
//! 10. `SubscribeActivations` — open the per-session activations stream
//!     and start a background task that auto-acks each event with
//!     `success = true`. Stays alive for the rest of the resource phase
//!     so IS-05 PATCHes against libnvnmos can drive the round-trip.
//! 11. `AddSender` — create a sender, assert `source_id` / `flow_id` /
//!     `sender_id` match the corresponding `nvnmos::make_*_id` helpers.
//! 12. `AddReceiver` — create a receiver with the same `name` as the
//!     sender in (11) to exercise the side-disambiguated namespace, and
//!     assert `receiver_id` matches `nvnmos::make_receiver_id(seed,
//!     name)` (a distinct UUID from the sender's).
//! 13. `SyncResourceState` on the sender with an updated transport_file
//!     (bumped SDP session version) — exercises the (re)activate path.
//!     (`SyncResourceState` deliberately does not fire the activation
//!     callback, so the auto-ack task stays quiet here.)
//! 14. `SyncResourceState` on the sender with `transport_file = None` —
//!     exercises the deactivate path.
//! 15. **In-band activation round-trip**: PATCH libnvnmos's IS-05
//!     Connection API to activate then deactivate the sender. Each
//!     PATCH triggers libnvnmos's activation callback, which the
//!     daemon turns into an `ActivationEvent` on the
//!     `SubscribeActivations` stream; the auto-ack task acks and
//!     relays the event to the main flow, which asserts the
//!     `resource_handle` and `transport_file` presence match the
//!     PATCH. Skipped via `--skip-connection` when libnvnmos's HTTP
//!     server isn't reachable.
//! 16. `AddSender` with `name` ≠ SDP's `x-nvnmos-name` — expect
//!     `INVALID_ARGUMENT` (daemon-side mismatch detection).
//! 17. `RemoveResource` — drop the sender; the receiver survives.
//! 18. Optional hold: with `--hold-secs N>0`, sleep N seconds with the
//!     receiver still registered and the activations stream still open.
//!     Designed for manually `curl`-ing an IS-05 PATCH at libnvnmos's
//!     HTTP API to watch the full SubscribeActivations / AckActivation
//!     round-trip in the daemon logs. With `--hold-secs 0` (the
//!     default) this step is a no-op.
//! 19. `CloseSession` — drops the surviving receiver through libnvnmos,
//!     tears down the Node, and (because the daemon drops the
//!     subscription on close) ends the activations stream so the
//!     background ack task exits cleanly.
//!
//! **Channel mapping lifecycle** (`<seed>-channelmapping`):
//!
//! 20. `OpenSession` — fresh Node for IS-08 channel mapping.
//! 21. `SubscribeChannelMappingActivations` — separate stream from
//!     IS-05; background task auto-acks each event.
//! 22. `AddChannelMapping` without a prior subscribe must fail
//!     (`FAILED_PRECONDITION`); then subscribe and add a channel mapping
//!     with empty I/O ids; assert effective ids `input0` / `output0`.
//! 23. `SyncChannelMappingState` — dense active map (one entry per output
//!     channel); out-of-band publish parallel to `SyncResourceState`.
//! 24. `RemoveChannelMapping` — drop the channel mapping.
//! 25. `CloseSession` — tear down the Node and join the ack task.

use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use hyper_util::rt::TokioIo;
use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AckActivationRequest, AckChannelMappingActivationRequest, ActivationEvent, ActiveMapEntry,
    AddChannelMappingRequest, AddChannelMappingResponse, AddNodeRequest, AddNodeResponse,
    AddReceiverRequest, AddReceiverResponse, AddSenderRequest, AddSenderResponse, AssetConfig,
    ChannelMappingActivationEvent, ChannelMappingInput as ProtoChannelMappingInput,
    ChannelMappingOutput as ProtoChannelMappingOutput,
    ChannelMappingParentType as ProtoChannelMappingParentType, CloseSessionRequest, NodeConfig,
    OpenSessionRequest, OpenSessionResponse, RemoveChannelMappingRequest, RemoveNodeRequest,
    RemoveResourceRequest, Side as ProtoSide, SubscribeActivationsRequest,
    SubscribeChannelMappingActivationsRequest, SyncChannelMappingStateRequest,
    SyncResourceStateRequest, Transport as ProtoTransport,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::mpsc as tokio_mpsc;
use tonic::Code;
use tonic::transport::{Channel, Endpoint, Uri};
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

    /// Seconds to keep the resource-phase session open after the demo
    /// flow finishes, with the activations stream still attached and
    /// the receiver still registered. 0 (the default) exits
    /// immediately. Set to a positive value to manually drive an IS-05
    /// activation against libnvnmos (e.g. via `curl`) and observe the
    /// SubscribeActivations / AckActivation round-trip in the daemon
    /// log.
    #[arg(long, default_value_t = 0)]
    hold_secs: u64,

    /// TCP port to serve the NMOS HTTP APIs on, propagated as
    /// `NodeConfig.http_port`. libnvnmos collapses every HTTP API
    /// (Node, Connection, ...) onto this single port, so the example's
    /// Connection API PATCH round-trip targets this port too. Override
    /// when 8010 is in use by something else on this host.
    #[arg(long, default_value_t = 8010)]
    http_port: u16,

    /// Skip the in-band Connection API (IS-05) PATCH round-trip step.
    /// Useful in environments where libnvnmos's HTTP server isn't
    /// reachable from the example (e.g. sandboxed CI), or for
    /// narrowing down a regression to the gRPC-only paths.
    #[arg(long, default_value_t = false)]
    skip_connection: bool,
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

    tracing::info!(
        http_port = args.http_port,
        "NodeConfig.http_port (also the Connection API PATCH target)",
    );

    let channel = connect_uds(&args.uds)
        .await
        .with_context(|| format!("connecting to nvnmosd at {}", args.uds.display()))?;
    let mut client = NvnmosDaemonClient::new(channel);

    // (1) First OpenSession: creates the Node.
    let a = open(
        &mut client,
        &args.node_seed,
        args.http_port,
        "first session (creates Node)",
    )
    .await?;
    anyhow::ensure!(
        a.created_node,
        "first OpenSession on a fresh seed should have created_node=true; got {}",
        a.created_node,
    );

    // (2) Second OpenSession on the same seed: attaches to the existing
    // Node. The rest of `node_config` (host_name, asset_tags, …) is
    // ignored by the daemon when the Node already exists; we still
    // send a NodeConfig because the wire requires `node_config.seed`
    // to identify which Node to attach to.
    let b = open(
        &mut client,
        &args.node_seed,
        args.http_port,
        "second session (refcount bump)",
    )
    .await?;
    anyhow::ensure!(
        !b.created_node,
        "second OpenSession on the same seed should have created_node=false; got {}",
        b.created_node,
    );

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
    close(
        &mut client,
        &a.session_handle,
        "first close (refcount to 1)",
    )
    .await?;

    // (4) Close the second session: refcount 1→0, Node destroyed.
    close(
        &mut client,
        &b.session_handle,
        "second close (Node destroyed)",
    )
    .await?;

    // -------- Persistent Node lifecycle --------
    let persistent_seed = format!("{}-persistent", args.node_seed);

    // (5) AddNode: create a persistent Node.
    let added = add_node(&mut client, &persistent_seed, args.http_port).await?;
    anyhow::ensure!(
        !added.device_id.is_empty(),
        "AddNode returned an empty device_id"
    );

    // (6) Two OpenSessions on the persistent seed: attach to it without
    // affecting its lifetime. Both must return the persistent Node's id
    // *and* report `created_node=false` (the persistent Node was
    // created by AddNode in step 5).
    let c = open(
        &mut client,
        &persistent_seed,
        args.http_port,
        "first session on persistent Node",
    )
    .await?;
    let d = open(
        &mut client,
        &persistent_seed,
        args.http_port,
        "second session on persistent Node",
    )
    .await?;
    anyhow::ensure!(
        c.node_id == added.node_id && d.node_id == added.node_id,
        "OpenSession on the persistent seed returned the wrong node_id: \
         added={} session1={} session2={}",
        added.node_id,
        c.node_id,
        d.node_id,
    );
    anyhow::ensure!(
        !c.created_node && !d.created_node,
        "OpenSession on a pre-existing persistent Node should have \
         created_node=false; got session1={}, session2={}",
        c.created_node,
        d.created_node,
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
        args.http_port,
        "resource phase session (creates Node)",
    )
    .await?;
    anyhow::ensure!(
        r.created_node,
        "resource-phase OpenSession on a fresh seed should have \
         created_node=true; got {}",
        r.created_node,
    );

    let iface_ip = match args.interface_ip.clone() {
        Some(ip) => ip,
        None => autodetect_interface_ip().context("autodetect interface IP")?,
    };
    tracing::info!(%iface_ip, "resource phase using interface IP");

    // (10) Subscribe to activations and spawn the auto-ack task. The
    // returned `activations_rx` lets the main flow observe and assert
    // on activations once they round-trip through the daemon.
    let (ack_task, mut activations_rx) =
        spawn_auto_ack_task(client.clone(), r.session_handle.clone()).await?;

    // (11) AddSender on the happy path. Cross-check the daemon's returned
    // sender/source/flow ids against the pure helpers. The sender and
    // receiver deliberately share the same `name` ("video-a") to exercise
    // the Node-scoped, side-disambiguated namespace: a Sender and a
    // Receiver may share a name, and the daemon distinguishes them
    // (in `by_name`, in `ActivationEvent.side`, and in
    // `nmos_connection_activate(side, ...)`).
    let sender_name = "video-a";
    let sender_resp = add_sender(
        &mut client,
        &r.session_handle,
        sender_name,
        &build_video_sdp(sender_name, true, &iface_ip),
    )
    .await?;
    let expected_source_id =
        nvnmos::make_source_id(&resource_seed, sender_name).context("make_source_id")?;
    let expected_flow_id =
        nvnmos::make_flow_id(&resource_seed, sender_name).context("make_flow_id")?;
    let expected_sender_id =
        nvnmos::make_sender_id(&resource_seed, sender_name).context("make_sender_id")?;
    anyhow::ensure!(
        sender_resp.source_id == expected_source_id,
        "AddSender returned source_id {} but make_source_id({:?}, {:?}) says {}",
        sender_resp.source_id,
        resource_seed,
        sender_name,
        expected_source_id,
    );
    anyhow::ensure!(
        sender_resp.flow_id == expected_flow_id,
        "AddSender returned flow_id {} but make_flow_id({:?}, {:?}) says {}",
        sender_resp.flow_id,
        resource_seed,
        sender_name,
        expected_flow_id,
    );
    anyhow::ensure!(
        sender_resp.sender_id == expected_sender_id,
        "AddSender returned sender_id {} but make_sender_id({:?}, {:?}) says {}",
        sender_resp.sender_id,
        resource_seed,
        sender_name,
        expected_sender_id,
    );

    // (12) AddReceiver with the same `name` as the sender above; the
    // daemon must accept this because Sender and Receiver are separate
    // namespaces. Cross-check the returned receiver id against
    // `make_receiver_id` (which uses a different UUID salt than the
    // sender variant, so the resulting ids must differ even though the
    // names match).
    let receiver_name = "video-a";
    let receiver_resp = add_receiver(
        &mut client,
        &r.session_handle,
        receiver_name,
        &build_video_sdp(receiver_name, false, &iface_ip),
    )
    .await?;
    let expected_receiver_id =
        nvnmos::make_receiver_id(&resource_seed, receiver_name).context("make_receiver_id")?;
    anyhow::ensure!(
        receiver_resp.receiver_id == expected_receiver_id,
        "AddReceiver returned receiver_id {} but make_receiver_id({:?}, {:?}) says {}",
        receiver_resp.receiver_id,
        resource_seed,
        receiver_name,
        expected_receiver_id,
    );
    anyhow::ensure!(
        sender_resp.sender_id != receiver_resp.receiver_id,
        "Sender and Receiver shared the same name {:?} but their NMOS \
         UUIDs collided: {}",
        sender_name,
        sender_resp.sender_id,
    );

    // (13) SyncResourceState on the sender with a fresh transport_file.
    // Bump the SDP session version (the `<sess-version>` token in `o=`)
    // so libnvnmos sees a real change. The daemon maps this onto
    // `nmos_connection_activate(Some(_))`.
    let updated_sender_sdp =
        build_video_sdp(sender_name, true, &iface_ip).replacen("o=- 0 0", "o=- 0 1", 1);
    sync_resource_state(
        &mut client,
        &r.session_handle,
        &sender_resp.resource_handle,
        Some(&updated_sender_sdp),
        "sender re-sync with updated SDP",
    )
    .await?;

    // (14) SyncResourceState on the sender with `transport_file = None`.
    // The daemon maps this onto `nmos_connection_activate(None)`.
    sync_resource_state(
        &mut client,
        &r.session_handle,
        &sender_resp.resource_handle,
        None,
        "sender deactivation",
    )
    .await?;

    // (15) IS-05 PATCH activate then deactivate against libnvnmos for
    // the sender. Unlike steps 13/14, which call
    // `nmos_connection_activate` directly (out-of-band), these PATCHes
    // exercise the *in-band* path: libnvnmos's activation callback
    // fires, the daemon turns it into an `ActivationEvent` on the
    // session's `SubscribeActivations` stream, the auto-ack task
    // acks, libnvnmos's PATCH returns 200. We assert every step
    // observed end-to-end via the relay channel.
    //
    // Skipped when `--skip-connection` is set, since the round-trip
    // needs libnvnmos's HTTP server to be reachable from this process.
    if !args.skip_connection {
        drive_connection_sender_activation(
            &mut activations_rx,
            &sender_resp.resource_handle,
            &iface_ip,
            args.http_port,
            &sender_resp.sender_id,
            true,
        )
        .await
        .context("Connection API PATCH activate round-trip")?;
        drive_connection_sender_activation(
            &mut activations_rx,
            &sender_resp.resource_handle,
            &iface_ip,
            args.http_port,
            &sender_resp.sender_id,
            false,
        )
        .await
        .context("Connection API PATCH deactivate round-trip")?;
    } else {
        tracing::info!(
            "--skip-connection set; skipping the in-band Connection API PATCH round-trip"
        );
    }

    // (16) Mismatch: claim name="claimed-id" but build the SDP with a
    // different x-nvnmos-name ("real-id"). The daemon detects this by
    // asking libnvnmos to look up the claimed name after the add and
    // returns INVALID_ARGUMENT when the lookup misses.
    let mismatch_sdp = build_video_sdp("real-id", true, &iface_ip);
    let mismatch_resp = client
        .add_sender(AddSenderRequest {
            session_handle: r.session_handle.clone(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: mismatch_sdp,
            name: "claimed-id".to_string(),
        })
        .await;
    match mismatch_resp {
        Err(status) if status.code() == Code::InvalidArgument => {
            tracing::info!(
                grpc_message = %status.message(),
                "AddSender correctly rejected name vs x-nvnmos-name mismatch"
            );
        }
        Err(status) => anyhow::bail!(
            "AddSender mismatch returned code={:?} (expected InvalidArgument): {}",
            status.code(),
            status.message(),
        ),
        Ok(_) => anyhow::bail!("AddSender mismatch unexpectedly succeeded"),
    }

    // (17) RemoveResource for the sender. The receiver must survive.
    remove_resource(
        &mut client,
        &r.session_handle,
        &sender_resp.resource_handle,
        "remove sender (receiver survives)",
    )
    .await?;

    // (18) Optional hold so an operator can curl an IS-05 PATCH at
    // libnvnmos and observe SubscribeActivations / AckActivation in
    // the daemon log. The receiver is still registered and the
    // activations stream is still attached, so any incoming IS-05
    // activation against the receiver will round-trip through the
    // auto-ack task. With --hold-secs 0 (default) this is a no-op.
    if args.hold_secs > 0 {
        tracing::info!(
            hold_secs = args.hold_secs,
            receiver_resource_id = %receiver_resp.receiver_id,
            "holding session open for manual IS-05 PATCH testing",
        );
        tokio::time::sleep(Duration::from_secs(args.hold_secs)).await;
    }

    // (19) CloseSession: drops the surviving receiver through libnvnmos,
    // tears down the (session-refcounted) Node, and (because the
    // daemon drops the subscription on close) ends the activations
    // stream so the auto-ack task exits cleanly.
    close(
        &mut client,
        &r.session_handle,
        "resource phase close (drops receiver, destroys Node)",
    )
    .await?;

    // Join the auto-ack task. It exits once the stream closes; give it
    // a brief grace period before complaining.
    match tokio::time::timeout(Duration::from_secs(5), ack_task).await {
        Ok(Ok(())) => tracing::info!("activation ack task joined cleanly"),
        Ok(Err(e)) => tracing::error!(error = %e, "activation ack task panicked"),
        Err(_) => tracing::warn!(
            "activation ack task did not exit within 5s of CloseSession; \
             abandoning",
        ),
    }

    // -------- Channel mapping lifecycle --------
    let cm_seed = format!("{}-channelmapping", args.node_seed);

    let cm = open(
        &mut client,
        &cm_seed,
        args.http_port,
        "channel mapping session (creates Node)",
    )
    .await?;
    anyhow::ensure!(
        cm.created_node,
        "channelmapping-phase OpenSession on a fresh seed should have \
         created_node=true; got {}",
        cm.created_node,
    );

    let channelmapping_name = "studio-map";
    let cm_input = ProtoChannelMappingInput {
        name: "Studio In".to_string(),
        description: String::new(),
        channel_labels: vec!["L".to_string(), "R".to_string()],
        parent_type: ProtoChannelMappingParentType::Receiver as i32,
        ..Default::default()
    };
    let cm_output = ProtoChannelMappingOutput {
        name: "Studio Out".to_string(),
        description: String::new(),
        channel_labels: vec!["L".to_string(), "R".to_string()],
        ..Default::default()
    };

    let pre_subscribe = client
        .add_channel_mapping(AddChannelMappingRequest {
            session_handle: cm.session_handle.clone(),
            name: channelmapping_name.to_string(),
            inputs: vec![cm_input.clone()],
            outputs: vec![cm_output.clone()],
        })
        .await;
    match pre_subscribe {
        Err(status) if status.code() == Code::FailedPrecondition => {
            tracing::info!(
                grpc_message = %status.message(),
                "AddChannelMapping correctly rejected before SubscribeChannelMappingActivations",
            );
        }
        Err(status) => anyhow::bail!(
            "AddChannelMapping before subscribe returned code={:?} \
             (expected FailedPrecondition): {}",
            status.code(),
            status.message(),
        ),
        Ok(_) => anyhow::bail!("AddChannelMapping before subscribe unexpectedly succeeded"),
    }

    let (cm_ack_task, _cm_activations_rx) =
        spawn_auto_ack_channelmapping_task(client.clone(), cm.session_handle.clone()).await?;

    let cm_resp = add_channel_mapping(
        &mut client,
        &cm.session_handle,
        channelmapping_name,
        &[cm_input],
        &[cm_output],
    )
    .await?;
    anyhow::ensure!(
        cm_resp.input_ids == ["input0"],
        "first channel mapping on Node input id defaulting: expected [input0], got {:?}",
        cm_resp.input_ids,
    );
    anyhow::ensure!(
        cm_resp.output_ids == ["output0"],
        "first channel mapping on Node output id defaulting: expected [output0], got {:?}",
        cm_resp.output_ids,
    );

    sync_channel_mapping_state(
        &mut client,
        &cm.session_handle,
        &cm_resp.channelmapping_handle,
        "output0",
        &[
            ActiveMapEntry {
                input_id: Some("input0".to_string()),
                input_channel: Some(0),
            },
            ActiveMapEntry {
                input_id: None,
                input_channel: None,
            },
        ],
        "route L→L (R unrouted)",
    )
    .await?;

    remove_channel_mapping(
        &mut client,
        &cm.session_handle,
        &cm_resp.channelmapping_handle,
        "remove studio-map channel mapping",
    )
    .await?;

    close(
        &mut client,
        &cm.session_handle,
        "channel mapping close (destroys Node)",
    )
    .await?;

    match tokio::time::timeout(Duration::from_secs(5), cm_ack_task).await {
        Ok(Ok(())) => tracing::info!("channelmapping ack task joined cleanly"),
        Ok(Err(e)) => tracing::error!(error = %e, "channelmapping ack task panicked"),
        Err(_) => tracing::warn!(
            "channelmapping ack task did not exit within 5s of CloseSession; abandoning",
        ),
    }

    tracing::info!("done");
    Ok(())
}

/// Open the per-session activations stream and spawn a background task
/// that auto-acks each incoming activation with `success = true`. Every
/// successfully-acked event is also relayed on the returned channel so
/// the main test flow can wait on activations and assert their content;
/// the relay is best-effort (`try_send`), so a slow consumer just
/// drops events rather than stalling the ack path. The task exits
/// when the daemon ends the stream (e.g. on `CloseSession`) or when
/// its peer errors.
async fn spawn_auto_ack_task(
    mut client: NvnmosDaemonClient<Channel>,
    session_handle: String,
) -> anyhow::Result<(
    tokio::task::JoinHandle<()>,
    tokio_mpsc::Receiver<ActivationEvent>,
)> {
    tracing::info!(session_handle, "SubscribeActivations");
    let mut stream = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session_handle.clone(),
        })
        .await
        .context("SubscribeActivations failed")?
        .into_inner();

    let (relay_tx, relay_rx) = tokio_mpsc::channel::<ActivationEvent>(16);

    let handle = tokio::spawn(async move {
        loop {
            match stream.message().await {
                Ok(Some(event)) => {
                    let activated = event.transport_file.is_some();
                    let side = ProtoSide::try_from(event.side).unwrap_or(ProtoSide::Unspecified);
                    tracing::info!(
                        session_handle,
                        resource_handle = %event.resource_handle,
                        activation_handle = %event.activation_handle,
                        side = ?side,
                        activated,
                        "received activation; auto-acking",
                    );
                    let ack = client
                        .ack_activation(AckActivationRequest {
                            session_handle: session_handle.clone(),
                            activation_handle: event.activation_handle.clone(),
                            success: true,
                            failure_reason: String::new(),
                        })
                        .await;
                    if let Err(status) = ack {
                        tracing::error!(
                            session_handle,
                            activation_handle = %event.activation_handle,
                            grpc_code = ?status.code(),
                            grpc_message = %status.message(),
                            "AckActivation failed",
                        );
                        continue;
                    }
                    if let Err(e) = relay_tx.try_send(event) {
                        tracing::debug!(
                            session_handle,
                            error = %e,
                            "relay channel full or closed; activation observation dropped",
                        );
                    }
                }
                Ok(None) => {
                    tracing::info!(session_handle, "activations stream ended");
                    break;
                }
                Err(status) => {
                    tracing::error!(
                        session_handle,
                        grpc_code = ?status.code(),
                        grpc_message = %status.message(),
                        "activations stream errored",
                    );
                    break;
                }
            }
        }
    });
    Ok((handle, relay_rx))
}

async fn spawn_auto_ack_channelmapping_task(
    mut client: NvnmosDaemonClient<Channel>,
    session_handle: String,
) -> anyhow::Result<(
    tokio::task::JoinHandle<()>,
    tokio_mpsc::Receiver<ChannelMappingActivationEvent>,
)> {
    tracing::info!(session_handle, "SubscribeChannelMappingActivations");
    let mut stream = client
        .subscribe_channel_mapping_activations(SubscribeChannelMappingActivationsRequest {
            session_handle: session_handle.clone(),
        })
        .await
        .context("SubscribeChannelMappingActivations failed")?
        .into_inner();

    let (relay_tx, relay_rx) = tokio_mpsc::channel::<ChannelMappingActivationEvent>(16);

    let handle = tokio::spawn(async move {
        loop {
            match stream.message().await {
                Ok(Some(event)) => {
                    tracing::info!(
                        session_handle,
                        channelmapping_handle = %event.channelmapping_handle,
                        activation_handle = %event.activation_handle,
                        output_id = %event.output_id,
                        "received channelmapping activation; auto-acking",
                    );
                    let ack = client
                        .ack_channel_mapping_activation(AckChannelMappingActivationRequest {
                            session_handle: session_handle.clone(),
                            activation_handle: event.activation_handle.clone(),
                            success: true,
                            failure_reason: String::new(),
                        })
                        .await;
                    if let Err(status) = ack {
                        tracing::error!(
                            session_handle,
                            grpc_code = ?status.code(),
                            grpc_message = %status.message(),
                            "AckChannelMappingActivation failed",
                        );
                        break;
                    }
                    let _ = relay_tx.try_send(event);
                }
                Ok(None) => {
                    tracing::info!(session_handle, "channelmapping activations stream ended");
                    break;
                }
                Err(status) => {
                    tracing::error!(
                        session_handle,
                        grpc_code = ?status.code(),
                        grpc_message = %status.message(),
                        "channelmapping activations stream errored",
                    );
                    break;
                }
            }
        }
    });
    Ok((handle, relay_rx))
}

async fn open(
    client: &mut NvnmosDaemonClient<Channel>,
    node_seed: &str,
    http_port: u16,
    label: &str,
) -> anyhow::Result<OpenSessionResponse> {
    tracing::info!(node_seed = %node_seed, http_port, "OpenSession ({label})");
    let resp = client
        .open_session(OpenSessionRequest {
            node_config: Some(default_node_config(node_seed, http_port)),
        })
        .await
        .with_context(|| format!("OpenSession ({label}) failed"))?
        .into_inner();
    tracing::info!(
        session_handle = %resp.session_handle,
        node_id = %resp.node_id,
        created_node = resp.created_node,
        "session open ({label})",
    );
    Ok(resp)
}

/// Build the default `NodeConfig` used by every `OpenSession` / `AddNode`
/// call in this example. The `seed` field is informational on the wire
/// — the daemon overrides it with the explicit `node_seed` — but we
/// still set it to the same value for consistency. `asset_tags` is
/// populated unconditionally so the IS-04 `/self` endpoint always shows
/// BCP-002-02 distinguishing info while exercising the daemon's
/// `AssetConfig` translation path. `instance_id` is the node seed
/// itself: BCP-002-02 calls for a per-instance serial-number-like
/// value, and the seed is already the per-Node identity that drives
/// every UUID we expose, so reusing it keeps the asset distinguishing
/// info aligned with the rest of the Node's identity (and gives each
/// of the three Nodes the example creates a distinct `instance_id`).
fn default_node_config(node_seed: &str, http_port: u16) -> NodeConfig {
    NodeConfig {
        seed: node_seed.to_string(),
        http_port: u32::from(http_port),
        asset_tags: Some(AssetConfig {
            manufacturer: "NVIDIA".to_string(),
            product: "nvnmosd-example".to_string(),
            instance_id: node_seed.to_string(),
            functions: vec!["Example".to_string()],
        }),
        ..Default::default()
    }
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
    http_port: u16,
) -> anyhow::Result<AddNodeResponse> {
    tracing::info!(node_seed = %node_seed, http_port, "AddNode (create persistent Node)");
    let resp = client
        .add_node(AddNodeRequest {
            node_config: Some(default_node_config(node_seed, http_port)),
        })
        .await
        .context("AddNode failed")?
        .into_inner();
    tracing::info!(
        node_id = %resp.node_id,
        device_id = %resp.device_id,
        "AddNode succeeded"
    );
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
    sender_name: &str,
    transport_file: &str,
) -> anyhow::Result<AddSenderResponse> {
    tracing::info!(session_handle, sender_name, "AddSender");
    let resp = client
        .add_sender(AddSenderRequest {
            session_handle: session_handle.to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: transport_file.to_string(),
            name: sender_name.to_string(),
        })
        .await
        .context("AddSender failed")?
        .into_inner();
    tracing::info!(
        resource_handle = %resp.resource_handle,
        source_id = %resp.source_id,
        flow_id = %resp.flow_id,
        sender_id = %resp.sender_id,
        "AddSender succeeded",
    );
    Ok(resp)
}

async fn add_receiver(
    client: &mut NvnmosDaemonClient<Channel>,
    session_handle: &str,
    receiver_name: &str,
    transport_file: &str,
) -> anyhow::Result<AddReceiverResponse> {
    tracing::info!(session_handle, receiver_name, "AddReceiver");
    let resp = client
        .add_receiver(AddReceiverRequest {
            session_handle: session_handle.to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: transport_file.to_string(),
            name: receiver_name.to_string(),
        })
        .await
        .context("AddReceiver failed")?
        .into_inner();
    tracing::info!(
        resource_handle = %resp.resource_handle,
        receiver_id = %resp.receiver_id,
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

async fn sync_resource_state(
    client: &mut NvnmosDaemonClient<Channel>,
    session_handle: &str,
    resource_handle: &str,
    transport_file: Option<&str>,
    label: &str,
) -> anyhow::Result<()> {
    let activated = transport_file.is_some();
    tracing::info!(
        session_handle,
        resource_handle,
        activated,
        "SyncResourceState ({label})",
    );
    client
        .sync_resource_state(SyncResourceStateRequest {
            session_handle: session_handle.to_string(),
            resource_handle: resource_handle.to_string(),
            transport_file: transport_file.map(str::to_string),
        })
        .await
        .with_context(|| format!("SyncResourceState ({label}) failed"))?;
    Ok(())
}

async fn add_channel_mapping(
    client: &mut NvnmosDaemonClient<Channel>,
    session_handle: &str,
    name: &str,
    inputs: &[ProtoChannelMappingInput],
    outputs: &[ProtoChannelMappingOutput],
) -> anyhow::Result<AddChannelMappingResponse> {
    tracing::info!(session_handle, name, "AddChannelMapping");
    let resp = client
        .add_channel_mapping(AddChannelMappingRequest {
            session_handle: session_handle.to_string(),
            name: name.to_string(),
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
        })
        .await
        .context("AddChannelMapping failed")?
        .into_inner();
    tracing::info!(
        channelmapping_handle = %resp.channelmapping_handle,
        input_ids = ?resp.input_ids,
        output_ids = ?resp.output_ids,
        "AddChannelMapping succeeded",
    );
    Ok(resp)
}

async fn remove_channel_mapping(
    client: &mut NvnmosDaemonClient<Channel>,
    session_handle: &str,
    channelmapping_handle: &str,
    label: &str,
) -> anyhow::Result<()> {
    tracing::info!(
        session_handle,
        channelmapping_handle,
        "RemoveChannelMapping ({label})",
    );
    client
        .remove_channel_mapping(RemoveChannelMappingRequest {
            session_handle: session_handle.to_string(),
            channelmapping_handle: channelmapping_handle.to_string(),
        })
        .await
        .with_context(|| format!("RemoveChannelMapping ({label}) failed"))?;
    Ok(())
}

async fn sync_channel_mapping_state(
    client: &mut NvnmosDaemonClient<Channel>,
    session_handle: &str,
    channelmapping_handle: &str,
    output_id: &str,
    active_map: &[ActiveMapEntry],
    label: &str,
) -> anyhow::Result<()> {
    tracing::info!(
        session_handle,
        channelmapping_handle,
        output_id,
        channels = active_map.len(),
        "SyncChannelMappingState ({label})",
    );
    client
        .sync_channel_mapping_state(SyncChannelMappingStateRequest {
            session_handle: session_handle.to_string(),
            channelmapping_handle: channelmapping_handle.to_string(),
            output_id: output_id.to_string(),
            active_map: active_map.to_vec(),
        })
        .await
        .with_context(|| format!("SyncChannelMappingState ({label}) failed"))?;
    Ok(())
}

/// Issue an IS-05 Connection API `PATCH` against libnvnmos's HTTP
/// server and return the parsed response status + body.
///
/// We hand-roll the HTTP request over `tokio::net::TcpStream` to avoid
/// dragging an HTTP client into the workspace just for one request.
/// The request always sets `Connection: close`, so the server closes
/// the socket after the response and we can drain the body with a
/// single `read_to_end`. The whole exchange is capped at 10s so a
/// stuck activation can't hang the test.
///
/// The IS-05 activation round-trip goes:
///
/// 1. We `PATCH /…/staged` with `master_enable` + `activation`.
/// 2. libnvnmos accepts the staged change and invokes the activation
///    callback.
/// 3. The callback hops into the daemon, which puts an
///    [`ActivationEvent`] on the session's `SubscribeActivations`
///    stream.
/// 4. The auto-ack task receives, sends back `AckActivation { success
///    = true }`, then relays the event on `activations_rx`.
/// 5. libnvnmos's callback returns successfully, the PATCH returns
///    200 OK.
///
/// libnvnmos blocks the PATCH until step 4 completes, so this call
/// only returns once the activation is fully applied (or until the
/// activation-ack timeout in the daemon NACKs the round-trip).
async fn connection_patch(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
) -> anyhow::Result<(u16, String)> {
    let addr = format!("{host}:{port}");
    let request = format!(
        "PATCH {path} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    );

    tokio::time::timeout(Duration::from_secs(10), async {
        let mut sock = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("TcpStream::connect({addr})"))?;
        sock.write_all(request.as_bytes())
            .await
            .context("writing HTTP request")?;
        let mut buf = Vec::with_capacity(4096);
        sock.read_to_end(&mut buf)
            .await
            .context("reading HTTP response")?;
        let resp = String::from_utf8_lossy(&buf).into_owned();

        let status_line = resp.lines().next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow::anyhow!("could not parse HTTP status line: {status_line:?}"))?;
        Ok((status, resp))
    })
    .await
    .context("IS-05 PATCH timed out after 10s")?
}

/// Drive one IS-05 sender activation/deactivation round-trip and
/// assert the corresponding `ActivationEvent` makes it back through
/// the daemon's `SubscribeActivations` stream. `expect_active`
/// controls both the staged `master_enable` value sent to libnvnmos
/// and the asserted shape of the event (`transport_file = Some` for
/// activations, `None` for deactivations).
async fn drive_connection_sender_activation(
    activations_rx: &mut tokio_mpsc::Receiver<ActivationEvent>,
    expected_resource_handle: &str,
    connection_host: &str,
    connection_port: u16,
    sender_resource_id: &str,
    expect_active: bool,
) -> anyhow::Result<()> {
    let body = format!(
        r#"{{"master_enable":{enable},"activation":{{"mode":"activate_immediate"}}}}"#,
        enable = expect_active,
    );
    let path = format!("/x-nmos/connection/v1.1/single/senders/{sender_resource_id}/staged");
    tracing::info!(
        connection_host,
        connection_port,
        sender_resource_id,
        expect_active,
        "IS-05 PATCH (sender {action})",
        action = if expect_active {
            "activate"
        } else {
            "deactivate"
        },
    );

    let (status, body_resp) = connection_patch(connection_host, connection_port, &path, &body)
        .await
        .with_context(|| format!("IS-05 PATCH {path}"))?;
    anyhow::ensure!(
        status == 200,
        "IS-05 PATCH returned HTTP {status}, expected 200; body:\n{body_resp}",
    );

    // libnvnmos blocks the PATCH until the activation has been
    // acknowledged, so by the time `connection_patch` returns the relay must
    // already have the event. A small timeout guards against the
    // ack-task being scheduled-out for an instant after the daemon
    // pushed onto the stream.
    let event = tokio::time::timeout(Duration::from_secs(2), activations_rx.recv())
        .await
        .context("timed out waiting for ActivationEvent on the relay channel")?
        .context("activations relay channel closed before an event arrived")?;
    anyhow::ensure!(
        event.resource_handle == expected_resource_handle,
        "ActivationEvent for unexpected resource_handle: got {:?}, expected {:?}",
        event.resource_handle,
        expected_resource_handle,
    );
    anyhow::ensure!(
        event.transport_file.is_some() == expect_active,
        "ActivationEvent transport_file presence mismatch: got is_some={}, expected {}",
        event.transport_file.is_some(),
        expect_active,
    );
    let event_side = ProtoSide::try_from(event.side).unwrap_or(ProtoSide::Unspecified);
    anyhow::ensure!(
        event_side == ProtoSide::Sender,
        "ActivationEvent for a sender PATCH had wrong side: got {:?}",
        event_side,
    );
    tracing::info!(
        resource_handle = %event.resource_handle,
        activation_handle = %event.activation_handle,
        side = ?event_side,
        expect_active,
        "IS-05 activation round-trip observed on activations stream",
    );
    Ok(())
}

/// Minimal ST 2110-20 SDP. Cloned from `rust/nvnmos/examples/node.rs`'s
/// `build_video_sdp` (same parameter set; same encoding parameters); the
/// example client is intentionally self-contained, so duplication is
/// preferable to a shared "test fixtures" crate.
fn build_video_sdp(name: &str, sender: bool, iface_ip: &str) -> String {
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
         s=nvnmosd-example video {direction} {name}\r\n\
         t=0 0\r\n\
         a=x-nvnmos-name:{name}\r\n\
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
