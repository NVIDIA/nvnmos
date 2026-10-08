// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A client that NACKs an immediate activation makes the controller's
//! request fail with 500.

mod common;

use std::time::Duration;

use nvnmos_rpc::v1::{
    AckActivationRequest, AckChannelMappingActivationRequest, AddChannelMappingRequest,
    AddSenderRequest, ChannelMappingInput, ChannelMappingOutput, SubscribeActivationsRequest,
    SubscribeChannelMappingActivationsRequest, Transport as ProtoTransport,
};
use tokio_stream::StreamExt;

use common::{
    DaemonHarness, autodetect_iface_ip, connect, http_patch_activate_immediate, http_request,
    open_session_with_port,
};

const EVENT_BUDGET: Duration = Duration::from_secs(10);

fn minimal_sender_sdp(name: &str, iface_ip: &str) -> String {
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {iface_ip}\r\n\
         s=activation-nack\r\n\
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nacked_immediate_connection_activation_fails_the_patch() {
    let mut harness = DaemonHarness::spawn(18_080, 18_089, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();
    let (session, http_port) = open_session_with_port(&mut client, "activation-nack-is05").await;
    let mut stream = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeActivations")
        .into_inner();
    let sender = client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "video".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: minimal_sender_sdp("video", &iface),
        })
        .await
        .expect("AddSender")
        .into_inner();

    let staged_path = format!(
        "/x-nmos/connection/v1.1/single/senders/{}/staged",
        sender.sender_id
    );
    let patch = tokio::spawn(async move {
        http_patch_activate_immediate("127.0.0.1", http_port, &staged_path, None, None).await
    });
    let event = tokio::time::timeout(EVENT_BUDGET, stream.next())
        .await
        .expect("activation event timeout")
        .expect("activation stream ended")
        .expect("activation stream error");
    client
        .ack_activation(AckActivationRequest {
            session_handle: session.clone(),
            activation_handle: event.activation_handle,
            success: false,
            failure_reason: "test NACK".to_string(),
        })
        .await
        .expect("AckActivation");

    let error = patch
        .await
        .expect("PATCH task")
        .expect_err("a NACKed immediate activation should fail the PATCH");
    assert!(error.contains("HTTP 500"), "{error}");
    harness.assert_running();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nacked_immediate_channelmapping_activation_fails_the_post() {
    let mut harness = DaemonHarness::spawn(18_090, 18_099, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let (session, http_port) = open_session_with_port(&mut client, "activation-nack-is08").await;
    let mut stream = client
        .subscribe_channel_mapping_activations(SubscribeChannelMappingActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeChannelMappingActivations")
        .into_inner();
    client
        .add_channel_mapping(AddChannelMappingRequest {
            session_handle: session.clone(),
            name: "mapping".to_string(),
            inputs: vec![ChannelMappingInput {
                id: "input0".to_string(),
                channel_labels: vec!["L".to_string()],
                ..Default::default()
            }],
            outputs: vec![ChannelMappingOutput {
                id: "output0".to_string(),
                channel_labels: vec!["L".to_string()],
                ..Default::default()
            }],
        })
        .await
        .expect("AddChannelMapping");

    let post = tokio::spawn(async move {
        http_request(
            "POST",
            "127.0.0.1",
            http_port,
            "/x-nmos/channelmapping/v1.0/map/activations/",
            Some(
                r#"{"activation":{"mode":"activate_immediate"},"action":{"output0":{"0":{"input":"input0","channel_index":0}}}}"#,
            ),
        )
        .await
    });
    let event = tokio::time::timeout(EVENT_BUDGET, stream.next())
        .await
        .expect("channel-mapping activation event timeout")
        .expect("channel-mapping activation stream ended")
        .expect("channel-mapping activation stream error");
    client
        .ack_channel_mapping_activation(AckChannelMappingActivationRequest {
            session_handle: session.clone(),
            activation_handle: event.activation_handle,
            success: false,
            failure_reason: "test NACK".to_string(),
        })
        .await
        .expect("AckChannelMappingActivation");

    let (status, response) = post.await.expect("POST task").expect("POST");
    assert_eq!(status, 500, "{response}");
    harness.assert_running();
}
