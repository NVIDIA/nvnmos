// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! An MXL Sender's Flow has the `colorspace`, `transfer_characteristic` and
//! `component_range` of its MXL flow definition. IS-05 has no transport file
//! for MXL, so the Flow is the only place a Controller can find them. When the
//! flow definition omits `transfer_characteristic` or `component_range`, so
//! does the Flow.

mod common;

use nvnmos_rpc::v1::{
    AddSenderRequest, CloseSessionRequest, SubscribeActivationsRequest, Transport as ProtoTransport,
};

use common::{DaemonHarness, connect, http_get_json, open_session_with_port};

const PORTS: std::ops::RangeInclusive<u16> = 18_470..=18_479;

fn video_flow_def(name: &str, colorimetry: &serde_json::Value) -> String {
    let mut flow_def = serde_json::json!({
        "label": name,
        "description": "",
        "media_type": "video/v210",
        "grain_rate": { "numerator": 50, "denominator": 1 },
        "frame_width": 1920,
        "frame_height": 1080,
        "interlace_mode": "progressive",
        "components": [
            { "name": "Y", "width": 1920, "height": 1080, "bit_depth": 10 },
            { "name": "Cb", "width": 960, "height": 1080, "bit_depth": 10 },
            { "name": "Cr", "width": 960, "height": 1080, "bit_depth": 10 }
        ],
        "tags": {
            "urn:x-nvnmos:tag:name": [name]
        }
    });
    for (key, value) in colorimetry.as_object().expect("colorimetry is an object") {
        flow_def[key] = value.clone();
    }
    flow_def.to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mxl_video_colorimetry_comes_from_flow_def() {
    let mut harness = DaemonHarness::spawn(PORTS, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let (session, http_port) = open_session_with_port(&mut client, "mxl-colorimetry").await;
    let _activations = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeActivations");

    for (index, colorimetry) in [
        serde_json::json!({
            "colorspace": "BT709",
            "transfer_characteristic": "SDR",
            "component_range": "FULL"
        }),
        serde_json::json!({
            "colorspace": "BT2100",
            "transfer_characteristic": "PQ",
            "component_range": "NARROW"
        }),
        serde_json::json!({ "colorspace": "BT709" }),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("ms{index}");
        let sender = client
            .add_sender(AddSenderRequest {
                session_handle: session.clone(),
                name: name.clone(),
                transport: ProtoTransport::Mxl as i32,
                transport_file: video_flow_def(&name, &colorimetry),
            })
            .await
            .unwrap_or_else(|e| panic!("AddSender {colorimetry}: {e}"))
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
        for key in ["colorspace", "transfer_characteristic", "component_range"] {
            assert_eq!(flow.get(key), colorimetry.get(key), "{key}: {flow}");
        }
    }

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
    drop(harness);
}
