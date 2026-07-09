// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end audio/video/caption synchronisation through NMOS Senders and
//! Receivers, parameterised over transport (`mxl`, `udp`, `nvdsudp`).
//!
//! The producer runs the phase-locked [`avsyncvideotestsrc`] and
//! [`avsyncaudiotestsrc`] from the `gst-avsynctest-rs` crate. `st2038extractor`
//! splits the video's ancillary data (the frame index plus a phase-locked
//! CEA-708 TICK/TOCK caption) off into its own flow, so the Node exposes three
//! Senders — video (v210/UYVP), data (ST 2038), audio (F32LE/S24BE):
//!
//! ```text
//! avsyncvideotestsrc (video+data) ! ... ! st2038extractor ─ ext.src    ! queue ! nmossink (video)
//!                                                         └ ext.st2038 ! queue ! nmossink (data)
//! avsyncaudiotestsrc (audio) ! ...                                     ! queue ! nmossink (audio)
//! ```
//!
//! The consumer's three Receivers read the flows back; `st2038combiner`
//! re-attaches the ancillary data onto the recovered video frames, so the
//! caption rides the video buffer again and A/V + caption alignment can be
//! asserted on two appsinks:
//!
//! ```text
//! nmossrc (video) ! queue ! comb.sink    ─ st2038combiner ! queue ! appsink (video+data)
//! nmossrc (data)  ! queue ! comb.st2038 ─┘
//! nmossrc (audio) ! queue                                         ! appsink (audio)
//! ```
//!
//! Ported from `mxl/rust/gst-mxl-rs/tests/av_sync.rs`, substituting
//! `nmossink`/`nmossrc` (+ an `nvnmosd` child process) for the bare
//! `mxlsink`/`mxlsrc`, and adding the ancillary data flow and caption check.
//!
//! Each case self-gates: it runs when its transport's toolchain is present and
//! otherwise prints a skip reason (so a checkout without MXL / DeepStream
//! neither fails nor is silently `#[ignore]`d).
//!
//! MXL co-times both flows through one grain-index→PTS mapping, so ancillary
//! pairs exactly (`anc_tolerance_frames() == 0`) and captions survive. The `udp`
//! case runs but relaxes ancillary to +/-1 frame: the software RTP receive path
//! (`udpsrc ! rtp*depay`, no rtpjitterbuffer) derives each flow's PTS from
//! packet arrival time, so a video frame's whole ancillary group (the index and
//! its co-timed caption CDP) shifts onto an adjacent frame (a "double" next to a
//! "none") — every packet still arrives, within one frame of its home, and the
//! caption always rides with its bar-centre index. Exact `udp` pairing needs an
//! RFC 7273-sync jitterbuffer on a shared PTP clock (see [`skip_reason`]).
//! Captions are still asserted best-effort because atomic index+caption delivery
//! depends on the `rtpsmpte291depay` multi-ANC fix that is not yet in a released
//! `gst-plugins-rs`; on a stock toolchain the second ANC packet is mis-read.
//! `nvdsudp` needs the DeepStream + PTP runtime and self-skips without it.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

use common::{init, nvnmosd_skip_reason, DaemonGuard};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use test_skip::skip;

// Cadence. 25 fps puts an exact frame on every 200 ms pip and — unlike 50 fps —
// keeps each TICK/TOCK caption's CEA-708 DTVCC packet inside a single frame's
// CDP, so a bar-centre frame carries its whole caption (at 50 fps the caption
// spills into the next frame's cc_data). 25/1 is CDP-representable.
const FR_NUM: i32 = 25;
const FR_DEN: i32 = 1;
const FRAME_PERIOD_NS: u64 = gst::ClockTime::SECOND.nseconds() * FR_DEN as u64 / FR_NUM as u64;
const PIP_INTERVAL_NS: u64 = 200_000_000; // 200 ms
const FRAMES_PER_INTERVAL: u64 = PIP_INTERVAL_NS / FRAME_PERIOD_NS; // 5
// Small frames keep every push cheap; width is a multiple of 6 (v210 group) and
// of 2 (UYVP pair), height > 1 so the bar is visible on more than one line.
const WIDTH: i32 = 192;
const HEIGHT: i32 = 4;
const NUM_FRAMES: i32 = 75; // 3 s at 25 fps; < 256 so the byte-0 index is unambiguous
const RATE: i32 = 48_000;
const NUM_AUDIO_BUFFERS: i32 = 155; // covers the video run with margin
/// Maximum inter-flow attach offset, in frames. Each Receiver's reader anchors
/// at its flow's head as it appears; the video and data readers can anchor up to
/// this many frames apart, so only that many head frames may arrive before the
/// combiner can pair ancillary onto them. After attach there are no drops (in
/// practice the offset is one or two frames). Applied once — not per reader.
const ATTACH_SLACK: usize = 5;
/// Extra grace (`GstAggregator:latency`) for the `st2038combiner` to wait for a
/// late ancillary grain before finishing a video frame. The combiner's own
/// latency is one frame, so by default the ancillary grain's journey (producer
/// commit → MXL → data `mxlsrc` → queue → combiner) has only ~one frame plus the
/// reported upstream latency to arrive. This test co-locates producer and
/// consumer in a single process, so scheduling jitter of the consumer's data
/// reader thread (amplified under CPU load) can occasionally push the grain past
/// that deadline; the aggregator would then emit the video frame bare and
/// `drop-late` would discard the now-late grain (the data itself is never lost —
/// see the module header). One extra frame absorbs that jitter (proven: 0 vs. the
/// default window's intermittent drops under load). Folded straight into the
/// aggregator's wait deadline and reported downstream, so sync stays consistent.
const COMBINER_LATENCY_NS: u64 = FRAME_PERIOD_NS; // 40 ms at 25 fps

