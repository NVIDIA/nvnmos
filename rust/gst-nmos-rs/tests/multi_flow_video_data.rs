// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end integration test for the canonical multi-flow NMOS shape:
//! a single Node with two Senders (v210 video + ST 2038 ancillary data)
//! and the matching two Receivers reading back from the same MXL Domain.
//!
//! Ported from `mxl/rust/gst-mxl-rs/tests/video_data_sync.rs::v210_with_meta_to_v210_and_st2038_via_mxl`.
//! That test runs `appsrc → st2038extractor → 2× mxlsink` against
//! `2× mxlsrc → 2× appsink`; this NMOS port substitutes `nmossink` /
//! `nmossrc` and adds an `nvnmosd` daemon spawned as a child process.
//! The producer/consumer pipeline shapes, frame-index stamping, and
//! disjoint per-`frame_idx` validation are unchanged.
//!
//! The test self-gates: it runs automatically when the full MXL toolchain
//! is present and otherwise prints a skip reason and returns (so a checkout
//! without MXL — e.g. CI today — neither fails nor is silently `#[ignore]`d).
//! It needs:
//! * `nvnmosd` built against `libnvnmos.so` (set `NVNMOSD_BIN` or have
//!   `<workspace>/rust/target/{debug,release}/nvnmosd` present);
//! * `libgstnmos.so` + gst-mxl-rs's `libgstmxl.so` reachable via
//!   `GST_PLUGIN_PATH`;
//! * `libnvnmos.so` + `libmxl.so` reachable via `LD_LIBRARY_PATH`;
//! * `/dev/shm` writable (Linux-only, mirrors the gst-mxl-rs test).
//!
//! Run with:
//!
//! ```bash
//! NVNMOSD_BIN=$TARGET_DIR/debug/nvnmosd \
//! GST_PLUGIN_PATH=$TARGET_DIR/debug:$MXL_PLUGIN_DIR \
//! LD_LIBRARY_PATH=$NVNMOS_LIB_DIR/lib:$MXL_RT_LIB_DIR \
//! cargo test --manifest-path rust/Cargo.toml -p gst-nmos-rs \
//!   --test multi_flow_video_data -- --test-threads=1 --nocapture
//! ```

mod common;

use common::init;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use test_skip::skip;

// Producer cadence — matches gst-mxl-rs. 30000/1001 ≈ 29.97 fps.
const FRAMERATE_NUM: i32 = 30_000;
const FRAMERATE_DEN: i32 = 1_001;
const FRAME_PERIOD_NS: u64 =
    gst::ClockTime::SECOND.nseconds() * FRAMERATE_DEN as u64 / FRAMERATE_NUM as u64;
// v210 pads lines to a multiple of 128 bytes; width >= 2 is the
// smallest valid frame and keeps each push trivially cheap.
const VIDEO_WIDTH: u32 = 2;
const VIDEO_HEIGHT: u32 = 2;

// NMOS identifiers shared across the test. Static (per-process) is
// fine because each test invocation spawns its own daemon + scratch
// `/dev/shm` domain so collisions across runs are impossible.
const NODE_SEED: &str = "nvnmos-gst-test";
const PRODUCER_VIDEO_NAME: &str = "video-sender";
const PRODUCER_DATA_NAME: &str = "data-sender";
const CONSUMER_VIDEO_NAME: &str = "video-receiver";
const CONSUMER_DATA_NAME: &str = "data-receiver";
const VIDEO_FLOW_ID: &str = "00000000-0000-0000-0000-000000000001";
const DATA_FLOW_ID: &str = "00000000-0000-0000-0000-000000000002";
const DOMAIN_ID: &str = "11111111-2222-3333-4444-555555555555";

// First-sample timeout absorbs the daemon spawn + plugin load +
// MXL flow allocation latency; once the producer is steady,
// subsequent samples arrive at the frame period and a much tighter
// timeout still catches a stuck consumer mid-stream.
const FIRST_SAMPLE_TIMEOUT_MS: u64 = 5_000;
const STEADY_SAMPLE_TIMEOUT_MS: u64 = 500;

