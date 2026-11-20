// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The channel symbols of the IS-04 Source of an RTP audio Sender come from
//! the ST 2110-30 `channel-order` in its SDP. Channels that the
//! `channel-order` does not declare are undefined (`U01`, `U02`, ...), and
//! without a `channel-order` every channel is undefined.

mod common;

use nvnmos_rpc::v1::{
    AddSenderRequest, CloseSessionRequest, SubscribeActivationsRequest, Transport as ProtoTransport,
};

use common::{DaemonHarness, autodetect_iface_ip, connect, http_get_json, open_session_with_port};

fn audio_sdp(
    name: &str,
    iface_ip: &str,
    rtp_port: u16,
    channels: usize,
    channel_order: Option<&str>,
) -> String {
    let fmtp = channel_order.map_or(String::new(), |order| {
        format!("a=fmtp:97 channel-order={order}\r\n")
    });
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {iface_ip}\r\n\
         s={name}\r\n\
         t=0 0\r\n\
         a=x-nvnmos-name:{name}\r\n\
         m=audio {rtp_port} RTP/AVP 97\r\n\
         c=IN IP4 233.252.0.1/64\r\n\
         a=source-filter: incl IN IP4 233.252.0.1 {iface_ip}\r\n\
         a=x-nvnmos-iface-ip:{iface_ip}\r\n\
         a=rtpmap:97 L24/48000/{channels}\r\n\
         {fmtp}\
         a=ptime:1\r\n\
         a=mediaclk:direct=0\r\n\
         a=x-nvnmos-src-port:{rtp_port}\r\n\
         a=ts-refclk:localmac=CA-FE-01-CA-FE-02\r\n"
    )
}

struct Case {
    name: &'static str,
    channel_order: Option<&'static str>,
    symbols: &'static [&'static str],
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_source_channel_symbols_come_from_channel_order() {
    const CASES: &[Case] = &[
        Case {
            name: "surround51",
            channel_order: Some("SMPTE2110.(51)"),
            symbols: &["L", "R", "C", "LFE", "Ls", "Rs"],
        },
        Case {
            name: "surround71",
            channel_order: Some("SMPTE2110.(71)"),
            symbols: &["L", "R", "C", "LFE", "Lss", "Rss", "Lrs", "Rrs"],
        },
        Case {
            name: "stereo-dual-mono",
            channel_order: Some("SMPTE2110.(ST,DM)"),
            symbols: &["L", "R", "M1", "M2"],
        },
        Case {
            name: "no-channel-order",
            channel_order: None,
            symbols: &["U01", "U02"],
        },
        Case {
            name: "undeclared-channels",
            channel_order: Some("SMPTE2110.(ST)"),
            symbols: &["L", "R", "U01", "U02"],
        },
        Case {
            name: "too-many-declared",
            channel_order: Some("SMPTE2110.(51)"),
            symbols: &["L", "R"],
        },
    ];

    let mut harness = DaemonHarness::spawn(18_460..=18_469, &[]);
    harness.ready().await;
    let mut client = connect(&harness.uds).await;
    let iface = autodetect_iface_ip();
    let (session, http_port) = open_session_with_port(&mut client, "audio-source-channels").await;
    let _stream = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session.clone(),
        })
        .await
        .expect("SubscribeActivations")
        .into_inner();

    for (index, case) in CASES.iter().enumerate() {
        let sender = client
            .add_sender(AddSenderRequest {
                session_handle: session.clone(),
                name: case.name.to_string(),
                transport: ProtoTransport::Rtp as i32,
                transport_file: audio_sdp(
                    case.name,
                    &iface,
                    5220 + 2 * index as u16,
                    case.symbols.len(),
                    case.channel_order,
                ),
            })
            .await
            .unwrap_or_else(|e| panic!("{} AddSender: {e}", case.name))
            .into_inner();

        let source = http_get_json(
            http_port,
            &format!("/x-nmos/node/v1.3/sources/{}", sender.source_id),
        )
        .await;
        let symbols: Vec<&str> = source["channels"]
            .as_array()
            .unwrap_or_else(|| panic!("{} Source channels: {source}", case.name))
            .iter()
            .map(|channel| channel["symbol"].as_str().expect("channel symbol"))
            .collect();
        assert_eq!(symbols, case.symbols, "{} Source: {source}", case.name);
    }

    let _ = client
        .close_session(CloseSessionRequest {
            session_handle: session,
        })
        .await;
    drop(harness);
}
