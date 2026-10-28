// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `OpenSession` returns the Node's Device id, including when the call
//! attaches to a Node that already exists.

mod common;

use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{AddNodeRequest, NodeConfig, OpenSessionRequest, OpenSessionResponse};
use tonic::transport::Channel;

use common::{DaemonHarness, connect, http_get_json};

const PORTS: std::ops::RangeInclusive<u16> = 18_380..=18_389;

async fn open(client: &mut NvnmosDaemonClient<Channel>, seed: &str) -> OpenSessionResponse {
    client
        .open_session(OpenSessionRequest {
            node_config: Some(NodeConfig {
                seed: seed.to_owned(),
                http_port: 0,
                host_addresses: vec!["127.0.0.1".to_owned()],
                ..Default::default()
            }),
        })
        .await
        .unwrap_or_else(|e| panic!("OpenSession {seed}: {e}"))
        .into_inner()
}

fn device_ids(port: u16, path_body: &serde_json::Value) -> Vec<String> {
    path_body
        .as_array()
        .unwrap_or_else(|| panic!("devices on port {port}: {path_body}"))
        .iter()
        .map(|device| {
            device["id"]
                .as_str()
                .unwrap_or_else(|| panic!("device id: {device}"))
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn open_session_returns_the_device_id_on_create_and_attach() {
    let mut harness = DaemonHarness::spawn(PORTS, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;

    let created = open(&mut client, "device-id-session").await;
    assert!(created.created_node);
    let http_port = u16::try_from(created.http_port).expect("http port");
    let self_doc = http_get_json(http_port, "/x-nmos/node/v1.3/self").await;
    assert_eq!(self_doc["id"].as_str(), Some(created.node_id.as_str()));
    let devices = http_get_json(http_port, "/x-nmos/node/v1.3/devices/").await;
    assert_eq!(
        device_ids(http_port, &devices),
        vec![created.device_id.clone()]
    );
    assert_ne!(created.device_id, created.node_id);

    let attached = open(&mut client, "device-id-session").await;
    assert!(!attached.created_node);
    assert_eq!(attached.node_id, created.node_id);
    assert_eq!(attached.device_id, created.device_id);
    assert_eq!(attached.http_port, created.http_port);

    let added = client
        .add_node(AddNodeRequest {
            node_config: Some(NodeConfig {
                seed: "device-id-persistent".to_owned(),
                http_port: 0,
                host_addresses: vec!["127.0.0.1".to_owned()],
                ..Default::default()
            }),
        })
        .await
        .expect("AddNode")
        .into_inner();
    let persistent = open(&mut client, "device-id-persistent").await;
    assert!(!persistent.created_node);
    assert_eq!(persistent.node_id, added.node_id);
    assert_eq!(persistent.device_id, added.device_id);
    assert_ne!(persistent.device_id, created.device_id);

    harness.assert_running();
}
