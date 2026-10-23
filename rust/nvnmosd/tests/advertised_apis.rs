// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Allow-list of advertised Node APIs (HTTP listings and Device controls).
//! Catches nmos-cpp adding another API that defaults onto the Node, and the
//! experimental Settings opt-in.

mod common;

use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{CloseSessionRequest, NodeConfig, OpenSessionRequest};
use serde_json::Value;
use tonic::transport::Channel;

use common::{
    DaemonHarness, PortRange, autodetect_iface_ip, connect, http_get_json, open_session_with_port,
};

// 18220..=18239 belong to the activation-ack lock-ordering tests.
const ANNOTATION_API_UNMOUNTED_PORTS: PortRange = 18_240..=18_249;
// 18370..=18379 belong to an open-session unit test in src/state.rs.
const HOST_NAME_IS_ADDRESS_PORTS: PortRange = 18_380..=18_389;
const HOST_NAME_IS_INTERFACE_ADDRESS_PORTS: PortRange = 18_390..=18_399;

fn json_string_set(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("expected array: {value}"))
        .iter()
        .map(|v| {
            v.as_str()
                .unwrap_or_else(|| panic!("expected string: {v}"))
                .to_owned()
        })
        .collect()
}

fn control_types(devices: &Value) -> Vec<String> {
    let mut types = Vec::new();
    for device in devices.as_array().expect("devices array") {
        for control in device["controls"].as_array().expect("controls") {
            types.push(control["type"].as_str().expect("type").to_owned());
        }
    }
    types.sort();
    types.dedup();
    types
}

async fn open_empty_node(
    http_ports: PortRange,
    extra_env: &[(&str, &str)],
) -> (DaemonHarness, NvnmosDaemonClient<Channel>, String, u16) {
    let mut harness = DaemonHarness::spawn(http_ports, extra_env);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let seed = format!(
        "advertised-apis-{}-{}",
        std::process::id(),
        extra_env
            .first()
            .map(|(k, v)| format!("{k}={v}"))
            .unwrap_or_else(|| "default".to_owned())
    );
    let (session, http_port) = open_session_with_port(&mut client, &seed).await;
    (harness, client, session, http_port)
}

#[tokio::test]
async fn advertised_apis_match_expected_surface() {
    let (harness, mut client, session, port) = open_empty_node(18_100..=18_109, &[]).await;

    let root = json_string_set(&http_get_json(port, "/").await);
    assert_eq!(root, ["log/", "x-manifest/", "x-nmos/"], "unexpected APIs");

    let x_nmos = json_string_set(&http_get_json(port, "/x-nmos/").await);
    assert_eq!(
        x_nmos,
        ["annotation/", "channelmapping/", "connection/", "node/"],
        "unexpected APIs"
    );

    let services = http_get_json(port, "/x-nmos/node/v1.3/self").await["services"].clone();
    let services = services.as_array().expect("services array");
    assert!(!services.is_empty(), "expected Annotation service");
    for service in services {
        assert_eq!(
            service["type"].as_str(),
            Some("urn:x-nmos:service:annotation/v1.0"),
            "unexpected Node services"
        );
        let href = service["href"].as_str().expect("service href");
        assert!(
            href.contains(&format!(":{port}/x-nmos/annotation/v1.0")),
            "unexpected Annotation href: {href}"
        );
    }

    let types = control_types(&http_get_json(port, "/x-nmos/node/v1.3/devices").await);
    assert!(
        types
            .iter()
            .any(|t| t.starts_with("urn:x-nmos:control:sr-ctrl/")),
        "expected Connection controls, got {types:?}"
    );
    assert!(
        types
            .iter()
            .any(|t| t == "urn:x-nmos:control:manifest-base/v1.0"),
        "expected Manifest Base controls, got {types:?}"
    );
    assert!(
        types.iter().all(|t| {
            !t.starts_with("urn:x-nmos:control:configuration/")
                && !t.starts_with("urn:x-nmos:control:ncp/")
                && !t.starts_with("urn:x-nmos:control:cm-ctrl/")
        }),
        "unexpected Device controls: {types:?}"
    );

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
    drop(harness);
}

