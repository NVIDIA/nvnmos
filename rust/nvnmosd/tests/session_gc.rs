// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for implicit `CloseSession` (session GC).

mod common;

use std::time::Duration;

use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AddSenderRequest, CloseSessionRequest, NodeConfig, OpenSessionRequest, RemoveResourceRequest,
    SubscribeActivationsRequest, Transport as ProtoTransport,
};
use tonic::Code;
use tonic::transport::Channel;

use common::{DaemonHarness, autodetect_iface_ip, connect};

fn spawn_gc_daemon(http_port_min: u16, http_port_max: u16) -> DaemonHarness {
    DaemonHarness::spawn(
        http_port_min,
        http_port_max,
        &[
            ("NVNMOSD_SESSION_GC", "1"),
            ("NVNMOSD_SESSION_SUBSCRIBE_TIMEOUT_SEC", "5"),
            ("NVNMOSD_SESSION_RESUBSCRIBE_TIMEOUT_SEC", "2"),
        ],
    )
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
    assert_eq!(err.code(), expected, "unexpected gRPC status: {err}");
}

/// Case A — subscribe before add.
#[tokio::test]
async fn subscribe_required_before_add() {
    let mut harness = spawn_gc_daemon(18_140, 18_149);
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
    let mut harness = spawn_gc_daemon(18_150, 18_159);
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
    let mut harness = spawn_gc_daemon(18_160, 18_169);
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
    let mut harness = spawn_gc_daemon(18_170, 18_179);
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