const NODE_SEED: &str = "nvnmos-avsync-test";
const VIDEO_SENDER: &str = "video-sender";
const DATA_SENDER: &str = "data-sender";
const AUDIO_SENDER: &str = "audio-sender";
const VIDEO_RECEIVER: &str = "video-receiver";
const DATA_RECEIVER: &str = "data-receiver";
const AUDIO_RECEIVER: &str = "audio-receiver";

// Stable per-flow MXL identifiers (each run gets a fresh /dev/shm domain, so
// these never collide across runs).
const DOMAIN_ID: &str = "11111111-2222-3333-4444-555555555555";
const VIDEO_FLOW_ID: &str = "00000000-0000-0000-0000-0000000000a0";
const DATA_FLOW_ID: &str = "00000000-0000-0000-0000-0000000000d0";
const AUDIO_FLOW_ID: &str = "00000000-0000-0000-0000-0000000000b0";

// RTP/UDP multicast destinations (one group per flow).
const VIDEO_MCAST: (&str, u32) = ("232.99.98.1", 5040);
const DATA_MCAST: (&str, u32) = ("232.99.98.2", 5042);
const AUDIO_MCAST: (&str, u32) = ("232.99.98.3", 5044);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transport {
    Mxl,
    Udp,
    NvdsUdp,
}

impl Transport {
    fn nmos_name(self) -> &'static str {
        match self {
            Transport::Mxl => "mxl",
            Transport::Udp => "udp",
            Transport::NvdsUdp => "nvdsudp",
        }
    }

    fn video_format(self) -> &'static str {
        match self {
            Transport::Mxl => "v210",
            Transport::Udp | Transport::NvdsUdp => "UYVP",
        }
    }

    fn audio_format(self) -> &'static str {
        match self {
            Transport::Mxl => "F32LE",
            Transport::Udp | Transport::NvdsUdp => "S24BE",
        }
    }

    /// `st2038combiner drop-late-st2038`. MXL (and PTP-timed nvdsudp) co-time the
    /// video and ancillary flows exactly, so late ANC is a real error worth
    /// dropping to keep pairing strict. The software RTP path times each flow from
    /// packet arrival, so the ancillary lands a few ms off its frame; dropping it
    /// would lose it, so we collect it onto the nearest frame instead.
    fn combiner_drop_late(self) -> bool {
        match self {
            Transport::Mxl | Transport::NvdsUdp => true,
            Transport::Udp => false,
        }
    }

    /// How far, in frames, a recovered frame's ancillary index may sit from the
    /// frame's own index. Zero for the co-timed transports (exact pairing); one for
    /// the software RTP path, whose arrival-time PTS can shift ancillary to an
    /// adjacent frame (see the module header).
    fn anc_tolerance_frames(self) -> u8 {
        match self {
            Transport::Mxl | Transport::NvdsUdp => 0,
            Transport::Udp => 1,
        }
    }

    /// Extra element factories the transport's inner data path needs, so a
    /// missing one skips (rather than fails) on a checkout without that stack.
    fn transport_factories(self) -> &'static [&'static str] {
        match self {
            Transport::Mxl => &["mxlsink", "mxlsrc"],
            Transport::Udp => &["udpsink", "udpsrc"],
            Transport::NvdsUdp => &["nvdsudpsink", "nvdsudpsrc"],
        }
    }
}

