// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Allow-list of advertised Node APIs (HTTP listings and Device controls).
//! Catches nmos-cpp adding another API that defaults onto the Node, and the
//! experimental Settings opt-in.

mod common;

use nvnmos_rpc::v1::CloseSessionRequest;
use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use serde_json::Value;
use tonic::transport::Channel;

use common::{DaemonHarness, connect, ephemeral_http_port, http_get_json, open_session_with_port};

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
    extra_env: &[(&str, &str)],
) -> (DaemonHarness, NvnmosDaemonClient<Channel>, String, u16) {
    let mut harness = DaemonHarness::spawn(extra_env);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let http_port = ephemeral_http_port();
    let seed = format!(
        "advertised-apis-{}-{}",
        std::process::id(),
        extra_env
            .first()
            .map(|(k, v)| format!("{k}={v}"))
            .unwrap_or_else(|| "default".to_owned())
    );
    let session = open_session_with_port(&mut client, &seed, http_port).await;
    (harness, client, session, http_port)
}

#[tokio::test]
async fn advertised_apis_match_expected_surface() {
    let (harness, mut client, session, port) = open_empty_node(&[]).await;

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
        open_empty_node(&[("NVNMOS_EXPERIMENTAL_SETTINGS", "1")]).await;

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
    let (harness, mut client, session, port) =
        open_empty_node(&[("NVNMOSD_ANNOTATION_API", "0")]).await;

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
