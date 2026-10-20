// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! libnvnmos reports only that a call failed, not why, so nvnmosd returns
//! `UNKNOWN` naming the libnvnmos call.

mod common;

use nvnmos_rpc::v1::{AddSenderRequest, SubscribeActivationsRequest, Transport as ProtoTransport};
use tonic::Code;

use common::{DaemonHarness, PortRange, connect, open_session_with_port};

const REJECTED_TRANSPORT_FILE_PORTS: PortRange = 18_360..=18_369;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_transport_file_is_unknown() {
    let mut harness = DaemonHarness::spawn(REJECTED_TRANSPORT_FILE_PORTS, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let (session, _) = open_session_with_port(&mut client, "libnvnmos-rejects-sdp").await;
    let _stream = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeActivations")
        .into_inner();

    let status = client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "sender".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: "not an SDP file\r\na=x-nvnmos-name:sender\r\n".to_string(),
        })
        .await
        .expect_err("libnvnmos should reject a transport file it cannot parse");
    assert_eq!(status.code(), Code::Unknown, "{status}");
    assert!(
        status.message().contains("add_nmos_sender_to_node_server"),
        "{status}"
    );
    harness.assert_running();
}
