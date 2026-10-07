// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Regression tests for stalls while an in-band IS-05 activation holds the
//! Node model lock; see `doc/designs/nvnmosd/lock-ordering.md`.
//!
//! The deadlock tests drive an HTTP PATCH of `/staged` so the libnvnmos
//! activation thread holds `model` and blocks on the client ack, then issue
//! a concurrent gRPC mutation on the same Node. They must pass after the
//! "no FFI under `state`" fix; they hang or time out on the pre-fix daemon.
//!
//! `single_worker_serves_ack_while_add_sender_waits_on_model` is the other
//! stall: `AddSender`'s libnvnmos call must not occupy the daemon's only
//! async worker, or `AckActivation` cannot run until the activation times out.

mod common;

use std::net::TcpListener;
use std::time::Duration;

use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AckActivationRequest, AddSenderRequest, CloseSessionRequest, SubscribeActivationsRequest,
    Transport as ProtoTransport,
};
use tokio::sync::oneshot;
use tokio_stream::StreamExt;
use tonic::transport::Channel;

use common::{
    DaemonHarness, PortRange, autodetect_iface_ip, connect, http_patch_activate_immediate,
    open_session_with_port,
};

const ACK_WORKER_PORTS: PortRange = 18_220..=18_229;
const ACK_CANCEL_PORTS: PortRange = 18_230..=18_239;

/// Upper bound the concurrent RPC is allowed to take. On the pre-fix daemon the
/// RPC deadlocks and never returns, so any finite budget fails it. Post-fix the
/// RPC must complete: `CloseSession` returns promptly (it aborts the parked
/// activation), while `AddSender` waits — correctly — for libnvnmos's `model`
/// lock, which the parked activation holds until its `ACTIVATION_ACK_TIMEOUT`
/// (5 s) elapses. So the budget must exceed that ack timeout with margin.
const CONCURRENT_RPC_BUDGET: Duration = Duration::from_secs(15);

/// Time to leave `AddSender` in flight before acking. An idle daemon accepts
/// the RPC and enters libnvnmos well inside this window. The activation
/// already holds `model`, so the call does not return during the wait.
const ADD_SENDER_BLOCKS_FOR: Duration = Duration::from_millis(300);

/// Shorter than `ACTIVATION_ACK_TIMEOUT` (5 s). If `AddSender` occupies the
/// only async worker, the ack cannot return inside this budget.
const SINGLE_WORKER_ACK_BUDGET: Duration = Duration::from_secs(1);

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
    let patch = tokio::spawn(async move {
        // CloseSession (and test teardown) can drop the Node HTTP listener
        // under this PATCH; an empty response is expected, not a failure,
        // once the activation event has been delivered.
        http_patch_activate_immediate(host, http_port, &staged_path, None, None).await
    });
    match tokio::time::timeout(Duration::from_secs(10), parked_rx).await {
        Ok(Ok(())) => parker,
        Ok(Err(_)) => panic!("activation parker dropped before signalling"),
        Err(elapsed) => {
            let patch_detail = if patch.is_finished() {
                match patch.await.expect("PATCH task") {
                    Ok(()) => "PATCH succeeded but no activation event arrived".to_string(),
                    Err(err) => format!("PATCH failed: {err}"),
                }
            } else {
                "PATCH had not finished".to_string()
            };
            panic!("timed out waiting for in-band activation event ({patch_detail}): {elapsed}");
        }
    }
}

