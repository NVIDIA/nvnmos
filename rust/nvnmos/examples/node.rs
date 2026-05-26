// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Rust port of `src/main.c` (`nvnmos-example`) demonstrating the safe wrapper.
//!
//! Mirrors the C example's coverage and pacing: an NMOS Node with four
//! senders and four receivers, split RTP/MXL × video/audio. The example walks
//! through the lifecycle the C app does — add → remove some → add back →
//! activate all → deactivate all → destroy — and prints the *expected* ids
//! (computed from the seed) and the *actual* ids (queried from the running
//! server) at each step so the lookup semantics around add/remove are visible.
//!
//! Pauses between phases (like the C example's `get_continue`) so the user
//! or a probing script has time to hit the IS-04 / IS-05 APIs before the
//! example moves on. Hit Enter (or answer `y`) to advance, `n` (or EOF) to
//! exit early.
//!
//! Real NMOS registration needs a real network interface, so the example is
//! driven by environment variables:
//!
//! * `NVNMOS_EXAMPLE_INTERFACE_IP` (required) — the IP of the interface to
//!   advertise (the same value that goes into `host_addresses`).
//! * `NVNMOS_EXAMPLE_PORT` (optional, default `18080`) — HTTP port.
//!
//! Skips with a friendly message if `NVNMOS_EXAMPLE_INTERFACE_IP` is not set.
//!
//! Run as:
//!
//! ```sh
//! NVNMOS_EXAMPLE_INTERFACE_IP=192.0.2.10 cargo run -p nvnmos --example node
//! ```

use std::env;
use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use nvnmos::{
    LOG_LEVEL_DEVEL, NodeConfig, NodeServer, ReceiverConfig, SenderConfig, Side, Transport,
    make_node_id, make_receiver_id, make_sender_id,
};

// === Example resource catalogue =============================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Media {
    Video,
    Audio,
}

struct Resource {
    name: &'static str,
    transport: Transport,
    media: Media,
}

// Match the C example's naming: NMOS senders are GStreamer-style sinks
// (`sink-N` / `mxl-sink-N`) and NMOS receivers are sources
// (`source-N` / `mxl-source-N`).
const SENDERS: &[Resource] = &[
    Resource { name: "sink-0",       transport: Transport::Rtp, media: Media::Video },
    Resource { name: "sink-1",       transport: Transport::Rtp, media: Media::Audio },
    Resource { name: "mxl-sink-0",   transport: Transport::Mxl, media: Media::Video },
    Resource { name: "mxl-sink-1",   transport: Transport::Mxl, media: Media::Audio },
];

const RECEIVERS: &[Resource] = &[
    Resource { name: "source-0",     transport: Transport::Rtp, media: Media::Video },
    Resource { name: "source-1",     transport: Transport::Rtp, media: Media::Audio },
    Resource { name: "mxl-source-0", transport: Transport::Mxl, media: Media::Video },
    Resource { name: "mxl-source-1", transport: Transport::Mxl, media: Media::Audio },
];

// MXL example UUIDs, lifted verbatim from `src/main.c` so this example
// produces the same flow ids the C example does.
const MXL_DOMAIN_ID: &str = "212ba127-f746-43c5-87d4-3962ec7ff284";
const MXL_VIDEO_FLOW_ID: &str = "5ede7baf-9dcf-4b80-9e44-bc0f615633b4";
const MXL_AUDIO_FLOW_ID: &str = "92029e8a-fb63-46d7-b2f4-abe2f8dbf083";

// === Reporting helpers ======================================================

fn print_id(kind: &str, name: &str, value: &str) {
    println!("  {kind:<8}  {name:<12}  {value}");
}

/// Mirrors `get_continue` in `src/main.c`: prompts and reads a line from
/// stdin so the user (or a probing script) can hit the IS-04 / IS-05 APIs
/// before the example mutates the model again. Treats EOF and `n` / `N` as
/// "stop"; anything else (Enter, `y`, …) as "continue".
fn ask_continue() -> bool {
    print!("Continue ([y]/n)? ");
    io::stdout().flush().ok();
    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(0) => false,
        Ok(_) => !matches!(line.trim().to_ascii_lowercase().as_str(), "n" | "no"),
        Err(_) => false,
    }
}

fn print_expected(seed: &str) {
    println!("Expected NMOS IDs (computed from seed):");
    print_id(
        "node",
        "",
        &make_node_id(seed).unwrap_or_else(|_| "<error>".into()),
    );
    for r in SENDERS {
        print_id(
            "sender",
            r.name,
            &make_sender_id(seed, r.name).unwrap_or_else(|_| "<error>".into()),
        );
    }
    for r in RECEIVERS {
        print_id(
            "receiver",
            r.name,
            &make_receiver_id(seed, r.name).unwrap_or_else(|_| "<error>".into()),
        );
    }
}

