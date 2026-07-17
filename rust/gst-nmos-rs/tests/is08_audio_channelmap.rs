// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! IS-08 audio channel map routing: two tones (A4 440 Hz / E5 ~659 Hz) through
//! `nmosaudiochannelmap`, verify per-src output dominance with Goertzel.
//!
//! Spawns a temporary `nvnmosd` for the duration of the test.
//! The cases run automatically when `nvnmosd` and `libnvnmos.so` are available
//! (CI sets `LD_LIBRARY_PATH` to the C build dir); otherwise they skip.
//!
//! ```bash
//! LD_LIBRARY_PATH=$NVNMOS_LIB_DIR \
//! cargo test --manifest-path rust/Cargo.toml -p gst-nmos-rs \
//!   --test is08_audio_channelmap -- --test-threads=1 --nocapture
//! ```

mod common;

use std::str::FromStr;
use std::time::Duration;

use common::{
    A4_HZ, DaemonGuard, Tone, init, nvnmosd_skip_reason, perfect_fifth_hz, require_factories,
};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use test_skip::skip;

const SAMPLE_RATE: i32 = 48_000;
const CHANNELS: u32 = 2;

struct RoutingCase {
    name: &'static str,
    src0_map: Option<&'static str>,
    src1_map: Option<&'static str>,
    expect_src0: Tone,
    expect_src1: Tone,
}

fn audio_caps() -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("rate", SAMPLE_RATE)
        .field("channels", CHANNELS as i32)
        .field("layout", "interleaved")
        .build()
}

fn make_tone_src(freq: f64) -> gst::Element {
    gst::ElementFactory::make("audiotestsrc")
        .name(format!("tone-{freq}"))
        .property("freq", freq)
        .property("volume", 0.8f64)
        .build()
        .expect("audiotestsrc")
}

fn make_capsfilter() -> gst::Element {
    gst::ElementFactory::make("capsfilter")
        .property("caps", audio_caps())
        .build()
        .expect("capsfilter")
}

fn request_map_pad(map: &gst::Element, templ_name: &str) -> gst::Pad {
    map.request_pad_simple(templ_name)
        .unwrap_or_else(|| panic!("request_pad_simple {templ_name} failed"))
}

fn make_appsink() -> gst_app::AppSink {
    gst::ElementFactory::make("appsink")
        .property("emit-signals", false)
        .property("sync", false)
        .property("max-buffers", 4u32)
        .property("drop", true)
        .property("caps", audio_caps())
        .build()
        .expect("appsink")
        .downcast::<gst_app::AppSink>()
        .expect("downcast appsink")
}

fn pull_mono_samples(appsink: &gst_app::AppSink, timeout: Duration) -> Vec<f32> {
    let mut mono = Vec::new();
    let deadline = std::time::Instant::now() + timeout;
    while mono.len() < SAMPLE_RATE as usize / 5 {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let remaining_ms = u64::try_from(remaining.as_millis()).unwrap_or(0);
        if remaining_ms == 0 {
            break;
        }
        let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(remaining_ms))
        else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        let buffer = sample.buffer().expect("buffer");
        let map = buffer.map_readable().expect("map");
        mono.extend(common::stereo_f32le_to_mono(map.as_slice()));
    }
    assert!(
        mono.len() >= SAMPLE_RATE as usize / 20,
        "insufficient audio samples from appsink (got {} frames)",
        mono.len()
    );
    mono
}

fn sample_has_signal(sample: &gst::Sample) -> bool {
    let Some(buffer) = sample.buffer() else {
        return false;
    };
    let Ok(map) = buffer.map_readable() else {
        return false;
    };

    map.as_slice().chunks_exact(4).any(|sample| {
        let sample = f32::from_le_bytes(sample.try_into().unwrap());
        sample != 0.0
    })
}

/// Pull and drop samples from each appsink until it has yielded non-silence, so
/// the caller measures steady-state output rather than the startup transient
/// where audiomixer can emit silence for an input leg that has not delivered yet.
fn settle(appsinks: &[(&str, &gst_app::AppSink)], timeout: Duration) {
    for (name, appsink) in appsinks {
        let deadline = std::time::Instant::now() + timeout;
        let mut saw_signal = false;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let remaining_ms = u64::try_from(remaining.as_millis()).unwrap_or(0);
            if remaining_ms == 0 {
                break;
            }
            let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(remaining_ms))
            else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            if sample_has_signal(&sample) {
                saw_signal = true;
                break;
            }
        }
        assert!(
            saw_signal,
            "{name}: did not produce non-silent audio during settle"
        );
    }
}