/// While an in-band IS-05 activation is parked on the client ack, adding
/// another sender on the same Node must not wedge the daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_band_activation_does_not_deadlock_add_sender() {
    let mut harness = DaemonHarness::spawn(18_120..=18_129, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();
    let (session, http_port) = open_session_with_port(&mut client, "lock-add").await;

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
    let mut harness = DaemonHarness::spawn(18_130..=18_139, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();
    let (session, http_port) = open_session_with_port(&mut client, "lock-close").await;

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

/// One async worker. While an in-band activation holds `model` waiting for
/// the ack, `AddSender` blocks in libnvnmos. `AckActivation` must still be
/// served well inside the ack timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_worker_serves_ack_while_add_sender_waits_on_model() {
    let mut harness = DaemonHarness::spawn(ACK_WORKER_PORTS, &[("TOKIO_WORKER_THREADS", "1")]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();
    let (session, http_port) = open_session_with_port(&mut client, "ack-worker").await;

    let mut stream = subscribe_activations(&mut client, &session).await;

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

    let staged_path = staged_sender_path(&s1.sender_id);
    let patch = tokio::spawn(async move {
        http_patch_activate_immediate("127.0.0.1", http_port, &staged_path, None, None).await
    });
    let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("timed out waiting for in-band activation event")
        .expect("activation stream ended")
        .expect("activation stream error");

    let mut add_client = client.clone();
    let add_session = session.clone();
    let add_iface = iface.clone();
    let add_s2 = tokio::spawn(async move {
        add_client
            .add_sender(AddSenderRequest {
                session_handle: add_session,
                name: "s2".to_string(),
                transport: ProtoTransport::Rtp as i32,
                transport_file: minimal_sender_sdp("s2", &add_iface),
            })
            .await
    });

    tokio::time::sleep(ADD_SENDER_BLOCKS_FOR).await;
    assert!(
        !add_s2.is_finished(),
        "AddSender s2 returned before the ack; the activation was not holding the model lock"
    );

    let ack = tokio::time::timeout(
        SINGLE_WORKER_ACK_BUDGET,
        client.ack_activation(AckActivationRequest {
            session_handle: session.clone(),
            activation_handle: event.activation_handle,
            success: true,
            failure_reason: String::new(),
        }),
    )
    .await;
    assert!(
        ack.is_ok(),
        "AckActivation was not served within {SINGLE_WORKER_ACK_BUDGET:?} while AddSender \
         occupied the only async worker"
    );
    ack.expect("AckActivation join").expect("AckActivation");

    let add_rpc = tokio::time::timeout(Duration::from_secs(3), add_s2)
        .await
        .expect("AddSender s2 did not finish after the ack released the model lock")
        .expect("AddSender s2 task");
    add_rpc.expect("AddSender s2");

    // Keep the activation stream open until the ack has been applied.
    drop(stream);
    patch
        .await
        .expect("activation PATCH task")
        .expect("IS-05 activation PATCH");
    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
}

/// Dropping `AddSender` while it is blocked on `model` must not skip the
/// commit. A repeat add of that name is `already_exists`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_add_sender_still_records_the_name() {
    let mut harness = DaemonHarness::spawn(ACK_CANCEL_PORTS, &[("TOKIO_WORKER_THREADS", "1")]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();
    let (session, http_port) = open_session_with_port(&mut client, "ack-cancel").await;

    let mut stream = subscribe_activations(&mut client, &session).await;

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

    let staged_path = staged_sender_path(&s1.sender_id);
    let patch = tokio::spawn(async move {
        http_patch_activate_immediate("127.0.0.1", http_port, &staged_path, None, None).await
    });
    let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("timed out waiting for in-band activation event")
        .expect("activation stream ended")
        .expect("activation stream error");

    let mut add_client = client.clone();
    let add_session = session.clone();
    let add_iface = iface.clone();
    let add_s2 = tokio::spawn(async move {
        add_client
            .add_sender(AddSenderRequest {
                session_handle: add_session,
                name: "s2".to_string(),
                transport: ProtoTransport::Rtp as i32,
                transport_file: minimal_sender_sdp("s2", &add_iface),
            })
            .await
    });

    tokio::time::sleep(ADD_SENDER_BLOCKS_FOR).await;
    assert!(
        !add_s2.is_finished(),
        "AddSender s2 returned before the ack; the activation was not holding the model lock"
    );
    add_s2.abort();

    client
        .ack_activation(AckActivationRequest {
            session_handle: session.clone(),
            activation_handle: event.activation_handle,
            success: true,
            failure_reason: String::new(),
        })
        .await
        .expect("AckActivation");

    // The ack has released `model`. Wait for the detached commit to record
    // the name before the repeat add.
    tokio::time::sleep(ADD_SENDER_BLOCKS_FOR).await;

    let repeat = client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "s2".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: minimal_sender_sdp("s2", &iface),
        })
        .await
        .expect_err("repeat AddSender s2");
    assert_eq!(
        repeat.code(),
        tonic::Code::AlreadyExists,
        "repeat AddSender s2: {repeat}"
    );

    drop(stream);
    patch
        .await
        .expect("activation PATCH task")
        .expect("IS-05 activation PATCH");
    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
}
