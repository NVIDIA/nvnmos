// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `SubscribeAnnotations` reports the stored annotation. `UpdateAnnotation`
//! updates it and the stream then carries that stored result.

mod common;

use std::time::Duration;

use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    ActivationEvent, AddReceiverRequest, AddSenderRequest, Annotation, AnnotationEvent,
    AnnotationTag, NodeConfig, OpenSessionRequest, RemoveResourceRequest, ResourceType,
    SubscribeActivationsRequest, SubscribeAnnotationsRequest, Transport as ProtoTransport, Update,
    UpdateAnnotationRequest,
};
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::{Code, Streaming};

use common::{DaemonHarness, PortRange, autodetect_iface_ip, connect, http_patch};

const STREAM_PORTS: PortRange = 18_370..=18_379;
const BUFFER_PORTS: PortRange = 18_380..=18_389;
const API_OFF_PORTS: PortRange = 18_390..=18_399;

const EVENT_BUDGET: Duration = Duration::from_secs(5);

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

fn annotation(label_update: Update, label: &str) -> Option<Annotation> {
    Some(Annotation {
        label_update: label_update as i32,
        label: label.to_string(),
        ..Annotation::default()
    })
}

fn assert_reset(annotation: &Annotation) {
    assert_eq!(annotation.label_update, Update::Reset as i32);
    assert_eq!(annotation.label, "");
    assert_eq!(annotation.description_update, Update::Reset as i32);
    assert_eq!(annotation.description, "");
    assert_eq!(annotation.tags_update, Update::Reset as i32);
    assert!(annotation.tags.is_empty());
}

fn tag<'a>(annotation: &'a Annotation, key: &str) -> Option<&'a AnnotationTag> {
    annotation.tags.iter().find(|tag| tag.key == key)
}

async fn next_event(stream: &mut Streaming<AnnotationEvent>) -> AnnotationEvent {
    timeout(EVENT_BUDGET, stream.next())
        .await
        .expect("annotation event timed out")
        .expect("annotation stream closed")
        .expect("annotation event")
}

