// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! IS-08 audio channel map integration tests: lockstep tones, static
//! `active-map` isolation from the first measure window, live IS-08 re-route,
//! and channel-count inference / mismatch at fixation.
//!
//! ```bash
//! LD_LIBRARY_PATH=$NVNMOS_LIB_DIR \
//! cargo test --manifest-path rust/Cargo.toml -p gst-nmos-rs \
//!   --test is08_audio_channelmap -- --test-threads=1 --nocapture
//! ```

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::str::FromStr;
use std::time::{Duration, Instant};

use common::{
    A4_HZ, DaemonGuard, Tone, init, nvnmosd_skip_reason, perfect_fifth_hz, require_factories,
};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use test_skip::skip;

const SAMPLE_RATE: i32 = 48_000;
const FRAME_SAMPLES: usize = 480; // 10 ms
const MEASURE_FRAMES: usize = 20; // 200 ms

fn audio_caps(channels: i32) -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("rate", SAMPLE_RATE)
        .field("channels", channels)
        .field("layout", "interleaved")
        .build()
}

fn make_appsrc(name: &str, channels: i32) -> gst_app::AppSrc {
    gst::ElementFactory::make("appsrc")
        .name(name)
        .property("format", gst::Format::Time)
        .property("is-live", true)
        .property("block", false)
        .property("caps", audio_caps(channels))
        .build()
        .expect("appsrc")
        .downcast::<gst_app::AppSrc>()
        .expect("downcast appsrc")
}

fn make_appsink(channels: i32) -> gst_app::AppSink {
    // drop=true: keep only the newest buffer. pull_mono drains and upstream
    // queues refill; a deeper appsink queue is unnecessary for these tests.
    make_appsink_opts(channels, true, 1)
}

fn make_appsink_opts(channels: i32, drop: bool, max_buffers: u32) -> gst_app::AppSink {
    gst::ElementFactory::make("appsink")
        .property("emit-signals", false)
        .property("sync", false)
        .property("max-buffers", max_buffers)
        .property("drop", drop)
        .property("caps", audio_caps(channels))
        .build()
        .expect("appsink")
        .downcast::<gst_app::AppSink>()
        .expect("downcast appsink")
}

fn active_map(s: &str) -> gst::Structure {
    gst::Structure::from_str(s).unwrap_or_else(|e| panic!("active-map `{s}`: {e}"))
}

fn capsfilter_channels(el: &gst::Element) -> u32 {
    let caps: gst::Caps = el.property("caps");
    let ch = caps
        .structure(0)
        .expect("caps structure")
        .get::<i32>("channels")
        .expect("channels field");
    assert!(ch > 0, "{}: non-positive channels", el.name());
    ch as u32
}

fn tone_buffer(freq: f32, pts: gst::ClockTime) -> gst::Buffer {
    let mut buf = gst::Buffer::with_size(FRAME_SAMPLES * 4).expect("buffer");
    {
        let buf = buf.get_mut().unwrap();
        buf.set_pts(pts);
        buf.set_duration(gst::ClockTime::from_nseconds(
            FRAME_SAMPLES as u64 * 1_000_000_000 / SAMPLE_RATE as u64,
        ));
        let mut map = buf.map_writable().expect("map");
        let omega = 2.0 * std::f32::consts::PI * freq / SAMPLE_RATE as f32;
        let start = (pts.nseconds() * SAMPLE_RATE as u64 / 1_000_000_000) as usize;
        for (i, chunk) in map.as_mut_slice().chunks_exact_mut(4).enumerate() {
            let sample = 0.8 * ((start + i) as f32 * omega).sin();
            chunk.copy_from_slice(&sample.to_le_bytes());
        }
    }
    buf
}

fn push_lockstep(a: &gst_app::AppSrc, b: &gst_app::AppSrc, start_idx: u64, n: usize) {
    for i in 0..n {
        let pts = gst::ClockTime::from_nseconds(
            (start_idx + i as u64) * FRAME_SAMPLES as u64 * 1_000_000_000 / SAMPLE_RATE as u64,
        );
        a.push_buffer(tone_buffer(A4_HZ, pts)).expect("push A");
        b.push_buffer(tone_buffer(perfect_fifth_hz(A4_HZ), pts))
            .expect("push B");
    }
}