/// Register the aligned A/V sources (they live in a library crate, not a plugin
/// on `GST_PLUGIN_PATH`) once per process, after `common::init`'s `gst::init`.
fn register_sources() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        gst::Element::register(
            None,
            "avsyncvideotestsrc",
            gst::Rank::NONE,
            gstavsynctest::videosrc::AvSyncVideoTestSrc::static_type(),
        )
        .unwrap();
        gst::Element::register(
            None,
            "avsyncaudiotestsrc",
            gst::Rank::NONE,
            gstavsynctest::audiosrc::AvSyncAudioTestSrc::static_type(),
        )
        .unwrap();
    });
}

/// Per-test MXL Domain under `/dev/shm`, removed on drop. Mirrors the gst-mxl-rs
/// domain guard and adds a BCP-007-03 `domain_def.json` so the `mxl-domain-id`
/// cross-check in `nmossink`/`nmossrc` passes.
struct TestDomainGuard {
    dir: PathBuf,
}

impl TestDomainGuard {
    fn new(test: &str) -> Self {
        let pid = std::process::id();
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock pre-epoch")
            .as_nanos();
        let dir = PathBuf::from(format!("/dev/shm/nvnmos_avsync_{test}_{pid}_{now_nanos}"));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("create test domain `{}`: {e}", dir.display()));
        let domain_def = serde_json::json!({
            "id": DOMAIN_ID,
            "label": format!("nvnmos avsync test {test}"),
        });
        std::fs::write(
            dir.join("domain_def.json"),
            serde_json::to_string_pretty(&domain_def).expect("serialise domain_def"),
        )
        .unwrap_or_else(|e| panic!("write domain_def.json: {e}"));
        Self { dir }
    }

    fn path(&self) -> &str {
        self.dir.to_str().expect("ASCII domain path")
    }
}

impl Drop for TestDomainGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Local egress NIC IPv4 for the RTP transports (skips loopback). Mirrors the
/// example scripts' `_default_nic_ip`.
fn nic_ip() -> String {
    if let Ok(ip) = std::env::var("NVNMOS_TEST_NIC_IP") {
        return ip;
    }
    let out = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show"])
        .output();
    if let Ok(out) = out {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 4 && f[1] != "lo" {
                if let Some(ip) = f[3].split('/').next() {
                    return ip.to_owned();
                }
            }
        }
    }
    "127.0.0.1".to_owned()
}

/// Essence caps for each flow (also drives the synthesised NMOS flow_def).
fn video_caps(t: Transport) -> String {
    format!(
        "video/x-raw,format={},width={WIDTH},height={HEIGHT},framerate={FR_NUM}/{FR_DEN}",
        t.video_format()
    )
}

fn data_caps() -> String {
    format!("meta/x-st-2038,framerate={FR_NUM}/{FR_DEN}")
}

fn audio_caps(t: Transport) -> String {
    format!(
        "audio/x-raw,format={},rate={RATE},channels=1,layout=interleaved",
        t.audio_format()
    )
}

/// The transport-specific `nmossink`/`nmossrc` properties for one flow. `domain`
/// is the MXL Domain path (unused for the RTP transports).
fn transport_props(
    t: Transport,
    is_sender: bool,
    flow_id: &str,
    mcast: (&str, u32),
    nic: &str,
    domain: &str,
) -> String {
    match t {
        Transport::Mxl => {
            format!("mxl-domain-id={DOMAIN_ID} mxl-domain-path={domain} mxl-flow-id={flow_id}")
        }
        Transport::Udp | Transport::NvdsUdp => {
            let (ip, port) = mcast;
            if is_sender {
                format!("destination-ip={ip} destination-port={port} source-ip={nic}")
            } else {
                format!(
                    "multicast-ip={ip} destination-port={port} interface-ip={nic} source-ip={nic}"
                )
            }
        }
    }
}

