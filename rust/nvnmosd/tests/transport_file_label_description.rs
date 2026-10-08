// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Label/description on GET `/transportfile` (RTP Senders) and the
//! activation callback:
//! - RTP Receiver with a staged SDP (`transport_file.data`): pass through that
//!   SDP's `s=`/`i=`.
//! - Otherwise (RTP Sender, RTP Receiver with no staged SDP, MXL): IS-04
//!   label/description. RTP writes empty label as `s= ` and omits empty `i=`;
//!   MXL writes both JSON strings (description may be `""`).

mod common;

use std::time::Duration;

use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AckActivationRequest, ActivationEvent, AddReceiverRequest, AddSenderRequest,
    CloseSessionRequest, SubscribeActivationsRequest, Transport as ProtoTransport,
};
use tokio_stream::StreamExt;
use tonic::Streaming;
use tonic::transport::Channel;

use common::{
    DaemonHarness, autodetect_iface_ip, connect, http_get, http_get_json,
    http_patch_activate_immediate, open_session_with_port,
};

/// Video SDP.
///
/// * `name` - `Some` means a configuring file (with `x-nvnmos-name` and
///   `x-nvnmos-iface-ip`, and when `sender` also `x-nvnmos-src-port`).
/// * `session_info` - optional `i=` line.
/// * `sender` - also adds `source-filter`.
fn video_sdp(
    name: Option<&str>,
    iface_ip: &str,
    session_name: &str,
    session_info: Option<&str>,
    rtp_port: u16,
    sender: bool,
) -> String {
    let information = session_info.map_or(String::new(), |info| format!("i={info}\r\n"));
    let nvnmos_name = name.map_or(String::new(), |name| format!("a=x-nvnmos-name:{name}\r\n"));
    let source_filter = if sender {
        format!("a=source-filter: incl IN IP4 233.252.0.0 {iface_ip}\r\n")
    } else {
        String::new()
    };
    let nvnmos_iface = name.map_or(String::new(), |_| {
        format!("a=x-nvnmos-iface-ip:{iface_ip}\r\n")
    });
    let src_port = if name.is_some() && sender {
        format!("a=x-nvnmos-src-port:{rtp_port}\r\n")
    } else {
        String::new()
    };
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {iface_ip}\r\n\
         s={session_name}\r\n\
         {information}\
         t=0 0\r\n\
         {nvnmos_name}\
         m=video {rtp_port} RTP/AVP 96\r\n\
         c=IN IP4 233.252.0.0/64\r\n\
         {source_filter}\
         {nvnmos_iface}\
         a=rtpmap:96 raw/90000\r\n\
         a=fmtp:96 sampling=YCbCr-4:2:2; width=1920; height=1080; \
         exactframerate=50; depth=10; TCS=SDR; colorimetry=BT709; \
         PM=2110GPM; SSN=ST2110-20:2017; TP=2110TPN;\r\n\
         a=mediaclk:direct=0\r\n\
         {src_port}\
         a=ts-refclk:localmac=CA-FE-01-CA-FE-02\r\n"
    )
}

fn configuring_video_sdp(
    name: &str,
    iface_ip: &str,
    session_name: &str,
    session_info: Option<&str>,
    rtp_port: u16,
    sender: bool,
) -> String {
    video_sdp(
        Some(name),
        iface_ip,
        session_name,
        session_info,
        rtp_port,
        sender,
    )
}

fn staged_video_sdp(
    iface_ip: &str,
    session_name: &str,
    session_info: Option<&str>,
    rtp_port: u16,
) -> String {
    video_sdp(None, iface_ip, session_name, session_info, rtp_port, true)
}

fn video_flow_def(name: &str, label: &str, description: &str) -> String {
    serde_json::json!({
        "label": label,
        "description": description,
        "media_type": "video/v210",
        "grain_rate": { "numerator": 50, "denominator": 1 },
        "frame_width": 1920,
        "frame_height": 1080,
        "interlace_mode": "progressive",
        "colorspace": "BT709",
        "components": [
            { "name": "Y", "width": 1920, "height": 1080, "bit_depth": 10 },
            { "name": "Cb", "width": 960, "height": 1080, "bit_depth": 10 },
            { "name": "Cr", "width": 960, "height": 1080, "bit_depth": 10 }
        ],
        "tags": {
            "urn:x-nvnmos:tag:name": [name],
            "urn:x-nvnmos:tag:caps": [""]
        }
    })
    .to_string()
}