fn pull_mono(appsink: &gst_app::AppSink, frames: usize) -> Vec<f32> {
    let mut mono = Vec::with_capacity(frames * FRAME_SAMPLES);
    let deadline = Instant::now() + Duration::from_secs(3);
    while mono.len() < frames * FRAME_SAMPLES {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let ms = u64::try_from(remaining.as_millis()).unwrap_or(0);
        if ms == 0 {
            break;
        }
        let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(ms)) else {
            continue;
        };
        let map = sample.buffer().unwrap().map_readable().unwrap();
        for chunk in map.as_slice().chunks_exact(4) {
            mono.push(f32::from_le_bytes(chunk.try_into().unwrap()));
        }
    }
    assert!(
        mono.len() >= FRAME_SAMPLES,
        "insufficient samples (got {})",
        mono.len()
    );
    mono
}

fn assert_tone(label: &str, samples: &[f32], expect: Tone) {
    assert!(
        expect.dominant_in(samples, SAMPLE_RATE as f32),
        "{label}: expected {:?} Hz; p_low={} p_high={}",
        expect.hz(),
        common::goertzel_power(samples, SAMPLE_RATE as f32, A4_HZ),
        common::goertzel_power(samples, SAMPLE_RATE as f32, perfect_fifth_hz(A4_HZ)),
    );
}

fn ephemeral_http_port() -> u16 {
    // Same approach as nvnmosd `lock_ordering_regression`: pick a free port up
    // front when the test must drive in-band HTTP (IS-08 here, IS-05 there).
    // Prefer `http-port=0` when the allocated port is not needed.
    let listener = TcpListener::bind("0.0.0.0:0").expect("bind ephemeral");
    listener.local_addr().expect("addr").port()
}

fn post_is08_activation(http_port: u16, body: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", http_port)) {
            Ok(s) => break s,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "IS-08 HTTP port {http_port} never accepted connections: {e}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let path = "/x-nmos/channelmapping/v1.0/map/activations/";
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{http_port}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).expect("write POST");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read POST");
    assert!(
        resp.starts_with("HTTP/1.1 2") || resp.starts_with("HTTP/1.0 2"),
        "IS-08 activation failed: {resp}"
    );
}

fn wait_paused(pipeline: &gst::Pipeline) {
    // Reach PAUSED first so nmosaudiochannelmap fixation can finish without
    // needing buffers (live appsrcs will not complete PLAYING until fed).
    if pipeline.set_state(gst::State::Paused).is_err() {
        dump_pipeline_errors(pipeline);
        panic!("set_state(Paused) failed");
    }
    let (ret, state, pending) = pipeline.state(gst::ClockTime::from_seconds(10));
    if ret.is_err() || state != gst::State::Paused || pending != gst::State::VoidPending {
        dump_pipeline_errors(pipeline);
        panic!("pipeline not PAUSED after wait: ret={ret:?} state={state:?} pending={pending:?}");
    }
}

/// Like [`wait_paused`], but returns whether PAUSED was reached cleanly.
fn try_wait_paused(pipeline: &gst::Pipeline) -> bool {
    if pipeline.set_state(gst::State::Paused).is_err() {
        return false;
    }
    let (ret, state, pending) = pipeline.state(gst::ClockTime::from_seconds(10));
    ret.is_ok() && state == gst::State::Paused && pending == gst::State::VoidPending
}

fn wait_playing(pipeline: &gst::Pipeline) {
    // Callers should push at least one buffer before waiting: live appsrcs keep
    // PAUSED→PLAYING async until data arrives.
    if pipeline.set_state(gst::State::Playing).is_err() {
        dump_pipeline_errors(pipeline);
        panic!("set_state(Playing) failed");
    }
    let (ret, state, pending) = pipeline.state(gst::ClockTime::from_seconds(10));
    if ret.is_err() || state != gst::State::Playing || pending != gst::State::VoidPending {
        dump_pipeline_errors(pipeline);
        panic!("pipeline not PLAYING after wait: ret={ret:?} state={state:?} pending={pending:?}");
    }
}