/// Element factories the end-to-end test needs, annotated with the plugin each
/// comes from (and the library that plugin links), so a found factory confirms
/// that plugin and its lib dependency are loaded:
/// * `appsrc` / `appsink` / `queue` — core GStreamer;
/// * `st2038extractor` — gst-plugins-rs `libgstrsclosedcaption.so`;
/// * `nmossink` / `nmossrc` — this workspace's `libgstnmos.so` (links `libnvnmos.so`);
/// * `mxlsrc` / `mxlsink` — gst-mxl-rs's `libgstmxl.so` (links `libmxl.so`); not
///   created directly here but built internally by `nmossink`/`nmossrc` at runtime.
const REQUIRED_FACTORIES: &[&str] = &[
    "appsrc",
    "appsink",
    "queue",
    "st2038extractor",
    "nmossink",
    "nmossrc",
    "mxlsrc",
    "mxlsink",
];

/// Reason the MXL-backed end-to-end test cannot run, or `None` when every
/// prerequisite is present. Mirrors the self-gating of the other daemon-backed
/// integration tests: an environment without the MXL toolchain (e.g. CI today)
/// skips rather than fails. Assumes `gst::init()` has run, for the factory probe.
fn skip_reason() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("MXL domains use /dev/shm (Linux only)".into());
    }
    if !Path::new("/dev/shm").is_dir() {
        return Some("/dev/shm is not available".into());
    }
    let bin = nvnmosd_bin();
    if !bin.exists() {
        return Some(format!("nvnmosd not built at `{}`", bin.display()));
    }
    if libnvnmos_dir().is_none() {
        return Some("libnvnmos.so not found via LD_LIBRARY_PATH or NVNMOS_LIB_DIR".into());
    }
    let missing: Vec<&str> = REQUIRED_FACTORIES
        .iter()
        .filter(|n| gst::ElementFactory::find(n).is_none())
        .copied()
        .collect();
    if !missing.is_empty() {
        return Some(format!(
            "missing GStreamer factories {missing:?}; set `GST_PLUGIN_PATH` to `libgstnmos.so` \
             + gst-mxl-rs's `libgstmxl.so`, and `LD_LIBRARY_PATH` to `libnvnmos.so` + `libmxl.so`"
        ));
    }
    None
}

/// Locate the `nvnmosd` binary. Prefer the `NVNMOSD_BIN` env var
/// (lets callers point at a specific build) and otherwise look in
/// `${CARGO_TARGET_DIR:-<manifest>/../target}/{debug,release}/nvnmosd`.
fn nvnmosd_bin() -> PathBuf {
    if let Ok(p) = std::env::var("NVNMOSD_BIN") {
        return PathBuf::from(p);
    }
    let target_dir = std::env::var("CARGO_TARGET_DIR").ok().unwrap_or_else(|| {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
        // .../rust/gst-nmos-rs -> .../rust/target
        PathBuf::from(manifest)
            .parent()
            .expect("manifest parent")
            .join("target")
            .to_string_lossy()
            .into_owned()
    });
    let debug = PathBuf::from(&target_dir).join("debug").join("nvnmosd");
    if debug.exists() {
        return debug;
    }
    PathBuf::from(target_dir).join("release").join("nvnmosd")
}