async fn open_session(
    client: &mut NvnmosDaemonClient<Channel>,
    seed: &str,
) -> (String, u16, Streaming<ActivationEvent>) {
    let resp = client
        .open_session(OpenSessionRequest {
            node_config: Some(NodeConfig {
                seed: seed.to_string(),
                http_port: 0,
                host_addresses: vec!["127.0.0.1".to_string()],
                label: "node-default".to_string(),
                ..NodeConfig::default()
            }),
        })
        .await
        .expect("OpenSession")
        .into_inner();
    let http_port = u16::try_from(resp.http_port).expect("http port");
    let activations = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: resp.session_handle.clone(),
        })
        .await
        .expect("SubscribeActivations")
        .into_inner();
    (resp.session_handle, http_port, activations)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn annotation_stream_reports_the_stored_annotation() {
    let mut harness = DaemonHarness::spawn(STREAM_PORTS, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();
    let (session, http_port, _activations) = open_session(&mut client, "annotation-stream").await;

    let mut stream = client
        .subscribe_annotations(SubscribeAnnotationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeAnnotations")
        .into_inner();

    let node = next_event(&mut stream).await;
    assert_eq!(node.resource_type, ResourceType::Node as i32);
    assert!(node.resource_handle.is_empty());
    assert_reset(node.annotation.as_ref().expect("node annotation"));
    let device = next_event(&mut stream).await;
    assert_eq!(device.resource_type, ResourceType::Device as i32);
    assert_reset(device.annotation.as_ref().expect("device annotation"));

    let sender = client
        .add_sender(AddSenderRequest {
            session_handle: session.clone(),
            name: "cam".to_string(),
            transport: ProtoTransport::Rtp as i32,
            transport_file: minimal_sender_sdp("cam", &iface),
        })
        .await
        .expect("AddSender")
        .into_inner();
    for resource_type in [
        ResourceType::Source,
        ResourceType::Flow,
        ResourceType::Sender,
    ] {
        let event = next_event(&mut stream).await;
        assert_eq!(event.resource_type, resource_type as i32);
        assert_eq!(event.resource_handle, sender.resource_handle);
        assert_reset(event.annotation.as_ref().expect("sender annotation"));
    }

    let receiver = client
        .add_receiver(AddReceiverRequest {
            session_handle: session.clone(),
            name: "mon".to_string(),
            transport: ProtoTransport::Rtp as i32,
            // A receiver ignores source-filter and x-nvnmos-src-port, so
            // the minimal sender SDP is fine here too.
            transport_file: minimal_sender_sdp("mon", &iface),
        })
        .await
        .expect("AddReceiver")
        .into_inner();
    let receiver_event = next_event(&mut stream).await;
    assert_eq!(receiver_event.resource_type, ResourceType::Receiver as i32);
    assert_eq!(receiver_event.resource_handle, receiver.resource_handle);
    assert_reset(
        receiver_event
            .annotation
            .as_ref()
            .expect("receiver annotation"),
    );

    client
        .update_annotation(UpdateAnnotationRequest {
            session_handle: session.clone(),
            resource_type: ResourceType::Sender as i32,
            resource_handle: sender.resource_handle.clone(),
            annotation: annotation(Update::Set, "overlay"),
        })
        .await
        .expect("UpdateAnnotation");
    let updated = next_event(&mut stream).await;
    assert_eq!(updated.resource_type, ResourceType::Sender as i32);
    let updated_annotation = updated.annotation.as_ref().expect("updated annotation");
    assert_eq!(updated_annotation.label_update, Update::Unchanged as i32);
    assert_eq!(updated_annotation.label, "overlay");
    assert_eq!(updated_annotation.description_update, Update::Reset as i32);
    assert_eq!(updated_annotation.tags_update, Update::Reset as i32);

    client
        .update_annotation(UpdateAnnotationRequest {
            session_handle: session.clone(),
            resource_type: ResourceType::Sender as i32,
            resource_handle: sender.resource_handle.clone(),
            annotation: Some(Annotation {
                description_update: Update::Set as i32,
                description: String::new(),
                ..Annotation::default()
            }),
        })
        .await
        .expect("empty description");
    let empty_description = next_event(&mut stream).await;
    let empty_description = empty_description
        .annotation
        .as_ref()
        .expect("empty description");
    assert_eq!(empty_description.label_update, Update::Unchanged as i32);
    assert_eq!(empty_description.label, "overlay");
    assert_eq!(
        empty_description.description_update,
        Update::Unchanged as i32
    );
    assert_eq!(empty_description.description, "");
    assert_eq!(empty_description.tags_update, Update::Reset as i32);

    client
        .update_annotation(UpdateAnnotationRequest {
            session_handle: session.clone(),
            resource_type: ResourceType::Sender as i32,
            resource_handle: sender.resource_handle.clone(),
            annotation: Some(Annotation {
                tags_update: Update::Set as i32,
                tags: vec![
                    AnnotationTag {
                        key: "foo".to_string(),
                        values_update: Update::Set as i32,
                        values: vec!["a".to_string(), "b".to_string()],
                    },
                    AnnotationTag {
                        key: "bar".to_string(),
                        values_update: Update::Set as i32,
                        values: Vec::new(),
                    },
                ],
                ..Annotation::default()
            }),
        })
        .await
        .expect("set tags");
    let tagged = next_event(&mut stream).await;
    let tagged = tagged.annotation.as_ref().expect("tagged annotation");
    assert_eq!(tagged.label, "overlay");
    assert_eq!(tagged.description, "");
    assert_eq!(tagged.tags_update, Update::Unchanged as i32);
    let foo = tag(tagged, "foo").expect("foo");
    assert_eq!(foo.values_update, Update::Unchanged as i32);
    assert_eq!(foo.values, ["a".to_string(), "b".to_string()]);
    let bar = tag(tagged, "bar").expect("bar");
    assert_eq!(bar.values_update, Update::Unchanged as i32);
    assert!(bar.values.is_empty());
    assert!(tag(tagged, "urn:x-nvnmos:tag:name").is_none());

    client
        .update_annotation(UpdateAnnotationRequest {
            session_handle: session.clone(),
            resource_type: ResourceType::Sender as i32,
            resource_handle: sender.resource_handle.clone(),
            annotation: Some(Annotation {
                tags_update: Update::Set as i32,
                tags: vec![AnnotationTag {
                    key: "foo".to_string(),
                    values_update: Update::Reset as i32,
                    values: vec!["ignored".to_string()],
                }],
                ..Annotation::default()
            }),
        })
        .await
        .expect("clear foo");
    let cleared = next_event(&mut stream).await;
    let cleared = cleared.annotation.as_ref().expect("cleared foo");
    assert!(tag(cleared, "foo").is_none());
    let bar = tag(cleared, "bar").expect("bar");
    assert_eq!(bar.values_update, Update::Unchanged as i32);
    assert!(bar.values.is_empty());
    assert_eq!(cleared.label, "overlay");
    assert_eq!(cleared.description, "");

    client
        .update_annotation(UpdateAnnotationRequest {
            session_handle: session.clone(),
            resource_type: ResourceType::Sender as i32,
            resource_handle: sender.resource_handle.clone(),
            annotation: annotation(Update::Reset, "session-default"),
        })
        .await
        .expect("reset label");
    let reset_label = next_event(&mut stream).await;
    let reset_label = reset_label.annotation.as_ref().expect("reset label");
    assert_eq!(reset_label.label_update, Update::Reset as i32);
    assert_eq!(reset_label.label, "");
    assert_eq!(reset_label.description_update, Update::Unchanged as i32);
    assert_eq!(reset_label.description, "");
    assert!(tag(reset_label, "bar").is_some());

    client
        .update_annotation(UpdateAnnotationRequest {
            session_handle: session.clone(),
            resource_type: ResourceType::Sender as i32,
            resource_handle: sender.resource_handle.clone(),
            annotation: Some(Annotation {
                description_update: Update::Reset as i32,
                tags_update: Update::Reset as i32,
                ..Annotation::default()
            }),
        })
        .await
        .expect("clear annotation");
    let reset = next_event(&mut stream).await;
    assert_reset(reset.annotation.as_ref().expect("reset annotation"));

    let (other, _other_port, _other_activations) =
        open_session(&mut client, "annotation-stream").await;
    let mut other_stream = client
        .subscribe_annotations(SubscribeAnnotationsRequest {
            session_handle: other.clone(),
        })
        .await
        .expect("other SubscribeAnnotations")
        .into_inner();
    let other_node = next_event(&mut other_stream).await;
    assert_eq!(other_node.resource_type, ResourceType::Node as i32);
    let other_device = next_event(&mut other_stream).await;
    assert_eq!(other_device.resource_type, ResourceType::Device as i32);
    assert!(
        timeout(Duration::from_millis(200), other_stream.next())
            .await
            .is_err(),
        "a session does not see a sender it does not own"
    );

    let body = r#"{"label":"shared-node"}"#;
    let (status, response) = http_patch(
        "127.0.0.1",
        http_port,
        "/x-nmos/annotation/v1.0/node/self/",
        body,
    )
    .await
    .expect("node PATCH");
    assert!(
        (200..300).contains(&status),
        "node PATCH {status}: {response}"
    );
    for stream in [&mut stream, &mut other_stream] {
        let event = next_event(stream).await;
        assert_eq!(event.resource_type, ResourceType::Node as i32);
        let annotation = event.annotation.as_ref().expect("node annotation");
        assert_eq!(annotation.label_update, Update::Unchanged as i32);
        assert_eq!(annotation.label, "shared-node");
    }

    let sender_path = format!("/x-nmos/annotation/v1.0/node/senders/{}/", sender.sender_id);
    let (status, response) = http_patch(
        "127.0.0.1",
        http_port,
        &sender_path,
        r#"{"label":"owner-only"}"#,
    )
    .await
    .expect("sender PATCH");
    assert!(
        (200..300).contains(&status),
        "sender PATCH {status}: {response}"
    );
    let owner_event = next_event(&mut stream).await;
    assert_eq!(owner_event.resource_type, ResourceType::Sender as i32);
    assert_eq!(
        owner_event
            .annotation
            .as_ref()
            .expect("owner annotation")
            .label,
        "owner-only"
    );
    assert!(
        timeout(Duration::from_millis(200), other_stream.next())
            .await
            .is_err(),
        "the other session received the sender annotation"
    );

    client
        .remove_resource(RemoveResourceRequest {
            session_handle: session.clone(),
            resource_handle: sender.resource_handle.clone(),
        })
        .await
        .expect("RemoveResource");
    assert!(
        timeout(Duration::from_millis(200), stream.next())
            .await
            .is_err(),
        "removing a resource emitted an annotation event"
    );

    let read_only = client
        .update_annotation(UpdateAnnotationRequest {
            session_handle: session.clone(),
            resource_type: ResourceType::Receiver as i32,
            resource_handle: receiver.resource_handle.clone(),
            annotation: Some(Annotation {
                tags_update: Update::Set as i32,
                tags: vec![AnnotationTag {
                    key: "urn:x-nvnmos:tag:name".to_string(),
                    values_update: Update::Set as i32,
                    values: vec!["nope".to_string()],
                }],
                ..Annotation::default()
            }),
        })
        .await
        .expect_err("read-only tag");
    assert_eq!(read_only.code(), Code::InvalidArgument);

    let mismatched = client
        .update_annotation(UpdateAnnotationRequest {
            session_handle: session,
            resource_type: ResourceType::Source as i32,
            resource_handle: receiver.resource_handle,
            annotation: annotation(Update::Set, "nope"),
        })
        .await
        .expect_err("resource type");
    assert_eq!(mismatched.code(), Code::InvalidArgument);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_annotation_burst_is_delivered_while_the_subscriber_keeps_up() {
    let mut harness = DaemonHarness::spawn(BUFFER_PORTS, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let (session, http_port, _activations) = open_session(&mut client, "annotation-buffer").await;
    let mut stream = client
        .subscribe_annotations(SubscribeAnnotationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeAnnotations")
        .into_inner();
    // Node and Device are already queued. Reading them leaves the buffer empty.
    let _node = next_event(&mut stream).await;
    let _device = next_event(&mut stream).await;

    // Each PATCH returns only after the daemon has queued that annotation.
    // The connection takes it from the buffer before the next PATCH, so
    // seventeen updates do not fill the sixteen-slot buffer or close
    // the stream.
    for index in 1..=17 {
        let (status, response) = http_patch(
            "127.0.0.1",
            http_port,
            "/x-nmos/annotation/v1.0/node/self/",
            &format!(r#"{{"label":"n{index}"}}"#),
        )
        .await
        .expect("label PATCH");
        assert!(
            (200..300).contains(&status),
            "PATCH n{index} returned {status}: {response}"
        );
    }

    for index in 1..=17 {
        let event = next_event(&mut stream).await;
        assert_eq!(event.resource_type, ResourceType::Node as i32);
        assert_eq!(
            event.annotation.as_ref().expect("node annotation").label,
            format!("n{index}")
        );
    }
    assert!(
        timeout(Duration::from_millis(200), stream.next())
            .await
            .is_err(),
        "the burst closed the annotation stream"
    );

    drop(stream);
    let started = std::time::Instant::now();
    let mut resumed = loop {
        match client
            .subscribe_annotations(SubscribeAnnotationsRequest {
                session_handle: session.clone(),
            })
            .await
        {
            Ok(stream) => break stream.into_inner(),
            Err(status)
                if status.code() == Code::AlreadyExists && started.elapsed() < EVENT_BUDGET =>
            {
                tokio::task::yield_now().await;
            }
            Err(status) => panic!("resubscribe: {status}"),
        }
    };
    let node = next_event(&mut resumed).await;
    assert_eq!(node.annotation.as_ref().expect("resumed node").label, "n17");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn annotation_rpcs_fail_when_the_api_is_off() {
    let mut harness = DaemonHarness::spawn(API_OFF_PORTS, &[("NVNMOSD_ANNOTATION_API", "0")]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let (session, _port, _activations) = open_session(&mut client, "annotation-off").await;

    let subscribe = client
        .subscribe_annotations(SubscribeAnnotationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect_err("subscribe while off");
    assert_eq!(subscribe.code(), Code::FailedPrecondition);

    let update = client
        .update_annotation(UpdateAnnotationRequest {
            session_handle: session,
            resource_type: ResourceType::Node as i32,
            resource_handle: String::new(),
            annotation: annotation(Update::Set, "nope"),
        })
        .await
        .expect_err("update while off");
    assert_eq!(update.code(), Code::FailedPrecondition);
}