fn dump_pipeline_errors(pipeline: &gst::Pipeline) {
    if let Some(bus) = pipeline.bus() {
        for msg in bus.iter_timed(gst::ClockTime::ZERO) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(err) => {
                    eprintln!(
                        "pipeline ERROR from {:?}: {} ({:?})",
                        msg.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                }
                MessageView::Warning(w) => {
                    eprintln!(
                        "pipeline WARN from {:?}: {}",
                        msg.src().map(|s| s.path_string()),
                        w.error()
                    );
                }
                _ => {}
            }
        }
    }
}

/// Default identity routes `src_0` from `input0` (tone A). An immediate IS-08
/// activation then remaps it to `input1` (tone B) on the same output.
#[test]
fn is08_audio_channelmap_live_reroute_swaps_tone() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&[
        "nmosaudiochannelmap",
        "appsrc",
        "appsink",
        "audiomixer",
        "audiomixmatrix",
    ]);

    let socket = tempfile::Builder::new()
        .prefix("nvnmos_is08_audio_reroute_")
        .suffix(".sock")
        .tempfile_in(std::env::temp_dir())
        .expect("temp socket")
        .into_temp_path();
    let daemon = DaemonGuard::new(socket.to_path_buf());
    // In-band IS-08 needs a known listen port; reserve one like lock_ordering
    // does for IS-05 PATCH (http-port=0 is preferred when the port is unused).
    let http_port = ephemeral_http_port();

    let pipeline = gst::Pipeline::default();
    let src_a = make_appsrc("tone-a", 1);
    let src_b = make_appsrc("tone-b", 1);
    let cf_a = gst::ElementFactory::make("capsfilter")
        .property("caps", audio_caps(1))
        .build()
        .unwrap();
    let cf_b = gst::ElementFactory::make("capsfilter")
        .property("caps", audio_caps(1))
        .build()
        .unwrap();
    let node_seed = format!("gst-is08-audio-reroute-{}", std::process::id());
    let map = gst::ElementFactory::make("nmosaudiochannelmap")
        .property("daemon-uri", daemon.uri())
        .property("node-seed", &node_seed)
        .property("channelmapping-name", "audio-reroute")
        .property("http-port", http_port as u32)
        .build()
        .expect("nmosaudiochannelmap");
    let sink0 = map.request_pad_simple("sink_%u").unwrap();
    let sink1 = map.request_pad_simple("sink_%u").unwrap();
    let src0 = map.request_pad_simple("src_%u").unwrap();
    sink0.set_property("input-id", "input0");
    sink1.set_property("input-id", "input1");
    sink0.set_property("channels", 1u32);
    sink1.set_property("channels", 1u32);
    src0.set_property("output-id", "output0");
    src0.set_property("channels", 1u32);
    let out = make_appsink(1);

    pipeline
        .add_many([
            src_a.upcast_ref(),
            src_b.upcast_ref(),
            &cf_a,
            &cf_b,
            &map,
            out.upcast_ref(),
        ])
        .unwrap();
    src_a.link(&cf_a).unwrap();
    src_b.link(&cf_b).unwrap();
    cf_a.static_pad("src")
        .unwrap()
        .link(&sink0)
        .expect("link A");
    cf_b.static_pad("src")
        .unwrap()
        .link(&sink1)
        .expect("link B");
    src0.link(&out.static_pad("sink").unwrap())
        .expect("link out");

    wait_paused(&pipeline);
    push_lockstep(&src_a, &src_b, 0, 2);
    wait_playing(&pipeline);

    // Default identity: output0 <- input0 (tone A).
    push_lockstep(&src_a, &src_b, 2, MEASURE_FRAMES);
    assert_tone(
        "before re-route",
        &pull_mono(&out, MEASURE_FRAMES),
        Tone::Low,
    );

    post_is08_activation(
        http_port,
        r#"{"activation":{"mode":"activate_immediate"},"action":{"output0":{"0":{"input":"input1","channel_index":0}}}}"#,
    );

    // Drop pre-activation buffers, push a fresh window, expect tone B.
    while out.try_pull_sample(gst::ClockTime::ZERO).is_some() {}
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut idx = (2 + MEASURE_FRAMES) as u64;
    let samples = loop {
        assert!(
            Instant::now() < deadline,
            "re-route did not yield tone B within timeout"
        );
        push_lockstep(&src_a, &src_b, idx, MEASURE_FRAMES);
        idx += MEASURE_FRAMES as u64;
        let samples = pull_mono(&out, MEASURE_FRAMES);
        if Tone::High.dominant_in(&samples, SAMPLE_RATE as f32) {
            break samples;
        }
    };
    assert_tone("after re-route", &samples, Tone::High);

    let _ = pipeline.set_state(gst::State::Null);
}