fn libnvnmos_dir() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(paths) = std::env::var("LD_LIBRARY_PATH") {
        dirs.extend(paths.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    }
    if let Ok(dir) = std::env::var("NVNMOS_LIB_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    dirs.into_iter().find(|d| d.join("libnvnmos.so").exists())
}

/// Spawns `nvnmosd` as a child process and kills it on drop. Polls
/// the UDS socket for up to 5s so the test only proceeds once the
/// gRPC service is actually accepting connections.
struct DaemonGuard {
    child: Child,
    socket: PathBuf,
}

impl DaemonGuard {
    fn new(socket: PathBuf) -> Self {
        let bin = nvnmosd_bin();
        assert!(
            bin.exists(),
            "nvnmosd binary not found at `{}`; build it with `cargo build -p nvnmosd` \
             or set NVNMOSD_BIN to a built binary path",
            bin.display(),
        );

        let _ = std::fs::remove_file(&socket);
        let mut command = Command::new(&bin);
        command
            .arg("--uds")
            .arg(&socket)
            .env("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // nvnmosd links libnvnmos.so; surface NVNMOS_LIB_DIR to the loader even
        // when the caller set only it (not LD_LIBRARY_PATH).
        if let Ok(lib_dir) = std::env::var("NVNMOS_LIB_DIR") {
            let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
            let value = if existing.is_empty() {
                lib_dir
            } else {
                format!("{lib_dir}:{existing}")
            };
            command.env("LD_LIBRARY_PATH", value);
        }
        let mut child = command
            .spawn()
            .unwrap_or_else(|e| panic!("spawn `{}`: {e}", bin.display()));

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if socket.exists() {
                return Self { child, socket };
            }
            thread::sleep(Duration::from_millis(50));
        }
        // Reap the runaway child so clippy's zombie-processes lint
        // is satisfied and so we don't leave a stray nvnmosd behind
        // when the test is about to panic.
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "nvnmosd UDS `{}` did not appear within 5s; check it built against libnvnmos \
             and that LD_LIBRARY_PATH includes the libnvnmos lib dir",
            socket.display(),
        );
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Per-test MXL Domain under `/dev/shm`, removed on drop. Mirrors
/// `gst-mxl-rs::tests::video_data_sync::TestDomainGuard` and adds a
/// BCP-007-03 `domain_def.json` so `nmossink`/`nmossrc`'s
/// mxl-domain-id cross-check passes.
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
        let dir = PathBuf::from(format!("/dev/shm/nvnmos_gst_test_{test}_{pid}_{now_nanos}"));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("create test domain `{}`: {e}", dir.display()));

        let domain_def = serde_json::json!({
            "id": DOMAIN_ID,
            "label": format!("nvnmos gst test {test}"),
            "description": "ephemeral test domain (auto-removed when the test ends)"
        });
        std::fs::write(
            dir.join("domain_def.json"),
            serde_json::to_string_pretty(&domain_def).expect("serialise domain_def"),
        )
        .unwrap_or_else(|e| panic!("write domain_def.json: {e}"));

        Self { dir }
    }

    fn path(&self) -> &str {
        self.dir
            .to_str()
            .expect("test domain path is ASCII (we built it from PID + nanos)")
    }
}

impl Drop for TestDomainGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Configuring transport file (flow_def.json) for a v210 video Sender
/// or Receiver. The daemon rejects a session whose `urn:x-nvnmos:tag:name`
/// doesn't match the claimed `sender-name`/`receiver-name`, so each
/// role gets its own file.
fn write_video_flow_file(out: &Path, name: &str) {
    let value = serde_json::json!({
        "id": VIDEO_FLOW_ID,
        "label": format!("nvnmos gst test {name}"),
        "description": format!("v210 {VIDEO_WIDTH}x{VIDEO_HEIGHT} {FRAMERATE_NUM}/{FRAMERATE_DEN}"),
        "tags": {
            "urn:x-nvnmos:tag:name": [name],
            "urn:x-nvnmos:tag:mxl-domain-id": [DOMAIN_ID],
        },
        "format": "urn:x-nmos:format:video",
        "media_type": "video/v210",
        "grain_rate": { "numerator": FRAMERATE_NUM, "denominator": FRAMERATE_DEN },
        "frame_width": VIDEO_WIDTH,
        "frame_height": VIDEO_HEIGHT,
        "interlace_mode": "progressive",
        "colorspace": "BT709",
        "transfer_characteristic": "SDR",
        "components": [
            { "name": "Y",  "width": VIDEO_WIDTH,     "height": VIDEO_HEIGHT, "bit_depth": 10 },
            { "name": "Cb", "width": VIDEO_WIDTH / 2, "height": VIDEO_HEIGHT, "bit_depth": 10 },
            { "name": "Cr", "width": VIDEO_WIDTH / 2, "height": VIDEO_HEIGHT, "bit_depth": 10 },
        ],
    });
    std::fs::write(out, serde_json::to_string_pretty(&value).expect("serialise flow_def"))
        .unwrap_or_else(|e| panic!("write `{}`: {e}", out.display()));
}