fn build_producer(t: Transport, uri: &str, nic: &str, domain: &str) -> gst::Pipeline {
    let vcaps = video_caps(t);
    let dcaps = data_caps();
    let acaps = audio_caps(t);
    let tp = t.nmos_name();
    let video_tp = transport_props(t, true, VIDEO_FLOW_ID, VIDEO_MCAST, nic, domain);
    let data_tp = transport_props(t, true, DATA_FLOW_ID, DATA_MCAST, nic, domain);
    let audio_tp = transport_props(t, true, AUDIO_FLOW_ID, AUDIO_MCAST, nic, domain);

    let desc = format!(
        "avsyncvideotestsrc is-live=true num-buffers={NUM_FRAMES} \
             pip-interval={PIP_INTERVAL_NS} width={WIDTH} height={HEIGHT} \
             framerate={FR_NUM}/{FR_DEN} \
           ! {vcaps} \
           ! st2038extractor name=ext remove-ancillary-meta=true \
         ext.src \
           ! queue \
           ! nmossink daemon-uri=\"{uri}\" transport={tp} node-seed={NODE_SEED} \
                 sender-name={VIDEO_SENDER} auto-activate=true caps=\"{vcaps}\" {video_tp} \
         ext.st2038 \
           ! queue \
           ! nmossink daemon-uri=\"{uri}\" transport={tp} node-seed={NODE_SEED} \
                 sender-name={DATA_SENDER} auto-activate=true caps=\"{dcaps}\" {data_tp} \
         avsyncaudiotestsrc is-live=true num-buffers={NUM_AUDIO_BUFFERS} \
             pip-interval={PIP_INTERVAL_NS} \
           ! {acaps} \
           ! queue \
           ! nmossink daemon-uri=\"{uri}\" transport={tp} node-seed={NODE_SEED} \
                 sender-name={AUDIO_SENDER} auto-activate=true caps=\"{acaps}\" {audio_tp}"
    );
    gst::parse::launch(&desc)
        .expect("parse producer")
        .downcast::<gst::Pipeline>()
        .expect("producer pipeline")
}

fn build_consumer(t: Transport, uri: &str, nic: &str, domain: &str) -> gst::Pipeline {
    let vcaps = video_caps(t);
    let dcaps = data_caps();
    let acaps = audio_caps(t);
    let tp = t.nmos_name();
    let video_tp = transport_props(t, false, VIDEO_FLOW_ID, VIDEO_MCAST, nic, domain);
    let data_tp = transport_props(t, false, DATA_FLOW_ID, DATA_MCAST, nic, domain);
    let audio_tp = transport_props(t, false, AUDIO_FLOW_ID, AUDIO_MCAST, nic, domain);

    // Diagnostic: tap the raw ST-2038 flow into its own appsink (bypassing the
    // combiner's running-time matching) to attribute data loss to transport vs
    // combiner.
    let data_tap = std::env::var("NVNMOS_TEST_DATA_TAP").is_ok();
    let data_branch = if data_tap {
        format!(
            "nmossrc daemon-uri=\"{uri}\" transport={tp} node-seed={NODE_SEED} \
                 receiver-name={DATA_RECEIVER} auto-activate=true caps=\"{dcaps}\" {data_tp} \
               ! tee name=dt \
             dt. ! queue ! comb.st2038 \
             dt. ! queue ! appsink name=data_sink sync=false caps=meta/x-st-2038"
        )
    } else {
        format!(
            "nmossrc daemon-uri=\"{uri}\" transport={tp} node-seed={NODE_SEED} \
                 receiver-name={DATA_RECEIVER} auto-activate=true caps=\"{dcaps}\" {data_tp} \
               ! queue \
               ! comb.st2038"
        )
    };
    let desc = format!(
        "nmossrc daemon-uri=\"{uri}\" transport={tp} node-seed={NODE_SEED} \
             receiver-name={VIDEO_RECEIVER} auto-activate=true caps=\"{vcaps}\" {video_tp} \
           ! queue \
           ! comb.sink \
         {data_branch} \
         st2038combiner name=comb drop-late-st2038={drop_late} latency={COMBINER_LATENCY_NS} \
           ! queue \
           ! appsink name=video_sink sync=true caps=video/x-raw,format={vfmt} \
         nmossrc daemon-uri=\"{uri}\" transport={tp} node-seed={NODE_SEED} \
             receiver-name={AUDIO_RECEIVER} auto-activate=true caps=\"{acaps}\" {audio_tp} \
           ! queue \
           ! audioconvert \
            ! appsink name=audio_sink sync=false caps=audio/x-raw,format=F32LE",
        vfmt = t.video_format(),
        drop_late = t.combiner_drop_late(),
    );
    gst::parse::launch(&desc)
        .expect("parse consumer")
        .downcast::<gst::Pipeline>()
        .expect("consumer pipeline")
}

/// First ST-2038 ANC packet's first user-data byte (the frame index stamped by
/// avsyncvideotestsrc). Manual bit unpack (SMPTE ST 2038, big-endian, MSB-first);
/// first user data word starts at bit 60. Diagnostic only.
fn st2038_first_index(data: &[u8]) -> Option<u8> {
    fn read_bits(data: &[u8], bit_offset: usize, width: usize) -> Option<u32> {
        let mut out = 0u32;
        for i in 0..width {
            let bit = bit_offset + i;
            let byte = *data.get(bit / 8)?;
            let shift = 7 - (bit % 8);
            out = (out << 1) | (((byte >> shift) & 1) as u32);
        }
        Some(out)
    }
    read_bits(data, 60, 10).map(|w| (w & 0xff) as u8)
}