fn set_playing_or_panic(pipeline: &gst::Pipeline) {
    match pipeline.set_state(gst::State::Playing) {
        Ok(_) => {}
        Err(_) => {
            dump_pipeline_errors(pipeline);
            panic!("failed to set pipeline to PLAYING");
        }
    }
    let timeout = gst::ClockTime::from_seconds(10);
    match pipeline.state(timeout) {
        (Ok(_), gst::State::Playing, gst::State::VoidPending) => {}
        (Ok(_), state, pending) => {
            dump_pipeline_errors(pipeline);
            panic!("pipeline stuck in {state:?} (pending {pending:?}) after PLAYING");
        }
        (Err(_), ..) => {
            dump_pipeline_errors(pipeline);
            panic!("pipeline state query failed after PLAYING");
        }
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

fn run_routing_case(daemon_uri: &str, node_seed: &str, case: &RoutingCase, declare_channels: bool) {
    let pipeline = gst::Pipeline::default();
    struct PipelineGuard(gst::Pipeline);
    impl Drop for PipelineGuard {
        fn drop(&mut self) {
            let _ = self.0.set_state(gst::State::Null);
        }
    }
    let _guard = PipelineGuard(pipeline.clone());

    let src_low = make_tone_src(f64::from(A4_HZ));
    let src_high = make_tone_src(f64::from(perfect_fifth_hz(A4_HZ)));
    let cf_low = make_capsfilter();
    let cf_high = make_capsfilter();

    let map = gst::ElementFactory::make("nmosaudiochannelmap")
        .name("map")
        .property("daemon-uri", daemon_uri)
        .property("node-seed", node_seed)
        .property("channelmapping-name", "audio-routing-smoke")
        .build()
        .expect("nmosaudiochannelmap");

    let sink0 = request_map_pad(&map, "sink_%u");
    let sink1 = request_map_pad(&map, "sink_%u");
    let src0 = request_map_pad(&map, "src_%u");
    let src1 = request_map_pad(&map, "src_%u");

    sink0.set_property("input-id", "input0");
    sink1.set_property("input-id", "input1");
    // When declared, the element pins the topology from these counts. When not,
    // it must infer them from the fixed peer caps (capsfilter / appsink) at
    // fixation; both paths are covered by separate tests.
    if declare_channels {
        sink0.set_property("channels", CHANNELS);
        sink1.set_property("channels", CHANNELS);
        src0.set_property("channels", CHANNELS);
        src1.set_property("channels", CHANNELS);
    }

    if let Some(s) = case.src0_map {
        let structure = gst::Structure::from_str(s).expect("src0 active-map");
        src0.set_property("active-map", &structure);
    }
    if let Some(s) = case.src1_map {
        let structure = gst::Structure::from_str(s).expect("src1 active-map");
        src1.set_property("active-map", &structure);
    }

    let out0 = make_appsink();
    let out1 = make_appsink();

    pipeline
        .add_many([
            &src_low,
            &cf_low,
            &src_high,
            &cf_high,
            &map,
            out0.upcast_ref(),
            out1.upcast_ref(),
        ])
        .expect("add elements");

    gst::Element::link_many([&src_low, &cf_low]).expect("link low tone");
    gst::Element::link_many([&src_high, &cf_high]).expect("link high tone");

    cf_low
        .static_pad("src")
        .unwrap()
        .link(&sink0)
        .expect("link cf_low -> map.sink_0");
    cf_high
        .static_pad("src")
        .unwrap()
        .link(&sink1)
        .expect("link cf_high -> map.sink_1");

    src0.link(&out0.static_pad("sink").unwrap())
        .expect("link map.src_0 -> appsink0");
    src1.link(&out1.static_pad("sink").unwrap())
        .expect("link map.src_1 -> appsink1");

    set_playing_or_panic(&pipeline);

    // Discard the startup transient before measuring: audiomixer
    // (ignore-inactive-pads=true) can emit silence for an input leg that has not
    // delivered yet. Once both outputs have yielded non-silence, the dominance
    // checks below measure steady-state routing rather than startup timing.
    settle(
        &[("src_0", &out0), ("src_1", &out1)],
        Duration::from_secs(3),
    );

    let samples0 = pull_mono_samples(&out0, Duration::from_secs(3));
    let samples1 = pull_mono_samples(&out1, Duration::from_secs(3));

    pipeline.set_state(gst::State::Null).expect("NULL");

    assert!(
        case.expect_src0.dominant_in(&samples0, SAMPLE_RATE as f32),
        "{}: src_0 expected {:?} Hz dominant; case={} p_low={} p_high={}",
        case.name,
        case.expect_src0.hz(),
        case.name,
        common::goertzel_power(&samples0, SAMPLE_RATE as f32, A4_HZ),
        common::goertzel_power(&samples0, SAMPLE_RATE as f32, perfect_fifth_hz(A4_HZ)),
    );
    assert!(
        case.expect_src1.dominant_in(&samples1, SAMPLE_RATE as f32),
        "{}: src_1 expected {:?} Hz dominant; case={} p_low={} p_high={}",
        case.name,
        case.expect_src1.hz(),
        case.name,
        common::goertzel_power(&samples1, SAMPLE_RATE as f32, A4_HZ),
        common::goertzel_power(&samples1, SAMPLE_RATE as f32, perfect_fifth_hz(A4_HZ)),
    );
}

#[test]
fn is08_audio_channelmap_routes_and_swaps_tones() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&[
        "nmosaudiochannelmap",
        "audiotestsrc",
        "capsfilter",
        "appsink",
        "audiomixer",
        "audiomixmatrix",
    ]);

    let socket = tempfile::Builder::new()
        .prefix("nvnmos_is08_audio_")
        .suffix(".sock")
        .tempfile_in(std::env::temp_dir())
        .expect("temp socket")
        .into_temp_path();
    let _daemon = DaemonGuard::new(socket.to_path_buf());
    let daemon_uri = _daemon.uri();

    let cases = [
        RoutingCase {
            name: "identity",
            src0_map: Some("map,0=input0:0,1=input0:1"),
            src1_map: Some("map,0=input1:0,1=input1:1"),
            expect_src0: Tone::Low,
            expect_src1: Tone::High,
        },
        RoutingCase {
            name: "swapped",
            src0_map: Some("map,0=input1:0,1=input1:1"),
            src1_map: Some("map,0=input0:0,1=input0:1"),
            expect_src0: Tone::High,
            expect_src1: Tone::Low,
        },
    ];

    for (idx, case) in cases.iter().enumerate() {
        let node_seed = format!("gst-is08-audio-{}-{}", std::process::id(), idx);
        run_routing_case(&daemon_uri, &node_seed, case, true);
    }
}