/// Configuring transport file for an ST 2038 ancillary Sender or
/// Receiver. `meta/x-st-2038` carries `framerate` in GStreamer caps;
/// the matching flow_def carries it as `grain_rate`.
fn write_data_flow_file(out: &Path, name: &str) {
    let value = serde_json::json!({
        "id": DATA_FLOW_ID,
        "label": format!("nvnmos gst test {name}"),
        "description": "ST 2038 ancillary metadata",
        "tags": {
            "urn:x-nvnmos:tag:name": [name],
            "urn:x-nvnmos:tag:mxl-domain-id": [DOMAIN_ID],
        },
        "format": "urn:x-nmos:format:data",
        "media_type": "video/smpte291",
        "grain_rate": { "numerator": FRAMERATE_NUM, "denominator": FRAMERATE_DEN },
    });
    std::fs::write(out, serde_json::to_string_pretty(&value).expect("serialise flow_def"))
        .unwrap_or_else(|e| panic!("write `{}`: {e}", out.display()));
}

// ----- ANC helpers (copied verbatim from gst-mxl-rs's video_data_sync.rs) -----

fn extend_with_even_odd_parity(v: u8) -> u16 {
    if v.count_ones() & 1 == 0 {
        0x1_00 | (v as u16)
    } else {
        0x2_00 | (v as u16)
    }
}

fn compute_checksum(did_10bit: u16, sdid_10bit: u16, dc_10bit: u16, data: &[u16]) -> u16 {
    let mut checksum = 0u16;
    checksum = checksum.wrapping_add(did_10bit & 0x1ff);
    checksum = checksum.wrapping_add(sdid_10bit & 0x1ff);
    checksum = checksum.wrapping_add(dc_10bit & 0x1ff);
    for &w in data {
        checksum = checksum.wrapping_add(w & 0x1ff);
    }
    checksum &= 0x1ff;
    checksum |= ((!(checksum >> 8)) & 0x01) << 9;
    checksum
}

fn add_ancillary_meta(
    buffer: &mut gst::BufferRef,
    line: u16,
    offset: u16,
    did: u8,
    sdid: u8,
    payload: &[u8],
) {
    let mut meta = gst_video::video_meta::AncillaryMeta::add(buffer);
    meta.set_c_not_y_channel(false);
    meta.set_line(line);
    meta.set_offset(offset);

    let did_10bit = extend_with_even_odd_parity(did);
    let sdid_10bit = extend_with_even_odd_parity(sdid);
    let dc_10bit = extend_with_even_odd_parity(payload.len() as u8);

    meta.set_did(did_10bit);
    meta.set_sdid_block_number(sdid_10bit);

    let data: Vec<u16> = payload
        .iter()
        .copied()
        .map(extend_with_even_odd_parity)
        .collect();
    meta.set_checksum(compute_checksum(did_10bit, sdid_10bit, dc_10bit, &data));
    meta.set_data(gst::glib::Slice::from(data));
}

/// Recover the original byte from a 10-bit-with-even/odd-parity ANC
/// word. Inverse of [`extend_with_even_odd_parity`].
fn ancillary_byte(word_10bit: u16) -> u8 {
    (word_10bit & 0xff) as u8
}

/// Read the first ST 2038 ANC packet's first user-data byte. We do
/// the bit unpacking manually here so the test doesn't depend on the
/// gst-mxl-rs crate (which lives in a sibling repo and isn't a
/// workspace member). Field layout (SMPTE ST 2038):
/// ```text
/// 6 bits  : zero
/// 1 bit   : C-not-Y
/// 11 bits : line number
/// 12 bits : horizontal offset
/// 10 bits : DID
/// 10 bits : SDID / DBN
/// 10 bits : data count
/// N×10    : user data words  <-- we want word 0's low byte
/// 10 bits : checksum
/// ```
/// First user data word starts at bit 60. The low 8 bits of that
/// 10-bit word are the marker we stamped in [`make_test_frame`].
fn st2038_first_packet_data0(st2038: &[u8]) -> u8 {
    fn read_bits(data: &[u8], bit_offset: usize, width: usize) -> u32 {
        // BigEndian bit order, MSB-first within each byte.
        let mut out: u32 = 0;
        for i in 0..width {
            let bit = bit_offset + i;
            let byte = data[bit / 8];
            let shift = 7 - (bit % 8);
            out = (out << 1) | (((byte >> shift) & 1) as u32);
        }
        out
    }
    let udw0 = read_bits(st2038, 60, 10) as u16;
    ancillary_byte(udw0)
}

// ----- producer / consumer pipeline construction -----