/// Check the session-level `s=` and `i=` of a generated SDP. `s=` is required
/// and must not be empty; `i=` is optional and must be absent when there is no
/// session information.
fn assert_sdp_session_fields(
    sdp: &str,
    expect_session_name: &str,
    expect_session_info: Option<&str>,
    what: &str,
) {
    let session_part = sdp.split("\r\nm=").next().unwrap_or(sdp);
    let lines = |key: &str| -> Vec<&str> {
        session_part
            .lines()
            .filter_map(|line| line.strip_prefix(key))
            .collect()
    };
    assert_eq!(
        lines("s="),
        vec![expect_session_name],
        "{what} session name:\n{sdp}"
    );
    assert_eq!(
        lines("i="),
        expect_session_info.into_iter().collect::<Vec<_>>(),
        "{what} session information:\n{sdp}"
    );
}

/// Map IS-04 label/description to the `s=` and `i=` they should produce
/// (empty label → `s= `; empty description omits `i=`).
fn expected_sdp_session_fields(label: &str, description: &str) -> (String, Option<String>) {
    let session_name = if label.is_empty() {
        " ".to_string()
    } else {
        label.to_string()
    };
    let session_info = if description.is_empty() {
        None
    } else {
        Some(description.to_string())
    };
    (session_name, session_info)
}

async fn http_get_label_description(
    http_port: u16,
    resource_type: &str,
    id: &str,
) -> (String, String) {
    let resource = http_get_json(
        http_port,
        &format!("/x-nmos/node/v1.3/{resource_type}/{id}"),
    )
    .await;
    (
        resource["label"]
            .as_str()
            .unwrap_or_else(|| panic!("label in {resource_type}/{id}: {resource}"))
            .to_string(),
        resource["description"]
            .as_str()
            .unwrap_or_else(|| panic!("description in {resource_type}/{id}: {resource}"))
            .to_string(),
    )
}

fn http_body(resp: &str) -> &str {
    resp.split("\r\n\r\n").nth(1).unwrap_or(resp)
}

/// Activate via IS-05 and return the in-band activation event. The PATCH is
/// only answered once the activation is acked, so it runs on its own task.
async fn activate(
    stream: &mut Streaming<ActivationEvent>,
    http_port: u16,
    staged_path: String,
    transport_file: Option<String>,
    mxl_flow_id: Option<String>,
) -> ActivationEvent {
    tokio::spawn(async move {
        http_patch_activate_immediate(
            "127.0.0.1",
            http_port,
            &staged_path,
            transport_file.as_deref(),
            mxl_flow_id.as_deref(),
        )
        .await
        .unwrap_or_else(|e| panic!("PATCH /staged: {e}"));
    });
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("activation event timeout")
        .expect("activation stream ended")
        .expect("activation stream error")
}

async fn ack_activation(
    client: &mut NvnmosDaemonClient<Channel>,
    session: &str,
    activation_handle: String,
) {
    client
        .ack_activation(AckActivationRequest {
            session_handle: session.to_string(),
            activation_handle,
            success: true,
            failure_reason: String::new(),
        })
        .await
        .expect("AckActivation");
}

struct RtpSenderCase {
    name: &'static str,
    configuring_session_name: &'static str,
    configuring_session_info: Option<&'static str>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sender_sdp_session_fields_come_from_is04_label_and_description() {
    const CASES: &[RtpSenderCase] = &[
        RtpSenderCase {
            name: "s1",
            configuring_session_name: "sender-label",
            configuring_session_info: Some("sender-desc"),
        },
        RtpSenderCase {
            name: "s2",
            configuring_session_name: "no-info-label",
            configuring_session_info: None,
        },
        RtpSenderCase {
            name: "s3",
            configuring_session_name: " ",
            configuring_session_info: Some("minimal-name-desc"),
        },
    ];