fn appsink(pipeline: &gst::Pipeline, name: &str) -> gst_app::AppSink {
    pipeline
        .by_name(name)
        .unwrap_or_else(|| panic!("appsink {name}"))
        .downcast::<gst_app::AppSink>()
        .expect("AppSink downcast")
}

fn running_time(sample: &gst::SampleRef) -> Option<gst::ClockTime> {
    let pts = sample.buffer()?.pts()?;
    sample
        .segment()?
        .downcast_ref::<gst::ClockTime>()?
        .to_running_time(pts)
}

/// A recovered video frame: its byte-0 frame index, running time, and *every*
/// ancillary packet the combiner attached to it — the frame indices recombined
/// from the data flow (usually one; a lossy live round-trip can land two on one
/// frame and none on a neighbour) and the non-null captions carried alongside.
struct VideoFrame {
    idx: u8,
    rt: u64,
    anc: Vec<u8>,
    captions: Vec<String>,
}

/// Pull recovered video frames (with recombined ancillary + captions) until the
/// last frame arrives or the deadline passes.
fn pull_video(sink: &gst_app::AppSink) -> Vec<VideoFrame> {
    let last = (NUM_FRAMES - 1) as u8;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let timeout = gst::ClockTime::from_seconds(2);
    let mut parser = cdp_types::CDPParser::new();
    let mut out = Vec::new();
    let mut seen_last = false;
    while !seen_last && std::time::Instant::now() < deadline {
        let Some(sample) = sink.try_pull_sample(timeout) else {
            if sink.is_eos() {
                break;
            }
            continue;
        };
        let Some(rt) = running_time(&sample) else {
            continue;
        };
        let buffer = sample.buffer().expect("video buffer");
        let idx = buffer.map_readable().expect("video readable").as_slice()[0];
        let anc = gstavsynctest::analyze::ancillary_indices(buffer, gstavsynctest::signal::ANC_DID);
        let captions = gstavsynctest::analyze::caption_cdps(buffer)
            .iter()
            .filter_map(|cdp| gstavsynctest::analyze::decode_caption(&mut parser, cdp))
            .collect();
        seen_last = idx == last;
        out.push(VideoFrame {
            idx,
            rt: rt.nseconds(),
            anc,
            captions,
        });
    }
    out
}

/// Drain `(running_time_ns, amplitude)` for every audio sample until `done` or
/// EOS/deadline (see gst-mxl-rs av_sync for why we drain continuously).
fn drain_audio(sink: &gst_app::AppSink, done: &AtomicBool) -> Vec<(u64, f32)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let timeout = gst::ClockTime::from_mseconds(200);
    let mut out = Vec::new();
    while std::time::Instant::now() < deadline {
        let Some(sample) = sink.try_pull_sample(timeout) else {
            if sink.is_eos() || done.load(Ordering::Relaxed) {
                break;
            }
            continue;
        };
        let Some(rt) = running_time(&sample) else {
            continue;
        };
        let base = rt.nseconds();
        let buffer = sample.buffer().expect("audio buffer");
        let map = buffer.map_readable().expect("audio readable");
        let body = gstavsynctest::analyze::f32le_samples(map.as_slice());
        for (i, &v) in body.iter().enumerate() {
            let t = base + i as u64 * gst::ClockTime::SECOND.nseconds() / RATE as u64;
            out.push((t, v));
        }
    }
    out
}

/// Expected TICK/TOCK for the bar-centre frame at index `idx` (`idx == k *
/// FRAMES_PER_INTERVAL`, `k >= 1`): odd pip -> TICK, even -> TOCK.
fn expected_caption(idx: u8) -> &'static str {
    let k = idx as u64 / FRAMES_PER_INTERVAL;
    if k % 2 == 1 {
        gstavsynctest::captions::CC_TICK
    } else {
        gstavsynctest::captions::CC_TOCK
    }
}