fn print_actual(server: &NodeServer) {
    println!("Actual NMOS IDs (queried from running server):");
    print_id(
        "node",
        "",
        &server.node_id().unwrap_or_else(|_| "<error>".into()),
    );
    for r in SENDERS {
        let value = match server.sender_id(r.name) {
            Ok(Some(v)) => v,
            Ok(None) => "<not found>".into(),
            Err(_) => "<error>".into(),
        };
        print_id("sender", r.name, &value);
    }
    for r in RECEIVERS {
        let value = match server.receiver_id(r.name) {
            Ok(Some(v)) => v,
            Ok(None) => "<not found>".into(),
            Err(_) => "<error>".into(),
        };
        print_id("receiver", r.name, &value);
    }
}

// === Transport-file builders ===============================================

fn build_transport_file(r: &Resource, sender: bool, iface_ip: &str) -> String {
    match (r.transport, r.media) {
        (Transport::Rtp, Media::Video) => build_video_sdp(r.name, sender, iface_ip),
        (Transport::Rtp, Media::Audio) => build_audio_sdp(r.name, sender, iface_ip),
        (Transport::Mxl, Media::Video) => {
            build_video_flow_def(r.name, sender, MXL_DOMAIN_ID, MXL_VIDEO_FLOW_ID)
        }
        (Transport::Mxl, Media::Audio) => {
            build_audio_flow_def(r.name, sender, MXL_DOMAIN_ID, MXL_AUDIO_FLOW_ID)
        }
        // `Transport` is `#[non_exhaustive]`; the catalogue above only uses
        // variants we know how to render.
        _ => unreachable!("unsupported transport in example catalogue"),
    }
}

/// SDP for an ST 2110-20 video sender or receiver. Modelled on
/// `init_video_sdp` in `src/main.c`.
fn build_video_sdp(name: &str, sender: bool, iface_ip: &str) -> String {
    const MULTICAST_IP: &str = "233.252.0.0";
    const SOURCE_IP: &str = "192.0.2.0";
    const DESTINATION_PORT: u16 = 5020;
    const SOURCE_PORT: u16 = 5004;
    const PAYLOAD_TYPE: u8 = 96;
    const ENCODING: &str = "raw/90000";
    const FMTP: &str = "sampling=YCbCr-4:2:2; width=1920; height=1080; \
        exactframerate=50; depth=10; TCS=SDR; colorimetry=BT709; \
        PM=2110GPM; SSN=ST2110-20:2017; TP=2110TPN; ";
    const TS_REFCLK: &str = "a=ts-refclk:localmac=CA-FE-01-CA-FE-02\r\n";

    let mut out = format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {iface_ip}\r\n\
         s=nvnmos example video {direction} {name}\r\n\
         t=0 0\r\n\
         a=x-nvnmos-name:{name}\r\n\
         m=video {DESTINATION_PORT} RTP/AVP {PAYLOAD_TYPE}\r\n\
         c=IN IP4 {MULTICAST_IP}/64\r\n\
         a=source-filter: incl IN IP4 {MULTICAST_IP} {filter_src}\r\n\
         a=x-nvnmos-iface-ip:{iface_ip}\r\n\
         a=rtpmap:{PAYLOAD_TYPE} {ENCODING}\r\n\
         a=fmtp:{PAYLOAD_TYPE} {FMTP}\r\n\
         a=mediaclk:direct=0\r\n",
        direction = if sender { "sender" } else { "receiver" },
        filter_src = if sender { iface_ip } else { SOURCE_IP },
    );
    if sender {
        use std::fmt::Write;
        let _ = write!(out, "a=x-nvnmos-src-port:{SOURCE_PORT}\r\n");
        out.push_str(TS_REFCLK);
    }
    out
}

/// SDP for an ST 2110-30 audio sender or receiver. Modelled on
/// `init_audio_sdp` in `src/main.c`.
fn build_audio_sdp(name: &str, sender: bool, iface_ip: &str) -> String {
    const MULTICAST_IP: &str = "233.252.0.1";
    const SOURCE_IP: &str = "192.0.2.1";
    const DESTINATION_PORT: u16 = 5030;
    const SOURCE_PORT: u16 = 5004;
    const PAYLOAD_TYPE: u8 = 97;
    const ENCODING: &str = "L24/48000/2";
    const FMTP: &str = "channel-order=SMPTE2110.(ST); ";
    const TS_REFCLK: &str = "a=ts-refclk:localmac=CA-FE-01-CA-FE-02\r\n";

    let mut out = format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {iface_ip}\r\n\
         s=nvnmos example audio {direction} {name}\r\n\
         t=0 0\r\n\
         a=x-nvnmos-name:{name}\r\n\
         m=audio {DESTINATION_PORT} RTP/AVP {PAYLOAD_TYPE}\r\n\
         c=IN IP4 {MULTICAST_IP}/64\r\n\
         a=source-filter: incl IN IP4 {MULTICAST_IP} {filter_src}\r\n\
         a=x-nvnmos-iface-ip:{iface_ip}\r\n\
         a=rtpmap:{PAYLOAD_TYPE} {ENCODING}\r\n\
         a=fmtp:{PAYLOAD_TYPE} {FMTP}\r\n\
         a=mediaclk:direct=0\r\n",
        direction = if sender { "sender" } else { "receiver" },
        filter_src = if sender { iface_ip } else { SOURCE_IP },
    );
    if sender {
        use std::fmt::Write;
        let _ = write!(out, "a=x-nvnmos-src-port:{SOURCE_PORT}\r\n");
        out.push_str("a=ptime:1\r\n");
        out.push_str(TS_REFCLK);
    }
    out
}