    let mut harness = DaemonHarness::spawn(18_180, 18_189, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();
    let (session, http_port) = open_session_with_port(&mut client, "sdp-sender-si").await;

    let mut stream = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeActivations")
        .into_inner();

    for (index, case) in CASES.iter().enumerate() {
        let rtp_port = 5020 + 2 * index as u16;
        let sender = client
            .add_sender(AddSenderRequest {
                session_handle: session.clone(),
                name: case.name.to_string(),
                transport: ProtoTransport::Rtp as i32,
                transport_file: configuring_video_sdp(
                    case.name,
                    &iface,
                    case.configuring_session_name,
                    case.configuring_session_info,
                    rtp_port,
                    true,
                ),
            })
            .await
            .expect("AddSender")
            .into_inner();

        let (label, description) =
            http_get_label_description(http_port, "senders", &sender.sender_id).await;
        let (expect_session_name, expect_session_info) =
            expected_sdp_session_fields(&label, &description);

        let staged_path = format!(
            "/x-nmos/connection/v1.1/single/senders/{}/staged",
            sender.sender_id
        );
        let event = activate(&mut stream, http_port, staged_path, None, None).await;
        assert_sdp_session_fields(
            event.transport_file.as_deref().expect("activation SDP"),
            &expect_session_name,
            expect_session_info.as_deref(),
            &format!("{} activation SDP", case.name),
        );

        ack_activation(&mut client, &session, event.activation_handle).await;

        let tf_path = format!(
            "/x-nmos/connection/v1.1/single/senders/{}/transportfile",
            sender.sender_id
        );
        let (status, resp) = http_get("127.0.0.1", http_port, &tf_path)
            .await
            .expect("GET /transportfile");
        assert!(
            (200..300).contains(&status),
            "GET /transportfile failed: {resp}"
        );
        assert_sdp_session_fields(
            http_body(&resp),
            &expect_session_name,
            expect_session_info.as_deref(),
            &format!("{} /transportfile", case.name),
        );
    }

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
    drop(harness);
}

struct StagedRtpReceiverCase {
    name: &'static str,
    configuring_session_name: &'static str,
    configuring_session_info: Option<&'static str>,
    staged_session_name: &'static str,
    staged_session_info: Option<&'static str>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receiver_sdp_session_fields_come_from_staged_sdp() {
    const CASES: &[StagedRtpReceiverCase] = &[
        StagedRtpReceiverCase {
            name: "r1",
            configuring_session_name: "configuring-label",
            configuring_session_info: Some("configuring-desc"),
            staged_session_name: "staged-label",
            staged_session_info: Some("staged-desc"),
        },
        StagedRtpReceiverCase {
            name: "r2",
            configuring_session_name: "no-info-configuring-label",
            configuring_session_info: Some("no-info-configuring-desc"),
            staged_session_name: "no-info-staged-label",
            staged_session_info: None,
        },
        StagedRtpReceiverCase {
            name: "r3",
            configuring_session_name: "minimal-name-configuring-label",
            configuring_session_info: None,
            staged_session_name: " ",
            staged_session_info: Some("minimal-name-staged-desc"),
        },
    ];

    let mut harness = DaemonHarness::spawn(18_190, 18_199, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();
    let (session, http_port) = open_session_with_port(&mut client, "sdp-receiver-staged").await;

    let mut stream = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeActivations")
        .into_inner();

    for (index, case) in CASES.iter().enumerate() {
        let rtp_port = 5020 + 2 * index as u16;
        let receiver = client
            .add_receiver(AddReceiverRequest {
                session_handle: session.clone(),
                name: case.name.to_string(),
                transport: ProtoTransport::Rtp as i32,
                transport_file: configuring_video_sdp(
                    case.name,
                    &iface,
                    case.configuring_session_name,
                    case.configuring_session_info,
                    rtp_port,
                    false,
                ),
            })
            .await
            .expect("AddReceiver")
            .into_inner();

        let staged_path = format!(
            "/x-nmos/connection/v1.1/single/receivers/{}/staged",
            receiver.receiver_id
        );
        let staged_sdp = staged_video_sdp(
            &iface,
            case.staged_session_name,
            case.staged_session_info,
            rtp_port,
        );
        let event = activate(&mut stream, http_port, staged_path, Some(staged_sdp), None).await;
        assert_sdp_session_fields(
            event.transport_file.as_deref().expect("activation SDP"),
            case.staged_session_name,
            case.staged_session_info,
            &format!("{} activation SDP", case.name),
        );

        ack_activation(&mut client, &session, event.activation_handle).await;
    }

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
    drop(harness);
}

struct UnstagedRtpReceiverCase {
    name: &'static str,
    configuring_session_name: &'static str,
    configuring_session_info: Option<&'static str>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receiver_sdp_session_fields_without_staged_sdp_come_from_is04() {
    const CASES: &[UnstagedRtpReceiverCase] = &[
        UnstagedRtpReceiverCase {
            name: "r4",
            configuring_session_name: "unstaged-label",
            configuring_session_info: Some("unstaged-desc"),
        },
        UnstagedRtpReceiverCase {
            name: "r5",
            configuring_session_name: "unstaged-no-info-label",
            configuring_session_info: None,
        },
        UnstagedRtpReceiverCase {
            name: "r6",
            configuring_session_name: " ",
            configuring_session_info: Some("unstaged-minimal-name-desc"),
        },
    ];