/// The recovered streams stay in lockstep. `anc_tol` frames is how far an
/// ancillary packet may sit from its home video frame: zero for the co-timed
/// transports (MXL), one for the software RTP path, whose arrival-time PTS shifts
/// ancillary onto an adjacent frame — which shows up as a frame that collected
/// two packets ("double") next to one that collected none ("none").
///
/// 1. The video Receiver attaches within `ATTACH_SLACK` of the head and then
///    reads contiguously through to the last frame — no gaps, no duplicates.
/// 2. `st2038combiner` conserves the data flow's ancillary: no frame carries more
///    than `anc_tol + 1` packets, every received frame-index packet lands on
///    exactly one video frame within `anc_tol` of its own index, and the received
///    indices are gap-free from the attach point through the last frame (so no
///    packet is lost or duplicated — a "double" is always matched by a "none").
/// 3. Captions ride that same ancillary: a frame's non-null captions are exactly
///    the phase-locked TICK/TOCK expected for the bar-centre packet indices it
///    carries (so a "double" straddling a bar-centre carries that caption too).
/// 4. Every bar-centre frame the audio spans has a coincident pip to within one
///    frame period (see gst-mxl-rs's av_sync for the A/V rationale).
fn assert_synchronised(video: &[VideoFrame], pips: &[u64], anc_tol: usize) {
    assert!(!pips.is_empty(), "no audio pips detected");
    let (first_pip, last_pip) = (*pips.first().unwrap(), *pips.last().unwrap());

    if std::env::var("NVNMOS_TEST_DUMP").is_ok() {
        eprintln!("--- video frames (idx, rt_ms, anc, captions) ---");
        for f in video {
            let mark = match f.anc.len() {
                0 => "  none",
                1 => "",
                _ => "DOUBLE",
            };
            eprintln!(
                "  idx={:3} rt={:6}ms anc={:?} {:?} {mark}",
                f.idx,
                f.rt / 1_000_000,
                f.anc,
                f.captions,
            );
        }
        eprintln!(
            "--- pips (ms) --- {:?}",
            pips.iter().map(|p| p / 1_000_000).collect::<Vec<_>>()
        );
    }

    // 1. Contiguous video capture.
    let frames: std::collections::BTreeSet<u8> = video.iter().map(|f| f.idx).collect();
    assert_eq!(
        frames.len(),
        video.len(),
        "duplicate video frames in capture"
    );
    let first = *frames.iter().next().expect("no video frames") as usize;
    let last = *frames.iter().next_back().unwrap() as usize;
    assert_eq!(
        last,
        NUM_FRAMES as usize - 1,
        "video did not read through the last frame {}, got {last}",
        NUM_FRAMES - 1
    );
    assert!(
        first <= ATTACH_SLACK,
        "video missed too many frames at attach: first read frame_idx {first}"
    );
    assert_eq!(
        frames.len(),
        last - first + 1,
        "video capture has gaps: {} frames over range {first}..={last}",
        frames.len()
    );

    // 2. Ancillary conserved: every received packet lands on exactly one frame
    //    within `anc_tol`, no frame over `anc_tol + 1` packets, no interior gaps.
    let max_per_frame = anc_tol + 1;
    let overloaded: Vec<(u8, &Vec<u8>)> = video
        .iter()
        .filter(|f| f.anc.len() > max_per_frame)
        .map(|f| (f.idx, &f.anc))
        .collect();
    assert!(
        overloaded.is_empty(),
        "frames carried more than {max_per_frame} ancillary packets (frame, anc): {overloaded:?}"
    );
    let mut placement: std::collections::BTreeMap<u8, Vec<u8>> = std::collections::BTreeMap::new();
    for f in video {
        for &a in &f.anc {
            placement.entry(a).or_default().push(f.idx);
        }
    }
    let misregistered: Vec<(u8, &Vec<u8>)> = placement
        .iter()
        .filter(|(a, on)| {
            on.len() != 1 || (on[0] as i64 - **a as i64).unsigned_abs() as usize > anc_tol
        })
        .map(|(a, on)| (*a, on))
        .collect();
    assert!(
        misregistered.is_empty(),
        "ancillary mis-registered beyond +/-{anc_tol} frame (anc -> frames): {misregistered:?}"
    );
    let recv_first = *placement.keys().next().expect("no ancillary received") as usize;
    let recv_last = *placement.keys().next_back().unwrap() as usize;
    assert!(
        recv_first <= ATTACH_SLACK,
        "ancillary attach missed too many at head: first received index {recv_first}"
    );
    assert!(
        recv_last >= last - anc_tol,
        "ancillary lost at the tail: last received index {recv_last}, video read to {last}"
    );
    assert_eq!(
        placement.len(),
        recv_last - recv_first + 1,
        "ancillary packets missing in range {recv_first}..={recv_last}: received {:?}",
        placement.keys().collect::<Vec<_>>()
    );

    // 3. Captions ride the recombined ancillary, so hold them to the same
    //    conservation contract as the index packets (§2): each bar-centre's
    //    TICK/TOCK is received exactly once, on a frame within `anc_tol` of that
    //    bar-centre. A caption CDP is just another ST-2038 packet, so UDP jitter
    //    slides it the same way — and by the same amount — as the frame-index
    //    packet it was emitted with. Attribute every received caption to the
    //    bar-centre whose expected text it matches within that window; one that
    //    matches none is a genuinely wrong or misplaced caption.
    let mut cap_placement: std::collections::BTreeMap<u8, Vec<u8>> =
        std::collections::BTreeMap::new();
    for f in video {
        for c in &f.captions {
            let target = ((f.idx as i64 - anc_tol as i64).max(0)
                ..=(f.idx as i64 + anc_tol as i64))
                .find_map(|i| {
                    let i = i as u8;
                    (i != 0
                        && (i as i32) < NUM_FRAMES
                        && (i as u64) % FRAMES_PER_INTERVAL == 0
                        && expected_caption(i) == c.as_str())
                    .then_some(i)
                });
            match target {
                Some(t) => cap_placement.entry(t).or_default().push(f.idx),
                None => panic!(
                    "frame {} carried caption {c:?} with no correct bar-centre \
                     within +/-{anc_tol} (anc {:?})",
                    f.idx, f.anc,
                ),
            }
        }
    }
    let cap_misregistered: Vec<(u8, &Vec<u8>)> = cap_placement
        .iter()
        .filter(|(b, on)| {
            on.len() != 1 || (on[0] as i64 - **b as i64).unsigned_abs() as usize > anc_tol
        })
        .map(|(b, on)| (*b, on))
        .collect();
    assert!(
        cap_misregistered.is_empty(),
        "captions mis-registered beyond +/-{anc_tol} frame (bar-centre -> frames): {cap_misregistered:?}"
    );
    let cap_first = *cap_placement.keys().next().expect("no captions received") as usize;
    let cap_last = *cap_placement.keys().next_back().unwrap() as usize;
    let fpi = FRAMES_PER_INTERVAL as usize;
    assert!(
        cap_first <= fpi + ATTACH_SLACK,
        "captions missed too many bar-centres at head: first received {cap_first}"
    );
    let last_bc = (last / fpi) * fpi;
    if last_bc + anc_tol <= last {
        assert!(
            cap_last >= last_bc,
            "captions lost at the tail: last received bar-centre {cap_last}, \
             video read to {last} (last bar-centre {last_bc})"
        );
    }
    assert_eq!(
        cap_placement.len(),
        (cap_last - cap_first) / fpi + 1,
        "bar-centre captions missing in {cap_first}..={cap_last}: received {:?}",
        cap_placement.keys().collect::<Vec<_>>()
    );

    // 4. A/V: every bar-centre frame the audio spans has a coincident pip.
    let mut in_span = 0;
    for frame in video
        .iter()
        .filter(|f| (f.idx as u64) != 0 && (f.idx as u64) % FRAMES_PER_INTERVAL == 0)
    {
        if frame.rt + FRAME_PERIOD_NS < first_pip || frame.rt > last_pip + FRAME_PERIOD_NS {
            continue;
        }
        in_span += 1;
        let nearest = pips
            .iter()
            .copied()
            .min_by_key(|p| p.abs_diff(frame.rt))
            .expect("pip present");
        let d = nearest.abs_diff(frame.rt);
        assert!(
            d <= FRAME_PERIOD_NS,
            "bar-centre frame {} (rt {}): nearest pip {nearest} is {d} ns away ({}) — pips: {pips:?}",
            frame.idx,
            frame.rt,
            if d < PIP_INTERVAL_NS / 2 {
                "over one frame period: A/V sync error"
            } else {
                "no pip near this frame: audio round-trip dropped samples"
            },
        );
    }

    let expected = (NUM_FRAMES as u64 / FRAMES_PER_INTERVAL) as usize;
    assert!(
        in_span >= expected - 2,
        "audio spanned only {in_span} of ~{expected} bar-centre frames (pips: {pips:?})",
    );
}

