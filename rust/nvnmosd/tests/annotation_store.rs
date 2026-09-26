// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Daemon annotation store: IS-13 PATCH, then the same seed and names after
//! restart, remove+add, and node teardown.

mod common;

use std::path::Path;

use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    ActivationEvent, AddSenderRequest, CloseSessionRequest, NodeConfig, OpenSessionRequest,
    RemoveResourceRequest, SubscribeActivationsRequest, Transport as ProtoTransport,
};
use tempfile::TempDir;
use tonic::Streaming;
use tonic::transport::Channel;

use common::{
    DaemonHarness, autodetect_iface_ip, connect, ephemeral_http_port, http_get_json, http_patch,
};

fn minimal_sender_sdp(name: &str, iface_ip: &str) -> String {
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {iface_ip}\r\n\
         s=session-default\r\n\
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

fn spawn(checkpoint: &Path, extra: &[(&str, &str)]) -> DaemonHarness {
    let mut env = vec![(
        "NVNMOSD_ANNOTATION_CHECKPOINT_FILE",
        checkpoint.to_str().unwrap(),
    )];
    env.extend_from_slice(extra);
    DaemonHarness::spawn(&env)
}

struct OpenedSession {
    handle: String,
    _activations: Streaming<ActivationEvent>,
}

async fn open_session(
    client: &mut NvnmosDaemonClient<Channel>,
    seed: &str,
    http_port: u16,
) -> OpenedSession {
    let resp = client
        .open_session(OpenSessionRequest {
            node_config: Some(NodeConfig {
                seed: seed.to_string(),
                http_port: u32::from(http_port),
                host_addresses: vec!["127.0.0.1".to_string()],
                label: "node-default".to_string(),
                ..NodeConfig::default()
            }),
        })
        .await
        .expect("OpenSession")
        .into_inner();
    let handle = resp.session_handle;
    let _activations = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: handle.clone(),
        })
        .await
        .expect("SubscribeActivations")
        .into_inner();
    OpenedSession {
        handle,
        _activations,
    }
}

struct AddedSender {
    resource_handle: String,
    source_id: String,
    flow_id: String,
    sender_id: String,
}

async fn add_sender(
    client: &mut NvnmosDaemonClient<Channel>,
    session: &str,
    name: &str,
    iface_ip: &str,
) -> AddedSender {
    let added = client
        .add_sender(AddSenderRequest {
            session_handle: session.to_string(),
            name: name.to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: minimal_sender_sdp(name, iface_ip),
        })
        .await
        .expect("AddSender")
        .into_inner();
    AddedSender {
        resource_handle: added.resource_handle,
        source_id: added.source_id,
        flow_id: added.flow_id,
        sender_id: added.sender_id,
    }
}

async fn device_id(port: u16) -> String {
    let devices = http_get_json(port, "/x-nmos/node/v1.3/devices").await;
    let devices = devices.as_array().expect("devices array");
    assert_eq!(devices.len(), 1, "one device per node: {devices:?}");
    devices[0]["id"].as_str().expect("device id").to_string()
}

async fn patch_annotation(port: u16, path: &str, body: serde_json::Value) {
    let body = body.to_string();
    let (status, response) = http_patch("127.0.0.1", port, path, &body)
        .await
        .unwrap_or_else(|error| panic!("PATCH {path}: {error}"));
    assert!(
        (200..300).contains(&status),
        "PATCH {path} {body} returned {status}: {response}"
    );
}