/// Companion to the routing test: leave every pad's `channels` at its default
/// (0) so the element must infer the 2-in/2-out topology from the fixed peer
/// caps (capsfilter on each sink, appsink caps on each src) during fixation.
#[test]
fn is08_audio_channelmap_infers_channels_from_peer_caps() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&[
        "nmosaudiochannelmap",
        "audiotestsrc",
        "capsfilter",
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
    let _daemon = DaemonGuard::new(socket.to_path_buf());

    let case = RoutingCase {
        name: "identity-inferred-channels",
        src0_map: Some("map,0=input0:0,1=input0:1"),
        src1_map: Some("map,0=input1:0,1=input1:1"),
        expect_src0: Tone::Low,
        expect_src1: Tone::High,
    };
    let node_seed = format!("gst-is08-audio-infer-{}", std::process::id());
    run_routing_case(&_daemon.uri(), &node_seed, &case, false);
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
    assert_eq!(
        sink.property::<u32>("channels"),
        7,
        "sink channels child prop"
    );
    assert_eq!(
        sink.property::<String>("receiver-name"),
        "rin",
        "sink receiver-name child prop"
    );
    assert_eq!(
        sink.property::<String>("label"),
        "in0",
        "sink label child prop"
    );

    let src = map.child_by_name("src_0").expect("src_0 pad");
    assert_eq!(
        src.property::<u32>("channels"),
        5,
        "src channels child prop"
    );
    assert_eq!(
        src.property::<String>("sender-name"),
        "sout",
        "src sender-name child prop"
    );
    assert_eq!(
        src.property::<String>("label"),
        "out0",
        "src label child prop"
    );
}