fn skip_reason(t: Transport) -> Option<String> {
    match t {
        // The software RTP receive path is `udpsrc ! rtp<essence>depay` with no
        // rtpjitterbuffer (see gst-nmos-rs `inner.rs`), so each flow's PTS comes
        // from packet arrival time, not its RTP timestamp; the video and data
        // flows land a few ms apart, which shifts a frame's ancillary group by up
        // to one frame (a frame that collects two packets next to one that
        // collects none). The test tolerates that (`anc_tolerance_frames`) —
        // every packet is still received within +/-1 frame, caption riding with
        // its index. Exact pairing needs an RFC 7273-sync jitterbuffer on a
        // shared PTP clock; MXL gets it for free (one grain-index→PTS map).
        Transport::Udp => {}
        Transport::NvdsUdp => {
            return Some(
                "nvdsudp needs the DeepStream/Rivermax + PTP runtime (hardware \
                 timestamping) to co-time the flows; not available here"
                    .into(),
            )
        }
        Transport::Mxl => {
            if !cfg!(target_os = "linux") {
                return Some("MXL domains use /dev/shm (Linux only)".into());
            }
            if !std::path::Path::new("/dev/shm").is_dir() {
                return Some("/dev/shm is not available".into());
            }
        }
    }
    if let Some(why) = nvnmosd_skip_reason() {
        return Some(why);
    }
    let mut needed: Vec<&str> = vec![
        "appsink",
        "audioconvert",
        "queue",
        "st2038extractor",
        "st2038combiner",
        "nmossink",
        "nmossrc",
        "avsyncvideotestsrc",
        "avsyncaudiotestsrc",
    ];
    needed.extend_from_slice(t.transport_factories());
    let missing: Vec<&str> = needed
        .into_iter()
        .filter(|n| gst::ElementFactory::find(n).is_none())
        .collect();
    if !missing.is_empty() {
        return Some(format!("missing element factories: {missing:?}"));
    }
    None
}