async fn label(port: u16, collection: &str, id: &str) -> String {
    let path = if collection.is_empty() {
        "/x-nmos/node/v1.3/self".to_string()
    } else {
        format!("/x-nmos/node/v1.3/{collection}/{id}")
    };
    http_get_json(port, &path).await["label"]
        .as_str()
        .unwrap_or_else(|| panic!("label missing in {path}"))
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn annotations_survive_restart_remove_and_node_teardown() {
    let dir = TempDir::new().expect("checkpoint dir");
    let checkpoint = dir.path().join("annotations.json");
    let iface = autodetect_iface_ip();

    let mut harness = spawn(&checkpoint, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let port_a = ephemeral_http_port();
    let session_a = open_session(&mut client, "seed-a", port_a).await;
    let added = add_sender(&mut client, &session_a.handle, "video", &iface).await;
    assert_eq!(
        label(port_a, "senders", &added.sender_id).await,
        "session-default"
    );
    let video_flow = label(port_a, "flows", &added.flow_id).await;
    let device = device_id(port_a).await;
    let device_label = label(port_a, "devices", &device).await;

    let only = add_sender(&mut client, &session_a.handle, "source-only", &iface).await;
    let only_sender = label(port_a, "senders", &only.sender_id).await;
    let only_flow = label(port_a, "flows", &only.flow_id).await;
    patch_annotation(
        port_a,
        &format!("/x-nmos/annotation/v1.0/node/sources/{}/", only.source_id),
        serde_json::json!({ "label": "source-only-patched" }),
    )
    .await;

    patch_annotation(
        port_a,
        &format!("/x-nmos/annotation/v1.0/node/senders/{}/", added.sender_id),
        serde_json::json!({ "label": "sender-patched" }),
    )
    .await;
    patch_annotation(
        port_a,
        &format!("/x-nmos/annotation/v1.0/node/sources/{}/", added.source_id),
        serde_json::json!({ "label": "source-patched" }),
    )
    .await;
    patch_annotation(
        port_a,
        "/x-nmos/annotation/v1.0/node/self/",
        serde_json::json!({ "label": "node-patched" }),
    )
    .await;

    client
        .remove_resource(RemoveResourceRequest {
            session_handle: session_a.handle.clone(),
            resource_handle: added.resource_handle,
        })
        .await
        .expect("RemoveResource");
    let added = add_sender(&mut client, &session_a.handle, "video", &iface).await;
    assert_eq!(
        label(port_a, "senders", &added.sender_id).await,
        "sender-patched"
    );
    assert_eq!(
        label(port_a, "sources", &added.source_id).await,
        "source-patched"
    );

    let handle = session_a.handle.clone();
    drop(session_a);
    client
        .close_session(CloseSessionRequest {
            session_handle: handle,
        })
        .await
        .expect("CloseSession");
    let session_a = open_session(&mut client, "seed-a", port_a).await;
    assert_eq!(label(port_a, "", "").await, "node-patched");
    let added = add_sender(&mut client, &session_a.handle, "video", &iface).await;
    assert_eq!(
        label(port_a, "senders", &added.sender_id).await,
        "sender-patched"
    );

    let port_b = ephemeral_http_port();
    let session_b = open_session(&mut client, "seed-b", port_b).await;
    let other = add_sender(&mut client, &session_b.handle, "video", &iface).await;
    assert_eq!(
        label(port_b, "senders", &other.sender_id).await,
        "session-default"
    );
    assert_eq!(label(port_b, "", "").await, "node-default");
    drop(session_a);
    drop(session_b);
    drop(client);
    harness.terminate();

    let mut harness = spawn(&checkpoint, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let port_a = ephemeral_http_port();
    let session_a = open_session(&mut client, "seed-a", port_a).await;
    assert_eq!(label(port_a, "", "").await, "node-patched");
    let added = add_sender(&mut client, &session_a.handle, "video", &iface).await;
    assert_eq!(
        label(port_a, "senders", &added.sender_id).await,
        "sender-patched"
    );
    assert_eq!(
        label(port_a, "sources", &added.source_id).await,
        "source-patched"
    );
    assert_eq!(label(port_a, "flows", &added.flow_id).await, video_flow);
    assert_eq!(label(port_a, "devices", &device).await, device_label);
    let only = add_sender(&mut client, &session_a.handle, "source-only", &iface).await;
    assert_eq!(
        label(port_a, "sources", &only.source_id).await,
        "source-only-patched"
    );
    assert_eq!(label(port_a, "senders", &only.sender_id).await, only_sender);
    assert_eq!(label(port_a, "flows", &only.flow_id).await, only_flow);
    let port_b = ephemeral_http_port();
    let session_b = open_session(&mut client, "seed-b", port_b).await;
    let other = add_sender(&mut client, &session_b.handle, "video", &iface).await;
    assert_eq!(
        label(port_b, "senders", &other.sender_id).await,
        "session-default"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn label_reset_is_not_restored_after_restart() {
    let dir = TempDir::new().expect("checkpoint dir");
    let checkpoint = dir.path().join("annotations.json");
    let iface = autodetect_iface_ip();
    let mut harness = spawn(&checkpoint, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let port = ephemeral_http_port();
    let session = open_session(&mut client, "seed-reset", port).await;
    let added = add_sender(&mut client, &session.handle, "video", &iface).await;
    let sender_id = added.sender_id;
    assert_eq!(label(port, "senders", &sender_id).await, "session-default");
    patch_annotation(
        port,
        &format!("/x-nmos/annotation/v1.0/node/senders/{sender_id}/"),
        serde_json::json!({ "label": "patched" }),
    )
    .await;
    patch_annotation(
        port,
        &format!("/x-nmos/annotation/v1.0/node/senders/{sender_id}/"),
        serde_json::json!({ "label": null }),
    )
    .await;
    assert_eq!(label(port, "senders", &sender_id).await, "session-default");
    drop(session);
    drop(client);
    harness.terminate();

    let mut harness = spawn(&checkpoint, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let port = ephemeral_http_port();
    let session = open_session(&mut client, "seed-reset", port).await;
    let added = add_sender(&mut client, &session.handle, "video", &iface).await;
    let sender_id = added.sender_id;
    assert_eq!(label(port, "senders", &sender_id).await, "session-default");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_checkpoint_exits() {
    let dir = TempDir::new().expect("checkpoint dir");
    let checkpoint = dir.path().join("annotations.json");
    std::fs::write(&checkpoint, b"not-json").unwrap();
    let mut harness = spawn(&checkpoint, &[]);
    let (status, stderr) = harness.wait_for_exit();
    assert!(
        !status.success(),
        "corrupt checkpoint should fail startup: {stderr}"
    );
    assert!(stderr.contains("invalid annotation checkpoint"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_annotation_api_does_not_read_the_checkpoint() {
    let dir = TempDir::new().expect("checkpoint dir");
    let checkpoint = dir.path().join("annotations.json");
    std::fs::write(&checkpoint, b"not-json").unwrap();
    let mut harness = spawn(&checkpoint, &[("NVNMOSD_ANNOTATION_API", "0")]);
    harness.ready().await;
    assert_eq!(std::fs::read(&checkpoint).unwrap(), b"not-json");
    harness.terminate();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unwritable_checkpoint_exits() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("checkpoint dir");
    let checkpoint = dir.path().join("annotations.json");
    let mut perms = std::fs::metadata(dir.path())
        .expect("metadata")
        .permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(dir.path(), perms).expect("readonly dir");
    let mut harness = spawn(&checkpoint, &[]);
    let (status, stderr) = harness.wait_for_exit();
    let mut perms = std::fs::metadata(dir.path())
        .expect("metadata")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(dir.path(), perms).expect("restore dir mode");
    assert!(
        !status.success(),
        "unwritable checkpoint directory should fail startup: {stderr}"
    );
    assert!(
        stderr.contains("failed to create"),
        "startup should report the replacement write: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn limit_refuses_a_new_entry_and_keeps_the_stored_one() {
    let dir = TempDir::new().expect("checkpoint dir");
    let checkpoint = dir.path().join("annotations.json");
    let iface = autodetect_iface_ip();
    let mut harness = spawn(&checkpoint, &[("NVNMOSD_ANNOTATION_ENTRY_LIMIT", "1")]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let port = ephemeral_http_port();
    let session = open_session(&mut client, "seed-limit", port).await;
    let kept = add_sender(&mut client, &session.handle, "kept", &iface).await;
    let dropped = add_sender(&mut client, &session.handle, "dropped", &iface).await;
    let kept_id = kept.sender_id;
    let dropped_id = dropped.sender_id;
    patch_annotation(
        port,
        &format!("/x-nmos/annotation/v1.0/node/senders/{kept_id}/"),
        serde_json::json!({ "label": "kept" }),
    )
    .await;
    patch_annotation(
        port,
        &format!("/x-nmos/annotation/v1.0/node/senders/{dropped_id}/"),
        serde_json::json!({ "label": "dropped" }),
    )
    .await;
    drop(session);
    drop(client);
    harness.terminate();
    let file = std::fs::read_to_string(&checkpoint).expect("checkpoint");
    assert!(file.contains("\"name\":\"kept\""), "{file}");
    assert!(!file.contains("dropped"), "{file}");

    let mut harness = spawn(&checkpoint, &[("NVNMOSD_ANNOTATION_ENTRY_LIMIT", "1")]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let port = ephemeral_http_port();
    let session = open_session(&mut client, "seed-limit", port).await;
    let kept = add_sender(&mut client, &session.handle, "kept", &iface).await;
    let dropped = add_sender(&mut client, &session.handle, "dropped", &iface).await;
    let kept_id = kept.sender_id;
    let dropped_id = dropped.sender_id;
    assert_eq!(label(port, "senders", &kept_id).await, "kept");
    assert_eq!(label(port, "senders", &dropped_id).await, "session-default");
}