/// MXL flow definition for an uncompressed v210 video sender or receiver.
/// Modelled on `init_video_flow_def` in `src/main.c`; see AMWA BCP-007-03.
fn build_video_flow_def(
    name: &str,
    sender: bool,
    mxl_domain_id: &str,
    mxl_flow_id: &str,
) -> String {
    // 1080p59.94 video/v210 (YCbCr-4:2:2, 10 bit, progressive).
    const W: u32 = 1920;
    const H: u32 = 1080;
    let wh = W / 2;
    let direction = if sender { "Sender" } else { "Receiver" };
    format!(
        r#"{{
  "id": "{mxl_flow_id}",
  "label": "nvnmos example MXL Video {direction} {name}",
  "description": "YCbCr-4:2:2, 10 bit, 1920 x 1080, progressive, 59.94 Hz",
  "tags": {{
    "urn:x-nmos:tag:grouphint/v1.0": [ "{name}:video" ],
    "urn:x-nvnmos:tag:name": [ "{name}" ],
    "urn:x-nvnmos:tag:mxl-domain-id": [ "{mxl_domain_id}" ]
  }},
  "format": "urn:x-nmos:format:video",
  "media_type": "video/v210",
  "grain_rate": {{ "numerator": 60000, "denominator": 1001 }},
  "frame_width": {W},
  "frame_height": {H},
  "interlace_mode": "progressive",
  "colorspace": "BT709",
  "transfer_characteristic": "SDR",
  "components": [
    {{ "name": "Y",  "width": {W},  "height": {H}, "bit_depth": 10 }},
    {{ "name": "Cb", "width": {wh}, "height": {H}, "bit_depth": 10 }},
    {{ "name": "Cr", "width": {wh}, "height": {H}, "bit_depth": 10 }}
  ]
}}
"#
    )
}

/// MXL flow definition for an audio/float32 sender or receiver.
/// Modelled on `init_audio_flow_def` in `src/main.c`.
fn build_audio_flow_def(
    name: &str,
    sender: bool,
    mxl_domain_id: &str,
    mxl_flow_id: &str,
) -> String {
    let direction = if sender { "Sender" } else { "Receiver" };
    format!(
        r#"{{
  "id": "{mxl_flow_id}",
  "label": "nvnmos example MXL Audio {direction} {name}",
  "description": "2 ch, 48 kHz, 32 bit",
  "tags": {{
    "urn:x-nmos:tag:grouphint/v1.0": [ "{name}:audio" ],
    "urn:x-nvnmos:tag:name": [ "{name}" ],
    "urn:x-nvnmos:tag:mxl-domain-id": [ "{mxl_domain_id}" ]
  }},
  "format": "urn:x-nmos:format:audio",
  "media_type": "audio/float32",
  "sample_rate": {{ "numerator": 48000, "denominator": 1 }},
  "channel_count": 2,
  "bit_depth": 32
}}
"#
    )
}

// === Main ==================================================================

// Senders and receivers that the example removes and then re-adds, to
// exercise both the remove path and the late-add path of the model.
const REMOVED_SENDERS: &[&str] = &["sink-0", "mxl-sink-1"];
const REMOVED_RECEIVERS: &[&str] = &["source-0", "mxl-source-1"];