/// `parse::launch` the shared producer (`appsrc → st2038extractor →
/// 2× nmossink`) and return the pipeline, its appsrc, and the v210
/// frame size in bytes. Mirrors `gst-mxl-rs`'s `build_producer`
/// with `mxlsink flow-id=...` replaced by `nmossink ... transport-file-path=...`.
fn build_producer(
    socket: &Path,
    domain_path: &str,
    video_flow_path: &Path,
    data_flow_path: &Path,
) -> (gst::Pipeline, gst_app::AppSrc, usize) {
    let video_info =
        gst_video::VideoInfo::builder(gst_video::VideoFormat::V210, VIDEO_WIDTH, VIDEO_HEIGHT)
            .fps(gst::Fraction::new(FRAMERATE_NUM, FRAMERATE_DEN))
            .build()
            .expect("v210 VideoInfo");
    let frame_bytes = video_info.size();

    let socket_uri = format!("unix:{}", socket.display());
    let video_flow_path = video_flow_path.display();
    let data_flow_path = data_flow_path.display();
    let producer_desc = format!(
        "appsrc name=src format=time block=true max-buffers=2 \
           caps=video/x-raw,format=v210,\
                width={VIDEO_WIDTH},\
                height={VIDEO_HEIGHT},\
                framerate={FRAMERATE_NUM}/{FRAMERATE_DEN} \
           ! st2038extractor name=ext remove-ancillary-meta=true \
         ext.src \
           ! queue max-size-buffers=2 \
           ! nmossink \
                daemon-uri=\"{socket_uri}\" \
                transport=mxl \
                node-seed={NODE_SEED} \
                sender-name={PRODUCER_VIDEO_NAME} \
                auto-activate=true \
                mxl-domain-path={domain_path} \
                transport-file-path={video_flow_path} \
         ext.st2038 \
           ! queue max-size-buffers=2 \
           ! nmossink \
                daemon-uri=\"{socket_uri}\" \
                transport=mxl \
                node-seed={NODE_SEED} \
                sender-name={PRODUCER_DATA_NAME} \
                auto-activate=true \
                mxl-domain-path={domain_path} \
                transport-file-path={data_flow_path}"
    );
    let producer = gst::parse::launch(&producer_desc)
        .expect("parse producer")
        .downcast::<gst::Pipeline>()
        .expect("producer is a Pipeline");
    let appsrc = producer
        .by_name("src")
        .expect("appsrc")
        .downcast::<gst_app::AppSrc>()
        .expect("AppSrc downcast");
    (producer, appsrc, frame_bytes)
}

/// `parse::launch` the shared consumer (`2× nmossrc → 2× appsink`)
/// and return the pipeline plus both appsinks.
fn build_consumer(
    socket: &Path,
    domain_path: &str,
    video_flow_path: &Path,
    data_flow_path: &Path,
) -> (gst::Pipeline, gst_app::AppSink, gst_app::AppSink) {
    let socket_uri = format!("unix:{}", socket.display());
    let video_flow_path = video_flow_path.display();
    let data_flow_path = data_flow_path.display();
    let consumer_desc = format!(
        "nmossrc \
             daemon-uri=\"{socket_uri}\" \
             transport=mxl \
             node-seed={NODE_SEED} \
             receiver-name={CONSUMER_VIDEO_NAME} \
             auto-activate=true \
             mxl-domain-path={domain_path} \
             transport-file-path={video_flow_path} \
           ! queue \
           ! appsink name=video_sink sync=false \
                 caps=video/x-raw,format=v210 \
         nmossrc \
             daemon-uri=\"{socket_uri}\" \
             transport=mxl \
             node-seed={NODE_SEED} \
             receiver-name={CONSUMER_DATA_NAME} \
             auto-activate=true \
             mxl-domain-path={domain_path} \
             transport-file-path={data_flow_path} \
           ! queue \
           ! appsink name=data_sink sync=false \
                 caps=meta/x-st-2038,\
                      alignment=frame,\
                      framerate={FRAMERATE_NUM}/{FRAMERATE_DEN}"
    );
    let consumer = gst::parse::launch(&consumer_desc)
        .expect("parse consumer")
        .downcast::<gst::Pipeline>()
        .expect("consumer is a Pipeline");
    let video_appsink = consumer
        .by_name("video_sink")
        .expect("video appsink")
        .downcast::<gst_app::AppSink>()
        .expect("video AppSink downcast");
    let data_appsink = consumer
        .by_name("data_sink")
        .expect("data appsink")
        .downcast::<gst_app::AppSink>()
        .expect("data AppSink downcast");
    (consumer, video_appsink, data_appsink)
}