/// Static `active-map` with lockstep sources: the first measure window must
/// already show isolated tones (no settle / discard). Covers identity and swap.
#[test]
fn is08_audio_channelmap_static_map_first_window_isolates_tones() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&[
        "nmosaudiochannelmap",
        "appsrc",
        "appsink",
        "audiomixer",
        "audiomixmatrix",
    ]);

    struct RoutingCase {
        name: &'static str,
        src0_map: &'static str,
        src1_map: &'static str,
        expect_src0: Tone,
        expect_src1: Tone,
    }
    let cases = [
        RoutingCase {
            name: "identity",
            src0_map: "map,0=input0:0",
            src1_map: "map,0=input1:0",
            expect_src0: Tone::Low,
            expect_src1: Tone::High,
        },
        RoutingCase {
            name: "swapped",
            src0_map: "map,0=input1:0",
            src1_map: "map,0=input0:0",
            expect_src0: Tone::High,
            expect_src1: Tone::Low,
        },
    ];

    for (idx, case) in cases.iter().enumerate() {
        let socket = tempfile::Builder::new()
            .prefix("nvnmos_is08_audio_static_")
            .suffix(".sock")
            .tempfile_in(std::env::temp_dir())
            .expect("temp socket")
            .into_temp_path();
        let daemon = DaemonGuard::new(socket.to_path_buf());

        let pipeline = gst::Pipeline::default();
        let src_a = make_appsrc("tone-a", 1);
        let src_b = make_appsrc("tone-b", 1);
        let cf_a = gst::ElementFactory::make("capsfilter")
            .property("caps", audio_caps(1))
            .build()
            .unwrap();
        let cf_b = gst::ElementFactory::make("capsfilter")
            .property("caps", audio_caps(1))
            .build()
            .unwrap();
        let node_seed = format!("gst-is08-audio-static-{}-{}", std::process::id(), idx);
        let map = gst::ElementFactory::make("nmosaudiochannelmap")
            .property("daemon-uri", daemon.uri())
            .property("node-seed", &node_seed)
            .property("channelmapping-name", format!("audio-static-{}", case.name))
            .build()
            .expect("nmosaudiochannelmap");
        let sink0 = map.request_pad_simple("sink_%u").unwrap();
        let sink1 = map.request_pad_simple("sink_%u").unwrap();
        let src0 = map.request_pad_simple("src_%u").unwrap();
        let src1 = map.request_pad_simple("src_%u").unwrap();
        sink0.set_property("input-id", "input0");
        sink1.set_property("input-id", "input1");
        sink0.set_property("channels", 1u32);
        sink1.set_property("channels", 1u32);
        src0.set_property("output-id", "output0");
        src1.set_property("output-id", "output1");
        src0.set_property("channels", 1u32);
        src1.set_property("channels", 1u32);
        src0.set_property("active-map", active_map(case.src0_map));
        src1.set_property("active-map", active_map(case.src1_map));
        // Keep the startup window: drop=false and room for prime + measure pushes.
        let out0 = make_appsink_opts(1, false, (2 + MEASURE_FRAMES) as u32);
        let out1 = make_appsink_opts(1, false, (2 + MEASURE_FRAMES) as u32);

        pipeline
            .add_many([
                src_a.upcast_ref(),
                src_b.upcast_ref(),
                &cf_a,
                &cf_b,
                &map,
                out0.upcast_ref(),
                out1.upcast_ref(),
            ])
            .unwrap();
        src_a.link(&cf_a).unwrap();
        src_b.link(&cf_b).unwrap();
        cf_a.static_pad("src").unwrap().link(&sink0).unwrap();
        cf_b.static_pad("src").unwrap().link(&sink1).unwrap();
        src0.link(&out0.static_pad("sink").unwrap()).unwrap();
        src1.link(&out1.static_pad("sink").unwrap()).unwrap();

        wait_paused(&pipeline);
        push_lockstep(&src_a, &src_b, 0, 2);
        wait_playing(&pipeline);
        push_lockstep(&src_a, &src_b, 2, MEASURE_FRAMES);

        assert_tone(
            &format!("{} src_0 first window", case.name),
            &pull_mono(&out0, MEASURE_FRAMES),
            case.expect_src0,
        );
        assert_tone(
            &format!("{} src_1 first window", case.name),
            &pull_mono(&out1, MEASURE_FRAMES),
            case.expect_src1,
        );

        let _ = pipeline.set_state(gst::State::Null);
    }
}