    let mut harness = DaemonHarness::spawn(18_200, 18_209, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();
    let (session, http_port) = open_session_with_port(&mut client, "sdp-receiver-unstaged").await;

    let mut stream = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeActivations")
        .into_inner();

    for (index, case) in CASES.iter().enumerate() {
        let rtp_port = 5120 + 2 * index as u16;
        let receiver = client
            .add_receiver(AddReceiverRequest {
                session_handle: session.clone(),
                name: case.name.to_string(),
                transport: ProtoTransport::Rtp as i32,
                transport_file: configuring_video_sdp(
                    case.name,
                    &iface,
                    case.configuring_session_name,
                    case.configuring_session_info,
                    rtp_port,
                    false,
                ),
            })
            .await
            .expect("AddReceiver")
            .into_inner();

        let (label, description) =
            http_get_label_description(http_port, "receivers", &receiver.receiver_id).await;
        let (expect_session_name, expect_session_info) =
            expected_sdp_session_fields(&label, &description);

        let staged_path = format!(
            "/x-nmos/connection/v1.1/single/receivers/{}/staged",
            receiver.receiver_id
        );
        let event = activate(&mut stream, http_port, staged_path, None, None).await;
        assert_sdp_session_fields(
            event.transport_file.as_deref().expect("activation SDP"),
            &expect_session_name,
            expect_session_info.as_deref(),
            &format!("{} activation SDP", case.name),
        );

        ack_activation(&mut client, &session, event.activation_handle).await;
    }

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
    drop(harness);
}

struct MxlCase {
    name: &'static str,
    configuring_label: &'static str,
    configuring_description: &'static str,
    sender: bool,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mxl_flow_def_label_and_description_come_from_is04() {
    const CASES: &[MxlCase] = &[
        MxlCase {
            name: "ms1",
            configuring_label: "mxl-sender-label",
            configuring_description: "mxl-sender-desc",
            sender: true,
        },
        MxlCase {
            name: "ms2",
            configuring_label: "mxl-sender-no-desc",
            configuring_description: "",
            sender: true,
        },
        MxlCase {
            name: "ms3",
            configuring_label: "",
            configuring_description: "mxl-sender-no-label",
            sender: true,
        },
        MxlCase {
            name: "mr1",
            configuring_label: "mxl-receiver-label",
            configuring_description: "mxl-receiver-desc",
            sender: false,
        },
        MxlCase {
            name: "mr2",
            configuring_label: "mxl-receiver-no-desc",
            configuring_description: "",
            sender: false,
        },
        MxlCase {
            name: "mr3",
            configuring_label: "",
            configuring_description: "mxl-receiver-no-label",
            sender: false,
        },
    ];

    let mut harness = DaemonHarness::spawn(18_210, 18_219, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let (session, http_port) = open_session_with_port(&mut client, "mxl-session-si").await;

    let mut stream = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeActivations")
        .into_inner();

    for (index, case) in CASES.iter().enumerate() {
        let flow_def = video_flow_def(
            case.name,
            case.configuring_label,
            case.configuring_description,
        );
        // MXL transport_params (mxl_flow_id) are IS-05 v1.2.
        let (resource_type, resource_id, staged_path, mxl_flow_id) = if case.sender {
            let sender = client
                .add_sender(AddSenderRequest {
                    session_handle: session.clone(),
                    name: case.name.to_string(),
                    transport: ProtoTransport::Mxl as i32,
                    transport_file: flow_def,
                })
                .await
                .expect("AddSender")
                .into_inner();
            (
                "senders",
                sender.sender_id.clone(),
                format!(
                    "/x-nmos/connection/v1.2/single/senders/{}/staged",
                    sender.sender_id
                ),
                None,
            )
        } else {
            let receiver = client
                .add_receiver(AddReceiverRequest {
                    session_handle: session.clone(),
                    name: case.name.to_string(),
                    transport: ProtoTransport::Mxl as i32,
                    transport_file: flow_def,
                })
                .await
                .expect("AddReceiver")
                .into_inner();
            (
                "receivers",
                receiver.receiver_id.clone(),
                format!(
                    "/x-nmos/connection/v1.2/single/receivers/{}/staged",
                    receiver.receiver_id
                ),
                Some(format!("55555555-dddd-5555-a555-55555555555{index}")),
            )
        };

        let (label, description) =
            http_get_label_description(http_port, resource_type, &resource_id).await;
        let event = activate(&mut stream, http_port, staged_path, None, mxl_flow_id).await;
        let flow = event
            .transport_file
            .as_deref()
            .expect("activation flow_def");
        let parsed: serde_json::Value =
            serde_json::from_str(flow).unwrap_or_else(|e| panic!("{}: {e}: {flow}", case.name));
        assert_eq!(
            parsed["label"].as_str(),
            Some(label.as_str()),
            "{} activation label: {flow}",
            case.name
        );
        assert_eq!(
            parsed["description"].as_str(),
            Some(description.as_str()),
            "{} activation description: {flow}",
            case.name
        );

        ack_activation(&mut client, &session, event.activation_handle).await;
    }

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
    drop(harness);
}