/// Build one v210 buffer for producer push `i`. Stamps the frame
/// index into byte 0 of the v210 payload **and** into byte 0 of every
/// ancillary payload, so the consumer can recover a producer frame
/// index from either flow and assert that the two flows expose the
/// same frame indices.
fn make_test_frame(frame_bytes: usize, i: u64) -> gst::Buffer {
    let pts = gst::ClockTime::from_nseconds(i * FRAME_PERIOD_NS);
    let mut buf = gst::Buffer::with_size(frame_bytes).expect("v210 buffer");
    let frame_idx = i as u8;
    {
        let b = buf.get_mut().expect("buffer mut");
        b.set_pts(pts);
        b.set_duration(gst::ClockTime::from_nseconds(FRAME_PERIOD_NS));
        {
            let mut map = b.map_writable().expect("buffer writable");
            map.as_mut_slice()[0] = frame_idx;
        }
        add_ancillary_meta(b, 9, 0, 0x44, 0x01, &[frame_idx, 0xa, 0xaa, 0x55]);
        add_ancillary_meta(b, 9, 32, 0x44, 0x02, &[frame_idx, 0xb, 0xaa, 0x55]);
    }
    buf
}

/// Handle for a background producer thread spawned by
/// [`start_pushing_test_frames`].
struct ProducerHandle {
    handle: thread::JoinHandle<u64>,
    stop: Arc<AtomicBool>,
}

/// Spawn a background thread that pushes paced v210+ANC buffers into
/// `appsrc` at the pinned frame rate until [`ProducerHandle::stop`] is
/// set or `push_buffer` fails (which happens when the pipeline
/// transitions to `Null` during teardown). The frame index marker is
/// a `u8`, so the thread caps itself at `MAX_FRAMES = 200` to keep
/// frame indices distinct over the test window.
fn start_pushing_test_frames(appsrc: &gst_app::AppSrc, frame_bytes: usize) -> ProducerHandle {
    const MAX_FRAMES: u64 = 200;
    let stop = Arc::new(AtomicBool::new(false));
    let appsrc = appsrc.clone();
    let stop_thread = Arc::clone(&stop);
    let handle = thread::spawn(move || -> u64 {
        let frame_period = Duration::from_nanos(FRAME_PERIOD_NS);
        let push_start = Instant::now();
        let mut i: u64 = 0;
        while !stop_thread.load(Ordering::Relaxed) && i < MAX_FRAMES {
            let buf = make_test_frame(frame_bytes, i);
            if appsrc.push_buffer(buf).is_err() {
                break;
            }
            i += 1;
            let next_deadline = push_start + frame_period * (i as u32);
            if let Some(remaining) = next_deadline.checked_duration_since(Instant::now()) {
                thread::sleep(remaining);
            }
        }
        i
    });
    ProducerHandle { handle, stop }
}

/// PTS and running time from a `gst::Sample`.
fn timing_from_sample(sample: &gst::SampleRef) -> (Option<gst::ClockTime>, Option<gst::ClockTime>) {
    let Some(buffer) = sample.buffer() else {
        return (None, None);
    };
    let pts = buffer.pts();
    let running_time = match (pts, sample.segment()) {
        (Some(pts), Some(seg)) => seg
            .downcast_ref::<gst::ClockTime>()
            .and_then(|clock_seg| clock_seg.to_running_time(pts)),
        _ => None,
    };
    (pts, running_time)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SampleTiming {
    pts: Option<gst::ClockTime>,
    running_time: Option<gst::ClockTime>,
    /// Frame index read out of v210 byte 0, or first ST 2038 ANC user-data byte 0.
    frame_idx: u8,
}

/// Pull exactly `n` samples from `sink`. Sample 0 gets
/// `first_timeout_ms` to absorb pipeline startup; subsequent samples
/// get `steady_timeout_ms`.
fn pull_n_samples(
    sink: &gst_app::AppSink,
    n: usize,
    first_timeout_ms: u64,
    steady_timeout_ms: u64,
    mut frame_from_buffer: impl FnMut(&gst::BufferRef) -> u8,
) -> Vec<SampleTiming> {
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let timeout_ms = if k == 0 { first_timeout_ms } else { steady_timeout_ms };
        let sample = sink
            .try_pull_sample(gst::ClockTime::from_mseconds(timeout_ms))
            .unwrap_or_else(|| {
                panic!(
                    "{}: sample {k} did not arrive within {timeout_ms}ms",
                    sink.name(),
                )
            });
        let buffer = sample.buffer().expect("sample buffer");
        let (pts, running_time) = timing_from_sample(&sample);
        out.push(SampleTiming {
            pts,
            running_time,
            frame_idx: frame_from_buffer(buffer),
        });
    }
    out
}