fn run_case(t: Transport) {
    init();
    register_sources();
    if let Some(why) = skip_reason(t) {
        skip!(why);
    }

    let domain_guard = (t == Transport::Mxl).then(|| TestDomainGuard::new(t.nmos_name()));
    let domain = domain_guard.as_ref().map(|d| d.path()).unwrap_or("");
    let nic = nic_ip();

    let pid = std::process::id();
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = PathBuf::from(format!("/tmp/nvnmos_avsync_{pid}_{now_nanos}.sock"));
    let _daemon = DaemonGuard::new(socket.clone());
    let uri = _daemon.uri();

    let producer = build_producer(t, &uri, &nic, domain);
    let consumer = build_consumer(t, &uri, &nic, domain);
    struct Teardown(gst::Pipeline, gst::Pipeline);
    impl Drop for Teardown {
        fn drop(&mut self) {
            let _ = self.0.set_state(gst::State::Null);
            let _ = self.1.set_state(gst::State::Null);
        }
    }
    let _teardown = Teardown(producer.clone(), consumer.clone());

    // Producer first so all three flows exist before the consumer attaches.
    producer.set_state(gst::State::Playing).expect("producer Playing");
    consumer.set_state(gst::State::Playing).expect("consumer Playing");
    let (res, _, _) = consumer.state(gst::ClockTime::from_seconds(10));
    res.expect("consumer reached Playing");

    let video_sink = appsink(&consumer, "video_sink");
    let audio_sink = appsink(&consumer, "audio_sink");
    let data_tap = std::env::var("NVNMOS_TEST_DATA_TAP")
        .ok()
        .map(|_| appsink(&consumer, "data_sink"));
    let done = AtomicBool::new(false);
    let (video, audio) = std::thread::scope(|s| {
        let audio = s.spawn(|| drain_audio(&audio_sink, &done));
        let data = data_tap.as_ref().map(|sink| {
            s.spawn(|| {
                let mut idx = Vec::new();
                while !done.load(Ordering::Relaxed) {
                    let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(200))
                    else {
                        if sink.is_eos() {
                            break;
                        }
                        continue;
                    };
                    let rt = running_time(&sample).map(|t| t.nseconds()).unwrap_or(0);
                    let buffer = sample.buffer().expect("data buffer");
                    let map = buffer.map_readable().expect("data readable");
                    idx.push((st2038_first_index(map.as_slice()), rt));
                }
                idx
            })
        });
        let video = pull_video(&video_sink);
        done.store(true, Ordering::Relaxed);
        let data = data.map(|d| d.join().expect("data drain thread"));
        if let Some(data) = data {
            eprintln!("--- raw ST-2038 (idx, rt_ms) ({} buffers) ---", data.len());
            for (i, rt) in &data {
                eprintln!("  data idx={:?} rt={}ms", i, rt / 1_000_000);
            }
        }
        (video, audio.join().expect("audio drain thread"))
    });

    let pips = gstavsynctest::analyze::detect_pips(&audio);
    assert_synchronised(&video, &pips, t.anc_tolerance_frames() as usize);
}

#[test]
fn av_sync_via_mxl() {
    run_case(Transport::Mxl);
}

#[test]
fn av_sync_via_udp() {
    run_case(Transport::Udp);
}

#[test]
fn av_sync_via_nvdsudp() {
    run_case(Transport::NvdsUdp);
}
