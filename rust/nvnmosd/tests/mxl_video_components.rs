// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The MXL video media type determines the components. An MXL Sender's Flow
//! has the components of its media type, so a `video/v210a` Flow includes
//! the alpha component. An MXL Receiver's caps do not constrain
//! `color_sampling`. An MXL video Sender or Receiver with another media type
//! is rejected.

mod common;

use nvnmos_rpc::v1::{
    AddReceiverRequest, AddSenderRequest, CloseSessionRequest, SubscribeActivationsRequest,
    Transport as ProtoTransport,
};

use common::{DaemonHarness, connect, http_get_json, open_session_with_port};

const PORTS: std::ops::RangeInclusive<u16> = 18_440..=18_449;
const OTHER_MEDIA_TYPE_PORTS: std::ops::RangeInclusive<u16> = 18_450..=18_459;

fn components(media_type: &str) -> serde_json::Value {
    let mut components = serde_json::json!([
        { "name": "Y", "width": 1920, "height": 1080, "bit_depth": 10 },
        { "name": "Cb", "width": 960, "height": 1080, "bit_depth": 10 },
        { "name": "Cr", "width": 960, "height": 1080, "bit_depth": 10 }
    ]);
    if media_type == "video/v210a" {
        components
            .as_array_mut()
            .expect("components is an array")
            .push(
                serde_json::json!({ "name": "A", "width": 1920, "height": 1080, "bit_depth": 10 }),
            );
    }
    components
}

fn video_flow_def(name: &str, media_type: &str) -> String {
    serde_json::json!({
        "label": name,
        "description": "",
        "media_type": media_type,
        "grain_rate": { "numerator": 50, "denominator": 1 },
        "frame_width": 1920,
        "frame_height": 1080,
        "interlace_mode": "progressive",
        "colorspace": "BT709",
        "components": components(media_type),
        "tags": {
            "urn:x-nvnmos:tag:name": [name]
        }
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mxl_video_components_come_from_flow_def() {
    let mut harness = DaemonHarness::spawn(PORTS, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let (session, http_port) = open_session_with_port(&mut client, "mxl-components").await;
    let _activations = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeActivations");

    for (index, media_type) in ["video/v210", "video/v210a"].into_iter().enumerate() {
        let sender_name = format!("ms{index}");
        let sender = client
            .add_sender(AddSenderRequest {
                session_handle: session.clone(),
                name: sender_name.clone(),
                transport: ProtoTransport::Mxl as i32,
                transport_file: video_flow_def(&sender_name, media_type),
            })
            .await
            .unwrap_or_else(|e| panic!("AddSender {media_type}: {e}"))
            .into_inner();
        let sender = http_get_json(
            http_port,
            &format!("/x-nmos/node/v1.3/senders/{}", sender.sender_id),
        )
        .await;
        let flow_id = sender["flow_id"]
            .as_str()
            .unwrap_or_else(|| panic!("flow_id: {sender}"));
        let flow = http_get_json(http_port, &format!("/x-nmos/node/v1.3/flows/{flow_id}")).await;
        assert_eq!(flow["media_type"], media_type, "{flow}");
        assert_eq!(flow["components"], components(media_type), "{flow}");

        let receiver_name = format!("mr{index}");
        let receiver = client
            .add_receiver(AddReceiverRequest {
                session_handle: session.clone(),
                name: receiver_name.clone(),
                transport: ProtoTransport::Mxl as i32,
                transport_file: video_flow_def(&receiver_name, media_type),
            })
            .await
            .unwrap_or_else(|e| panic!("AddReceiver {media_type}: {e}"))
            .into_inner();
        let receiver = http_get_json(
            http_port,
            &format!("/x-nmos/node/v1.3/receivers/{}", receiver.receiver_id),
        )
        .await;
        let constraint_sets = receiver["caps"]["constraint_sets"]
            .as_array()
            .unwrap_or_else(|| panic!("constraint_sets: {receiver}"));
        assert_eq!(constraint_sets.len(), 1, "{receiver}");
        assert_eq!(
            constraint_sets[0]["urn:x-nmos:cap:format:media_type"]["enum"],
            serde_json::json!([media_type]),
            "{receiver}"
        );
        assert!(
            constraint_sets[0]
                .get("urn:x-nmos:cap:format:color_sampling")
                .is_none(),
            "{receiver}"
        );
    }

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
    drop(harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mxl_video_with_another_media_type_is_rejected() {
    let mut harness = DaemonHarness::spawn(OTHER_MEDIA_TYPE_PORTS, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let (session, _) = open_session_with_port(&mut client, "mxl-other-media-type").await;
    let _activations = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeActivations");

    let sender = client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "ms".to_string(),
            transport: ProtoTransport::Mxl as i32,
            transport_file: video_flow_def("ms", "video/raw"),
        })
        .await;
    assert!(sender.is_err(), "AddSender video/raw: {sender:?}");

    let receiver = client
        .add_receiver(AddReceiverRequest {
            session_handle: session.clone(),
            name: "mr".to_string(),
            transport: ProtoTransport::Mxl as i32,
            transport_file: video_flow_def("mr", "video/raw"),
        })
        .await;
    assert!(receiver.is_err(), "AddReceiver video/raw: {receiver:?}");

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
    drop(harness);
}
