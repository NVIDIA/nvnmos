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

mod common;

use std::net::TcpListener;
use std::time::Duration;

use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AddSenderRequest, CloseSessionRequest, SubscribeActivationsRequest, Transport as ProtoTransport,
};
use tokio::sync::oneshot;
use tokio_stream::StreamExt;
use tonic::transport::Channel;

use common::{
    DaemonHarness, autodetect_iface_ip, connect, ephemeral_http_port,
    http_patch_activate_immediate, open_session_with_port,
};

/// Upper bound the concurrent RPC is allowed to take. On the pre-fix daemon the
/// RPC deadlocks and never returns, so any finite budget fails it. Post-fix the
/// RPC must complete: `CloseSession` returns promptly (it aborts the parked
/// activation), while `AddSender` waits — correctly — for libnvnmos's `model`
/// lock, which the parked activation holds until its `ACTIVATION_ACK_TIMEOUT`
/// (5 s) elapses. So the budget must exceed that ack timeout with margin.
const CONCURRENT_RPC_BUDGET: Duration = Duration::from_secs(15);

fn minimal_sender_sdp(name: &str, iface_ip: &str) -> String {
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
        // CloseSession (and test teardown) can drop the Node HTTP listener
        // under this PATCH; an empty response is expected, not a failure.
        let _ = http_patch_activate_immediate(host, http_port, &staged_path, None, None).await;
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
    let mut harness = DaemonHarness::spawn(&[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();
    let http_port = ephemeral_http_port();
    let session = open_session_with_port(&mut client, "lock-add", http_port).await;

    let stream = subscribe_activations(&mut client, &session).await;

    let s1 = client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "s1".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: minimal_sender_sdp("s1", &iface),
        })
        .await
        .expect("AddSender s1")
        .into_inner();

    let parker = start_parked_in_band_activation_on_stream(stream, http_port, &s1.sender_id).await;

    let mut add_client = client.clone();
    let add_session = session.clone();
    let add_iface = iface.clone();
    let add_result = tokio::time::timeout(CONCURRENT_RPC_BUDGET, async move {
        add_client
            .add_sender(AddSenderRequest {
                session_handle: add_session,
                name: "s2".to_string(),
                transport: ProtoTransport::Rtp as i32,
                transport_file: minimal_sender_sdp("s2", &add_iface),
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
    let add_rpc = add_result.expect("AddSender s2 join");
    if add_rpc.is_err() {
        harness.assert_running();
    }
    add_rpc.expect("AddSender s2 RPC");

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
    let mut harness = DaemonHarness::spawn(&[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();
    let http_port = ephemeral_http_port();
    let session = open_session_with_port(&mut client, "lock-close", http_port).await;

    let stream = subscribe_activations(&mut client, &session).await;

    let s1 = client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "s1".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: minimal_sender_sdp("s1", &iface),
        })
        .await
        .expect("AddSender s1")
        .into_inner();

    let parker = start_parked_in_band_activation_on_stream(stream, http_port, &s1.sender_id).await;

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
    let close_rpc = close_result.expect("CloseSession join");
    if close_rpc.is_err() {
        harness.assert_running();
    }
    close_rpc.expect("CloseSession RPC");

    assert!(
        os_port_free(http_port),
        "http port {http_port} must be bindable after CloseSession (LISTEN leaked?)"
    );
}
