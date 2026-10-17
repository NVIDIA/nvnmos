// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Default IS-08 Input and Output IDs are `inputN` and `outputN` for every
//! channel mapping, so a second channel mapping on a Node needs explicit IDs.

mod common;

use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AddChannelMappingRequest, AddChannelMappingResponse, ChannelMappingActivationEvent,
    ChannelMappingInput, ChannelMappingOutput, SubscribeChannelMappingActivationsRequest,
};
use tonic::transport::Channel;
use tonic::{Code, Status, Streaming};

use common::{DaemonHarness, PortRange, connect, http_get_json, open_session_with_port};

const SECOND_DEFAULTS_PORTS: PortRange = 18_330..=18_339;
const SECOND_EXPLICIT_PORTS: PortRange = 18_340..=18_349;

async fn subscribe(
    client: &mut NvnmosDaemonClient<Channel>,
    session: &str,
) -> Streaming<ChannelMappingActivationEvent> {
    client
        .subscribe_channel_mapping_activations(SubscribeChannelMappingActivationsRequest {
            session_handle: session.to_string(),
        })
        .await
        .expect("SubscribeChannelMappingActivations")
        .into_inner()
}

async fn add_mapping(
    client: &mut NvnmosDaemonClient<Channel>,
    session: &str,
    name: &str,
    input_id: &str,
    output_id: &str,
) -> Result<AddChannelMappingResponse, Status> {
    client
        .add_channel_mapping(AddChannelMappingRequest {
            session_handle: session.to_string(),
            name: name.to_string(),
            inputs: vec![ChannelMappingInput {
                id: input_id.to_string(),
                channel_labels: vec!["L".to_string(), "R".to_string()],
                ..Default::default()
            }],
            outputs: vec![ChannelMappingOutput {
                id: output_id.to_string(),
                channel_labels: vec!["L".to_string(), "R".to_string()],
                ..Default::default()
            }],
        })
        .await
        .map(|response| response.into_inner())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_channel_mapping_with_default_ids_is_rejected() {
    let mut harness = DaemonHarness::spawn(SECOND_DEFAULTS_PORTS, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let seed = "channelmapping-second-defaults";
    let (rx_session, _) = open_session_with_port(&mut client, seed).await;
    let (tx_session, _) = open_session_with_port(&mut client, seed).await;
    let _rx_stream = subscribe(&mut client, &rx_session).await;
    let _tx_stream = subscribe(&mut client, &tx_session).await;

    let first = add_mapping(&mut client, &rx_session, "rx", "", "")
        .await
        .expect("first AddChannelMapping");
    assert_eq!(first.input_ids, ["input0"]);
    assert_eq!(first.output_ids, ["output0"]);

    let status = add_mapping(&mut client, &tx_session, "tx", "", "")
        .await
        .expect_err("second channel mapping with default ids should be rejected");
    assert_eq!(status.code(), Code::AlreadyExists, "{status}");
    assert!(status.message().contains("input0"), "{status}");
    harness.assert_running();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_channel_mapping_with_explicit_ids_is_added() {
    let mut harness = DaemonHarness::spawn(SECOND_EXPLICIT_PORTS, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let seed = "channelmapping-second-explicit";
    let (rx_session, http_port) = open_session_with_port(&mut client, seed).await;
    let (tx_session, _) = open_session_with_port(&mut client, seed).await;
    let _rx_stream = subscribe(&mut client, &rx_session).await;
    let _tx_stream = subscribe(&mut client, &tx_session).await;

    add_mapping(&mut client, &rx_session, "rx", "", "")
        .await
        .expect("first AddChannelMapping");
    let second = add_mapping(&mut client, &tx_session, "tx", "tx_input", "tx_output")
        .await
        .expect("second AddChannelMapping");
    assert_eq!(second.input_ids, ["tx_input"]);
    assert_eq!(second.output_ids, ["tx_output"]);

    let mut inputs: Vec<String> = serde_json::from_value(
        http_get_json(http_port, "/x-nmos/channelmapping/v1.0/inputs/").await,
    )
    .expect("inputs list");
    inputs.sort();
    assert_eq!(inputs, ["input0/", "tx_input/"]);
    let mut outputs: Vec<String> = serde_json::from_value(
        http_get_json(http_port, "/x-nmos/channelmapping/v1.0/outputs/").await,
    )
    .expect("outputs list");
    outputs.sort();
    assert_eq!(outputs, ["output0/", "tx_output/"]);
    harness.assert_running();
}