/// Channel counts left unset so fixation must take them from peer caps.
#[test]
fn is08_audio_channelmap_infers_channels_from_peer_caps() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&[
        "nmosaudiochannelmap",
        "appsrc",
        "appsink",
        "audiomixer",
        "audiomixmatrix",
    ]);

    let socket = tempfile::Builder::new()
        .prefix("nvnmos_is08_audio_infer_")
        .suffix(".sock")
        .tempfile_in(std::env::temp_dir())
        .expect("temp socket")
        .into_temp_path();
    let daemon = DaemonGuard::new(socket.to_path_buf());

    // Asymmetric widths so a single shared default cannot pass by accident.
    const CH_A: i32 = 2;
    const CH_B: i32 = 8;
    const BUS: i32 = CH_A + CH_B;

    let pipeline = gst::Pipeline::default();
    let src_a = make_appsrc("audio-infer-a", CH_A);
    let src_b = make_appsrc("audio-infer-b", CH_B);
    let cf_a = gst::ElementFactory::make("capsfilter")
        .property("caps", audio_caps(CH_A))
        .build()
        .unwrap();
    let cf_b = gst::ElementFactory::make("capsfilter")
        .property("caps", audio_caps(CH_B))
        .build()
        .unwrap();
    let node_seed = format!("gst-is08-audio-infer-{}", std::process::id());
    let map = gst::ElementFactory::make("nmosaudiochannelmap")
        .property("daemon-uri", daemon.uri())
        .property("node-seed", &node_seed)
        .property("channelmapping-name", "audio-infer")
        .build()
        .expect("nmosaudiochannelmap");
    let sink0 = map.request_pad_simple("sink_%u").unwrap();
    let sink1 = map.request_pad_simple("sink_%u").unwrap();
    let src0 = map.request_pad_simple("src_%u").unwrap();
    let src1 = map.request_pad_simple("src_%u").unwrap();
    sink0.set_property("input-id", "input0");
    sink1.set_property("input-id", "input1");
    src0.set_property("output-id", "output0");
    src1.set_property("output-id", "output1");
    // Leave pad `channels` at 0 (default) so fixation derives counts from the
    // fixed peer caps (capsfilter / appsink).
    let out_a = make_appsink(CH_A);
    let out_b = make_appsink(CH_B);
    pipeline
        .add_many([
            src_a.upcast_ref(),
            src_b.upcast_ref(),
            &cf_a,
            &cf_b,
            &map,
            out_a.upcast_ref(),
            out_b.upcast_ref(),
        ])
        .unwrap();
    src_a.link(&cf_a).unwrap();
    src_b.link(&cf_b).unwrap();
    cf_a.static_pad("src").unwrap().link(&sink0).unwrap();
    cf_b.static_pad("src").unwrap().link(&sink1).unwrap();
    src0.link(&out_a.static_pad("sink").unwrap()).unwrap();
    src1.link(&out_b.static_pad("sink").unwrap()).unwrap();

    wait_paused(&pipeline);

    // Fixation builds named internals from the inferred topology.
    let map_bin = map.downcast_ref::<gst::Bin>().expect("map is a Bin");
    assert_eq!(
        capsfilter_channels(
            &map_bin
                .by_name("sink-capsfilter-0")
                .expect("sink-capsfilter-0")
        ),
        CH_A as u32,
        "input0 inferred channels"
    );
    assert_eq!(
        capsfilter_channels(
            &map_bin
                .by_name("sink-capsfilter-1")
                .expect("sink-capsfilter-1")
        ),
        CH_B as u32,
        "input1 inferred channels"
    );
    let mix0 = map_bin.by_name("mixmatrix-0").expect("mixmatrix-0");
    assert_eq!(
        mix0.property::<u32>("out-channels"),
        CH_A as u32,
        "output0 out-channels"
    );
    assert_eq!(
        mix0.property::<u32>("in-channels"),
        BUS as u32,
        "output0 in-channels (bus)"
    );
    let mix1 = map_bin.by_name("mixmatrix-1").expect("mixmatrix-1");
    assert_eq!(
        mix1.property::<u32>("out-channels"),
        CH_B as u32,
        "output1 out-channels"
    );
    assert_eq!(
        mix1.property::<u32>("in-channels"),
        BUS as u32,
        "output1 in-channels (bus)"
    );

    let _ = pipeline.set_state(gst::State::Null);
}