#[tokio::test]
async fn experimental_settings_env_enables_settings_api() {
    let (harness, mut client, session, port) =
        open_empty_node(18_110..=18_119, &[("NVNMOS_EXPERIMENTAL_SETTINGS", "1")]).await;

    let root = json_string_set(&http_get_json(port, "/").await);
    assert_eq!(root, ["log/", "settings/", "x-manifest/", "x-nmos/"]);

    let x_nmos = json_string_set(&http_get_json(port, "/x-nmos/").await);
    assert_eq!(
        x_nmos,
        ["annotation/", "channelmapping/", "connection/", "node/"]
    );

    let settings = http_get_json(port, "/settings/all/").await;
    assert!(settings.is_object(), "expected settings object: {settings}");
    assert_eq!(settings["http_port"].as_u64(), Some(u64::from(port)));
    assert_eq!(settings["configuration_port"].as_i64(), Some(-1));
    assert_eq!(settings["control_protocol_ws_port"].as_i64(), Some(-1));

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
    drop(harness);
}

#[tokio::test]
async fn annotation_api_env_leaves_annotation_unmounted() {
    let (harness, mut client, session, port) = open_empty_node(
        ANNOTATION_API_UNMOUNTED_PORTS,
        &[("NVNMOSD_ANNOTATION_API", "0")],
    )
    .await;

    let x_nmos = json_string_set(&http_get_json(port, "/x-nmos/").await);
    assert_eq!(
        x_nmos,
        ["channelmapping/", "connection/", "node/"],
        "unexpected APIs"
    );

    let services = http_get_json(port, "/x-nmos/node/v1.3/self").await["services"].clone();
    assert_eq!(services, Value::Array(vec![]), "unexpected Node services");

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
    drop(harness);
}

fn assert_listed_once(what: &str, mut items: Vec<String>) {
    assert!(!items.is_empty(), "expected {what}");
    items.sort();
    let mut unique = items.clone();
    unique.dedup();
    assert_eq!(items, unique, "{what} listed more than once");
}

async fn assert_hosts_listed_once(http_ports: PortRange, host_name: &str, host_addresses: &[&str]) {
    let mut harness = DaemonHarness::spawn(http_ports, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let resp = client
        .open_session(OpenSessionRequest {
            node_config: Some(NodeConfig {
                seed: format!(
                    "advertised-apis-{}-host-addresses-{}",
                    std::process::id(),
                    host_addresses.len()
                ),
                host_name: host_name.to_owned(),
                host_addresses: host_addresses.iter().map(|&a| a.to_owned()).collect(),
                ..Default::default()
            }),
        })
        .await
        .expect("OpenSession")
        .into_inner();
    let port = u16::try_from(resp.http_port).expect("allocated http_port");

    let endpoints = http_get_json(port, "/x-nmos/node/v1.3/self").await["api"]["endpoints"]
        .as_array()
        .expect("endpoints array")
        .iter()
        .map(Value::to_string)
        .collect();
    assert_listed_once("Node API endpoints", endpoints);

    let mut hrefs = Vec::new();
    for device in http_get_json(port, "/x-nmos/node/v1.3/devices")
        .await
        .as_array()
        .expect("devices array")
    {
        for control in device["controls"].as_array().expect("controls") {
            hrefs.push(control["href"].as_str().expect("href").to_owned());
        }
    }
    assert_listed_once("Device control hrefs", hrefs);

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: resp.session_handle,
        })
        .await;
    drop(harness);
}

#[tokio::test]
async fn host_name_that_is_a_host_address_is_listed_once() {
    assert_hosts_listed_once(HOST_NAME_IS_ADDRESS_PORTS, "127.0.0.1", &["127.0.0.1"]).await;
}

#[tokio::test]
async fn host_name_that_is_an_interface_address_is_listed_once() {
    // Empty host_addresses asks for every interface address, which excludes loopback.
    let host_name = autodetect_iface_ip();
    assert_hosts_listed_once(HOST_NAME_IS_INTERFACE_ADDRESS_PORTS, &host_name, &[]).await;
}