/// Disjoint-flow validation: assert that for every `frame_idx`
/// present on both the video and data sides, the per-frame
/// `(video.pts - data.pts)` and `(video.rt - data.rt)` differences
/// stay constant across frames (after a short warmup). See the
/// gst-mxl-rs original for the rationale on why an exact PTS match
/// would be too strict.
fn compare_disjoint_video_data(video: &[SampleTiming], data: &[SampleTiming]) {
    use std::collections::{BTreeMap, BTreeSet};

    let dump = |label: &str, samples: &[SampleTiming]| {
        eprintln!("{label} pull order ({} samples):", samples.len());
        for (k, s) in samples.iter().enumerate() {
            eprintln!(
                "  [{k:2}] frame_idx={:3} pts={:?} rt={:?}",
                s.frame_idx, s.pts, s.running_time
            );
        }
    };
    dump("video", video);
    dump("data", data);

    let mut video_by_frame = BTreeMap::new();
    for s in video {
        assert!(
            video_by_frame.insert(s.frame_idx, *s).is_none(),
            "duplicate video sample for frame_idx {}",
            s.frame_idx,
        );
    }
    let mut data_by_frame = BTreeMap::new();
    for s in data {
        assert!(
            data_by_frame.insert(s.frame_idx, *s).is_none(),
            "duplicate data sample for frame_idx {}",
            s.frame_idx,
        );
    }

    let video_keys: BTreeSet<_> = video_by_frame.keys().copied().collect();
    let data_keys: BTreeSet<_> = data_by_frame.keys().copied().collect();
    let common: BTreeSet<_> = video_keys.intersection(&data_keys).copied().collect();
    let only_video: Vec<_> = video_keys.difference(&data_keys).copied().collect();
    let only_data: Vec<_> = data_keys.difference(&video_keys).copied().collect();
    eprintln!(
        "disjoint flow: video {} sample(s), data {} sample(s), common {} frame_idx; \
         only-video {only_video:?}, only-data {only_data:?}",
        video.len(),
        data.len(),
        common.len(),
    );

    assert!(
        !common.is_empty(),
        "expected at least one frame_idx present on both video and data sides"
    );

    const WARMUP_FRAMES: usize = 3;
    assert!(
        common.len() > WARMUP_FRAMES,
        "need more than {WARMUP_FRAMES} common frames to assert PTS-gap stability; got {}",
        common.len(),
    );
    let steady: Vec<u8> = common.iter().copied().skip(WARMUP_FRAMES).collect();
    let pts_diff = |f: u8| -> i64 {
        let v = video_by_frame[&f].pts.expect("video pts").nseconds() as i64;
        let d = data_by_frame[&f].pts.expect("data pts").nseconds() as i64;
        v - d
    };
    let rt_diff = |f: u8| -> i64 {
        let v = video_by_frame[&f]
            .running_time
            .expect("video running_time")
            .nseconds() as i64;
        let d = data_by_frame[&f]
            .running_time
            .expect("data running_time")
            .nseconds() as i64;
        v - d
    };
    let tolerance_ns = (FRAME_PERIOD_NS / 10) as i64;
    let spread = |diffs: &[i64]| -> i64 {
        diffs.iter().copied().max().unwrap() - diffs.iter().copied().min().unwrap()
    };
    let pts_diffs: Vec<i64> = steady.iter().map(|&f| pts_diff(f)).collect();
    let rt_diffs: Vec<i64> = steady.iter().map(|&f| rt_diff(f)).collect();
    let pts_spread = spread(&pts_diffs);
    let rt_spread = spread(&rt_diffs);
    assert!(
        pts_spread <= tolerance_ns,
        "video/data PTS gap drifted by {pts_spread}ns across {} steady-state frame_idx; \
         expected <= {tolerance_ns}ns (1/10 of frame period); diffs={pts_diffs:?}",
        steady.len(),
    );
    assert!(
        rt_spread <= tolerance_ns,
        "video/data running_time gap drifted by {rt_spread}ns across {} steady-state frame_idx; \
         expected <= {tolerance_ns}ns; diffs={rt_diffs:?}",
        steady.len(),
    );
}