/// Declared pad `channels` that disagree with fixed peer caps must fail fixation.
#[test]
fn is08_audio_channelmap_rejects_channels_mismatch() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&[
        "nmosaudiochannelmap",
        "appsrc",
        "appsink",
        "audiomixer",
        "audiomixmatrix",
    ]);

    let socket = tempfile::Builder::new()
        .prefix("nvnmos_is08_audio_mismatch_")
        .suffix(".sock")
        .tempfile_in(std::env::temp_dir())
        .expect("temp socket")
        .into_temp_path();
    let daemon = DaemonGuard::new(socket.to_path_buf());

    let pipeline = gst::Pipeline::default();
    let src = make_appsrc("mismatch-src", 2);
    let cf = gst::ElementFactory::make("capsfilter")
        .property("caps", audio_caps(2))
        .build()
        .unwrap();
    let node_seed = format!("gst-is08-audio-mismatch-{}", std::process::id());
    let map = gst::ElementFactory::make("nmosaudiochannelmap")
        .property("daemon-uri", daemon.uri())
        .property("node-seed", &node_seed)
        .property("channelmapping-name", "audio-mismatch")
        .build()
        .expect("nmosaudiochannelmap");
    let sink0 = map.request_pad_simple("sink_%u").unwrap();
    let src0 = map.request_pad_simple("src_%u").unwrap();
    sink0.set_property("input-id", "input0");
    src0.set_property("output-id", "output0");
    // Peer caps are stereo; declare 8-channel Input → fixation must reject.
    sink0.set_property("channels", 8u32);
    src0.set_property("channels", 2u32);
    let out = make_appsink(2);
    pipeline
        .add_many([src.upcast_ref(), &cf, &map, out.upcast_ref()])
        .unwrap();
    src.link(&cf).unwrap();
    cf.static_pad("src").unwrap().link(&sink0).unwrap();
    src0.link(&out.static_pad("sink").unwrap()).unwrap();

    assert!(
        !try_wait_paused(&pipeline),
        "fixation should fail when declared channels disagree with peer caps"
    );
    let _ = pipeline.set_state(gst::State::Null);
}

/// gst-launch defers `sink_0::channels` until the request pad exists; verify parse applies them.
#[test]
fn gst_parse_applies_child_properties_on_request_pads() {
    init();
    require_factories(&["nmosaudiochannelmap", "audiotestsrc"]);

    let pipeline = gst::parse::launch(
        "nmosaudiochannelmap name=map \
         sink_0::channels=7 sink_0::receiver-name=rin sink_0::label=in0 \
         src_0::channels=5 src_0::sender-name=sout src_0::label=out0 \
         audiotestsrc num-buffers=1 ! audio/x-raw,channels=2 ! map.sink_0 \
         map.src_0 ! fakesink sync=false",
    )
    .expect("parse launch");

    let map = pipeline
        .downcast::<gst::Bin>()
        .expect("pipeline bin")
        .by_name("map")
        .expect("map element");
    let map = map.dynamic_cast::<gst::ChildProxy>().expect("ChildProxy");

    let sink = map.child_by_name("sink_0").expect("sink_0 pad");
    assert_eq!(sink.property::<u32>("channels"), 7);
    assert_eq!(sink.property::<String>("receiver-name"), "rin");
    assert_eq!(sink.property::<String>("label"), "in0");

    let src = map.child_by_name("src_0").expect("src_0 pad");
    assert_eq!(src.property::<u32>("channels"), 5);
    assert_eq!(src.property::<String>("sender-name"), "sout");
    assert_eq!(src.property::<String>("label"), "out0");
}