fn main() -> ExitCode {
    let Ok(iface_ip) = env::var("NVNMOS_EXAMPLE_INTERFACE_IP") else {
        eprintln!(
            "set NVNMOS_EXAMPLE_INTERFACE_IP=<host-ip> to run this example \
             (and optionally NVNMOS_EXAMPLE_PORT)"
        );
        return ExitCode::SUCCESS;
    };
    let port: u16 = env::var("NVNMOS_EXAMPLE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(18080);

    let seed = format!("{iface_ip}:{port}");

    print_expected(&seed);

    // Pre-build the sender / receiver configs once so we can re-use them
    // across the add / remove / add-back / activate cycle below.
    let senders: Vec<(&'static str, SenderConfig)> = SENDERS
        .iter()
        .map(|r| {
            (
                r.name,
                SenderConfig {
                    transport: r.transport,
                    transport_file: build_transport_file(r, true, &iface_ip),
                },
            )
        })
        .collect();
    let receivers: Vec<(&'static str, ReceiverConfig)> = RECEIVERS
        .iter()
        .map(|r| {
            (
                r.name,
                ReceiverConfig {
                    transport: r.transport,
                    transport_file: build_transport_file(r, false, &iface_ip),
                },
            )
        })
        .collect();

    let config = NodeConfig {
        seed: seed.clone(),
        host_addresses: vec![iface_ip.clone()],
        http_port: port,
        label: "nvnmos Rust example".into(),
        // Diagnostic: surface every libnvnmos log message (down to "devel")
        // so the log callback can capture activation exceptions and the like.
        log_level: LOG_LEVEL_DEVEL,
        ..Default::default()
    };
    // Mirror `handle_connection_activated` in `src/main.c`: log each IS-05
    // activation as it arrives and approve it. The smoke script (or a manual
    // `curl -X PATCH` against `/x-nmos/connection/v1.2/single/{senders,
    // receivers}/<id>/staged`) drives this path.
    //
    // Also forward libnvnmos's slog output to stderr — useful for diagnosing
    // activation failures (HTTP 500 with "Implementation error" / "JSON
    // error" / "Unexpected exception" surface here verbatim).
    let server = match NodeServer::builder(&config)
        .on_activation(|activation| {
            let verb = if activation.transport_file.is_some() {
                "activated via NMOS"
            } else {
                "deactivated via NMOS"
            };
            let role = match activation.side {
                Side::Sender => "sender",
                Side::Receiver => "receiver",
            };
            println!("{role} {} {verb}", activation.name);
            if let Some(tf) = activation.transport_file {
                println!("{tf}");
            }
            Ok(())
        })
        .on_log(|log| {
            eprintln!(
                "[nvnmos {} {}] {}",
                log.level, log.categories, log.message
            );
        })
        .build()
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to create NodeServer: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("Adding senders and receivers...");
    for (id, cfg) in &senders {
        if let Err(e) = server.add_sender(cfg) {
            eprintln!("add_sender({id}) failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    for (id, cfg) in &receivers {
        if let Err(e) = server.add_receiver(cfg) {
            eprintln!("add_receiver({id}) failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    print_actual(&server);
    if !ask_continue() {
        return ExitCode::SUCCESS;
    }

    println!("Removing some senders and receivers...");
    for id in REMOVED_SENDERS {
        if let Err(e) = server.remove_sender(id) {
            eprintln!("remove_sender({id}) failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    for id in REMOVED_RECEIVERS {
        if let Err(e) = server.remove_receiver(id) {
            eprintln!("remove_receiver({id}) failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    print_actual(&server);
    if !ask_continue() {
        return ExitCode::SUCCESS;
    }

    println!("Adding back some senders and receivers...");
    for id in REMOVED_SENDERS {
        let cfg = senders
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, c)| c)
            .expect("REMOVED_SENDERS must reference an id in SENDERS");
        if let Err(e) = server.add_sender(cfg) {
            eprintln!("add_sender({id}) failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    for id in REMOVED_RECEIVERS {
        let cfg = receivers
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, c)| c)
            .expect("REMOVED_RECEIVERS must reference an id in RECEIVERS");
        if let Err(e) = server.add_receiver(cfg) {
            eprintln!("add_receiver({id}) failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    print_actual(&server);
    if !ask_continue() {
        return ExitCode::SUCCESS;
    }

    println!("Activating senders and receivers...");
    for (name, cfg) in &senders {
        if let Err(e) = server.activate_connection(Side::Sender, name, Some(&cfg.transport_file)) {
            eprintln!("activate sender({name}) failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    for (name, cfg) in &receivers {
        if let Err(e) =
            server.activate_connection(Side::Receiver, name, Some(&cfg.transport_file))
        {
            eprintln!("activate receiver({name}) failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    if !ask_continue() {
        return ExitCode::SUCCESS;
    }

    println!("Deactivating senders and receivers...");
    for (name, _) in &senders {
        if let Err(e) = server.activate_connection(Side::Sender, name, None) {
            eprintln!("deactivate sender({name}) failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    for (name, _) in &receivers {
        if let Err(e) = server.activate_connection(Side::Receiver, name, None) {
            eprintln!("deactivate receiver({name}) failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    if !ask_continue() {
        return ExitCode::SUCCESS;
    }

    println!("Destroying NvNmos server...");
    ExitCode::SUCCESS
}