/// End-to-end test: `appsrc → st2038extractor → 2× nmossink` against
/// `2× nmossrc → 2× appsink`, with `nvnmosd` spawned as a child
/// process and a fresh `/dev/shm` MXL Domain. Validates that the
/// same producer-stamped frame index reaches both consumer appsinks
/// and that the per-frame PTS gap between the two flows stays
/// constant across the steady-state window.
#[test]
fn v210_with_meta_to_v210_and_st2038_via_nmos() {
    init();
    if let Some(why) = skip_reason() {
        skip!(why);
    }

    let domain_guard = TestDomainGuard::new("v210_with_meta");
    let domain_path = domain_guard.path().to_owned();

    let pid = std::process::id();
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = PathBuf::from(format!("/tmp/nvnmos_gst_test_{pid}_{now_nanos}.sock"));
    let _daemon = DaemonGuard::new(socket.clone());

    // One configuring flow_def per role per flow (4 files total).
    let work = tempfile::tempdir().expect("scratch tempdir");
    let producer_video_flow = work.path().join("producer_video.json");
    let producer_data_flow = work.path().join("producer_data.json");
    let consumer_video_flow = work.path().join("consumer_video.json");
    let consumer_data_flow = work.path().join("consumer_data.json");

    write_video_flow_file(&producer_video_flow, PRODUCER_VIDEO_NAME);
    write_data_flow_file(&producer_data_flow, PRODUCER_DATA_NAME);
    write_video_flow_file(&consumer_video_flow, CONSUMER_VIDEO_NAME);
    write_data_flow_file(&consumer_data_flow, CONSUMER_DATA_NAME);

    let (producer, appsrc, frame_bytes) = build_producer(
        &socket,
        &domain_path,
        &producer_video_flow,
        &producer_data_flow,
    );
    let (consumer, video_sink, data_sink) = build_consumer(
        &socket,
        &domain_path,
        &consumer_video_flow,
        &consumer_data_flow,
    );

    // Producer first so both MXL flows exist by the time the
    // consumer's mxlsrc readers attach.
    producer
        .set_state(gst::State::Playing)
        .expect("producer Playing");
    consumer
        .set_state(gst::State::Playing)
        .expect("consumer Playing");

    let producer_handle = start_pushing_test_frames(&appsrc, frame_bytes);

    const PULL_COUNT: usize = 30;

    let video_samples = pull_n_samples(
        &video_sink,
        PULL_COUNT,
        FIRST_SAMPLE_TIMEOUT_MS,
        STEADY_SAMPLE_TIMEOUT_MS,
        |buf| {
            assert_eq!(
                buf.size(),
                frame_bytes,
                "v210 round-trip should preserve frame size"
            );
            let map = buf.map_readable().expect("video buffer readable");
            map.as_slice()[0]
        },
    );
    let data_samples = pull_n_samples(
        &data_sink,
        PULL_COUNT,
        FIRST_SAMPLE_TIMEOUT_MS,
        STEADY_SAMPLE_TIMEOUT_MS,
        |buf| {
            assert!(
                buf.size() > 0,
                "ST 2038 round-trip buffer should be non-empty"
            );
            let map = buf.map_readable().expect("data buffer readable");
            st2038_first_packet_data0(map.as_slice())
        },
    );

    // Tear down the consumer first while the producer is still
    // pushing (matches gst-mxl-rs); then stop and tear down the
    // producer.
    consumer.set_state(gst::State::Null).expect("consumer Null");
    producer_handle.stop.store(true, Ordering::Relaxed);
    producer.set_state(gst::State::Null).expect("producer Null");
    let _pushed = producer_handle
        .handle
        .join()
        .expect("producer thread did not panic");

    compare_disjoint_video_data(&video_samples, &data_samples);
}
