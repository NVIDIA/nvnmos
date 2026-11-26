// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use gst::prelude::*;
use gstreamer as gst;
use std::sync::Once;
use std::time::{Duration, Instant};

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        gst::init().unwrap();
        gstavsynctest::plugin_register_static().unwrap();
    });
}

/// Sets the pipeline to NULL when dropped, including on test panic.
struct Stop(gst::Pipeline);
impl Drop for Stop {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

fn launch(description: &str) -> (gst::Pipeline, Stop) {
    init();
    let pipeline = gst::parse::launch(description)
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
    (pipeline.clone(), Stop(pipeline))
}

/// Analyzer plus direct 25 fps UYVP video and F32LE audio sources, 200 ms pips.
fn av_pipeline(analyzer_args: &str, live: bool) -> String {
    format!(
        "avsyncanalyzer name=check interval-ms=200 max-offset-ms=90 min-events=3 {analyzer_args} \
        avsyncvideotestsrc is-live={live} num-buffers=40 width=192 height=4 framerate=25/1 pip-interval=200000000 \
          ! video/x-raw,format=UYVP,width=192,height=4,framerate=25/1 ! check.video \
        avsyncaudiotestsrc is-live={live} num-buffers=160 samples-per-buffer=480 pip-interval=200000000 \
          ! audio/x-raw,format=F32LE,rate=48000,channels=2,layout=interleaved ! check.audio"
    )
}

/// Play until EOS and return the summary plus every `avsync-event`.
fn drain(pipeline: &gst::Pipeline) -> (gst::Structure, Vec<gst::Structure>) {
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut report = None;
    let mut events = Vec::new();
    let mut eos = false;
    while Instant::now() < deadline {
        let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        match message.view() {
            gst::MessageView::Error(error) => panic!(
                "{}: {} {:?}",
                error.src().unwrap().path_string(),
                error.error(),
                error.debug()
            ),
            gst::MessageView::Element(element) => {
                let s = element.structure().unwrap();
                if s.name() == "avsync-event" {
                    assert!(s.get::<gst::Structure>("details").is_ok());
                    events.push(s.to_owned());
                } else if s.name() == "avsync-summary" {
                    assert!(report.is_none(), "only one summary at EOS");
                    report = Some(s.to_owned());
                }
            }
            gst::MessageView::Eos(_) => {
                eos = true;
                break;
            }
            _ => {}
        }
    }
    assert!(eos, "all requested inputs must finish without hanging EOS");
    (report.expect("summary message before EOS"), events)
}

fn median(report: &gst::Structure, kind: &str) -> f64 {
    report
        .get::<gst::Structure>("offsets")
        .unwrap()
        .get::<gst::Structure>(kind)
        .unwrap()
        .get::<f64>("median_ms")
        .unwrap()
}

fn run(description: &str) -> gst::Structure {
    let (pipeline, _stop) = launch(description);
    let (report, events) = drain(&pipeline);
    let counts = report.get::<gst::Structure>("event_counts").unwrap();
    assert_eq!(
        events.len() as u64,
        ["video", "audio", "caption"]
            .iter()
            .map(|k| counts.get::<u64>(*k).unwrap())
            .sum::<u64>()
    );
    let cycles = report.get::<gst::Array>("cycles").unwrap();
    assert_eq!(
        cycles.len() as u64,
        report.get::<u64>("matched_cycles").unwrap()
    );
    for cycle in cycles.iter() {
        assert!(
            cycle
                .get::<gst::Structure>()
                .unwrap()
                .get::<u64>("video_media_ns")
                .is_ok()
        );
    }
    let stats = pipeline
        .by_name("check")
        .unwrap()
        .property::<gst::Structure>("stats");
    assert!(stats.get::<gst::Structure>("buffers").is_ok());
    report
}

#[test]
fn direct_av_sources_are_measured_from_pixels_and_samples() {
    for live in [true, false] {
        let report = run(&av_pipeline("", live));
        assert_eq!(
            report.get::<String>("status").unwrap(),
            "pass",
            "{report:#}"
        );
        assert!(report.get::<u64>("matched_cycles").unwrap() >= 3);
        assert!(median(&report, "audio").abs() < 1.);
        assert!(median(&report, "audio_onset").abs() < 4.);
    }
}

#[test]
fn direct_video_audio_and_captions_share_a_timeline() {
    // Use video ancillary metas to create a separate CDP stream. This exercises
    // the element on real generated captions even without gst-plugins-rs ANC
    // extractors installed on a developer machine.
    // At 60 fps a DTVCC packet spans CDPs. Its timestamp must remain the
    // packet's starting frame, not the frame completing its continuation.
    // At 30000/1001 fps pips fall inside frames, so a caption leads the
    // interpolated crossing by up to one frame and must still pass.
    for (fps, frames, drop_continuation) in [
        ("25/1", 40, false),
        ("60/1", 96, false),
        ("60/1", 96, true),
        ("30000/1001", 48, false),
    ] {
        let (pipeline, _stop) = launch(&format!(
            "avsyncanalyzer name=check interval-ms=200 max-offset-ms=90 min-events=3 \
        avsyncvideotestsrc is-live=true num-buffers={frames} width=192 height=4 framerate={fps} pip-interval=200000000 \
          ! video/x-raw,format=v210,width=192,height=4,framerate={fps} ! identity name=tap ! check.video \
        appsrc name=captions is-live=true format=time caps=closedcaption/x-cea-708,format=cdp,framerate={fps} ! check.caption \
        avsyncaudiotestsrc is-live=true num-buffers=160 samples-per-buffer=480 pip-interval=200000000 \
          ! audio/x-raw,format=S24BE,rate=48000,channels=2,layout=interleaved ! check.audio"
        ));
        let captions = pipeline
            .by_name("captions")
            .unwrap()
            .downcast::<gstreamer_app::AppSrc>()
            .unwrap();
        let seen = std::sync::atomic::AtomicUsize::new(0);
        pipeline
            .by_name("tap")
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
                move |_, info| {
                    if let Some(buffer) = info.buffer() {
                        let index = seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        for cdp in gstavsynctest::analyze::caption_cdps(buffer)
                            .into_iter()
                            .filter(|_| !drop_continuation || index != 13)
                        {
                            let mut output = gst::Buffer::from_mut_slice(cdp);
                            output.get_mut().unwrap().set_pts(buffer.pts());
                            output.get_mut().unwrap().set_duration(buffer.duration());
                            captions.push_buffer(output).unwrap();
                        }
                    }
                    if info
                        .event()
                        .is_some_and(|e| matches!(e.view(), gst::EventView::Eos(_)))
                    {
                        captions.end_of_stream().unwrap();
                    }
                    gst::PadProbeReturn::Ok
                },
            );
        let (report, events) = drain(&pipeline);
        assert_eq!(
            report.get::<String>("status").unwrap(),
            if drop_continuation {
                "inconclusive"
            } else {
                "pass"
            },
            "{report:#}"
        );
        if drop_continuation {
            let first_caption = events
                .iter()
                .find(|e| e.get::<String>("stream").unwrap() == "caption")
                .and_then(|e| e.get::<u64>("media_ns").ok());
            assert!(
                first_caption.unwrap() >= 400_000_000,
                "lost continuation must not backdate a later marker"
            );
        }
        let caption = median(&report, "caption");
        if fps == "30000/1001" {
            assert!((-33.4..=0.).contains(&caption), "{caption}");
        } else {
            assert_eq!(caption, 0.);
        }
    }
}

#[test]
fn interim_reports_cover_consecutive_windows() {
    let (pipeline, _stop) = launch(&av_pipeline("report-cycles=2", true));
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let mut windows = Vec::new();
    while let Some(message) = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(8),
        &[
            gst::MessageType::Element,
            gst::MessageType::Eos,
            gst::MessageType::Error,
        ],
    ) {
        match message.view() {
            gst::MessageView::Element(e)
                if e.structure().unwrap().name() == "avsync-interim-summary" =>
            {
                windows.push(e.structure().unwrap().to_owned())
            }
            gst::MessageView::Element(_) => {}
            gst::MessageView::Eos(_) => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(windows.len() >= 2, "{windows:#?}");
    for (i, w) in windows.iter().enumerate() {
        assert_eq!(w.get::<u64>("window_index").unwrap(), i as u64);
        assert_eq!(w.get::<String>("status").unwrap(), "pass", "{w:#}");
        assert_eq!(w.get::<u64>("matched_cycles").unwrap(), 2);
        if i > 0 {
            assert_eq!(
                w.get::<u64>("window_start_ns").unwrap(),
                windows[i - 1].get::<u64>("window_end_ns").unwrap()
            );
        }
    }
}

#[test]
fn streams_connected_late_and_out_of_order_still_measure() {
    // Like an operator making NMOS connections one at a time: one input only
    // starts flowing 600 ms after the other. Earlier cycles are excluded as
    // boundary, not counted as missing.
    for late in ["video", "audio"] {
        let (pipeline, _stop) = launch(&format!(
            "avsyncanalyzer name=check interval-ms=200 max-offset-ms=90 min-events=3 \
            avsyncvideotestsrc is-live=true num-buffers=40 width=192 height=4 framerate=25/1 pip-interval=200000000 \
              ! video/x-raw,format=UYVP,width=192,height=4,framerate=25/1 ! valve name=video drop={} ! check.video \
            avsyncaudiotestsrc is-live=true num-buffers=160 samples-per-buffer=480 pip-interval=200000000 \
              ! audio/x-raw,format=F32LE,rate=48000,channels=2,layout=interleaved ! valve name=audio drop={} ! check.audio",
            late == "video",
            late == "audio",
        ));
        let valve = pipeline.by_name(late).unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            valve.set_property("drop", false);
        });
        let (report, _) = drain(&pipeline);
        assert_eq!(
            report.get::<String>("status").unwrap(),
            "pass",
            "{late}: {report:#}"
        );
        assert!(report.get::<u64>("matched_cycles").unwrap() >= 3);
        assert!(median(&report, "audio").abs() < 1.);
    }
}

#[test]
fn reference_mode_rejects_arrival_based_pts() {
    let (pipeline, stop) = launch(
        "avsyncanalyzer name=check timestamp-mode=reference \
        videotestsrc num-buffers=1 ! video/x-raw,format=GRAY8 ! check.video \
        audiotestsrc num-buffers=1 ! audio/x-raw,format=F32LE ! check.audio",
    );
    let _ = pipeline.set_state(gst::State::Playing);
    let error = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(gst::ClockTime::from_seconds(3), &[gst::MessageType::Error]);
    drop(stop);
    let message = error.expect("missing reference metadata must fail");
    let gst::MessageView::Error(error) = message.view() else {
        unreachable!()
    };
    assert!(
        error
            .error()
            .message()
            .contains("GstReferenceTimestampMeta"),
        "{} {:?}",
        error.error(),
        error.debug()
    );
}

#[test]
fn anc_pad_can_arrive_after_playing() {
    init();
    if gst::ElementFactory::find("st2038anctocc").is_none() {
        return;
    }
    let (pipeline, stop) = launch(
        "avsyncanalyzer name=check \
        appsrc is-live=true ! check.video appsrc is-live=true ! check.audio",
    );
    pipeline.set_state(gst::State::Playing).unwrap();
    let _ = pipeline.state(gst::ClockTime::from_seconds(1));
    let check = pipeline.by_name("check").unwrap();
    let pad = check.request_pad_simple("anc");
    drop(stop);
    assert!(
        pad.is_some(),
        "extractors may expose ANC only after their first frame"
    );
}

#[test]
fn required_anc_cannot_silently_become_an_av_only_pass() {
    let report = run(&av_pipeline("require-anc=true", true));
    assert_eq!(report.get::<String>("status").unwrap(), "inconclusive");
    assert_eq!(
        report
            .get::<gst::Structure>("runtime")
            .unwrap()
            .get::<gst::Structure>("buffers")
            .unwrap()
            .get::<u64>("caption")
            .unwrap(),
        0
    );
}

#[test]
fn packed_scanline_detection_uses_pixels_without_a_frame_index() {
    use gstreamer_video::{VideoFormat, VideoInfo};
    init();
    for format in [VideoFormat::Uyvp, VideoFormat::V210] {
        let info = VideoInfo::builder(format, 6, 2)
            .fps(gst::Fraction::new(60, 1))
            .build()
            .unwrap();
        let mut buffer = gst::Buffer::with_size(info.size()).unwrap();
        {
            let mut map = buffer.get_mut().unwrap().map_writable().unwrap();
            map.as_mut_slice().fill(0);
            let row = &mut map.as_mut_slice()[info.stride()[0] as usize..];
            if format == VideoFormat::Uyvp {
                for pair in 0..3 {
                    let y = if pair == 2 { 940u64 } else { 64 };
                    let word = (512u64 << 30) | (y << 20) | (512 << 10) | y;
                    row[pair * 5..pair * 5 + 5].copy_from_slice(&word.to_be_bytes()[3..]);
                }
            } else {
                let words = [
                    512u32 | (64 << 10) | (512 << 20),
                    64 | (512 << 10) | (64 << 20),
                    512 | (64 << 10) | (512 << 20),
                    940 | (512 << 10) | (940 << 20),
                ];
                for (i, word) in words.iter().enumerate() {
                    row[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
                }
            }
        }
        let sample = gst::Sample::builder()
            .buffer(&buffer)
            .caps(&info.to_caps().unwrap())
            .build();
        assert_eq!(
            gstavsynctest::analyze::bar_centroid_at(&sample, 1, 0.86),
            Some(4.5)
        );
        assert_eq!(
            gstavsynctest::analyze::bar_centroid_at(&sample, 0, 0.86),
            None
        );
    }
}

#[test]
fn reference_domains_are_checked_before_internal_conversion() {
    for mismatch in [false, true] {
        let (pipeline, stop) = launch(
            "avsyncanalyzer name=check timestamp-mode=reference \
            appsrc name=v is-live=true format=time caps=video/x-raw,format=GRAY8,width=100,height=1,framerate=60/1 ! check.video \
            appsrc name=a is-live=true format=time caps=audio/x-raw,format=S24BE,rate=48000,channels=2,layout=interleaved ! check.audio",
        );
        pipeline.set_state(gst::State::Playing).unwrap();
        for (name, size, domain) in [("v", 100, 0), ("a", 576, if mismatch { 1 } else { 0 })] {
            let src = pipeline
                .by_name(name)
                .unwrap()
                .downcast::<gstreamer_app::AppSrc>()
                .unwrap();
            let mut buffer = gst::Buffer::from_mut_slice(vec![0u8; size]);
            let b = buffer.get_mut().unwrap();
            b.set_pts(gst::ClockTime::ZERO);
            gst::ReferenceTimestampMeta::add(
                b,
                &format!("timestamp/x-ptp,domain=(uint){domain}")
                    .parse::<gst::Caps>()
                    .unwrap(),
                gst::ClockTime::SECOND,
                None,
            );
            src.push_buffer(buffer).unwrap();
            src.end_of_stream().unwrap();
        }
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(3),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("EOS or clock-domain error");
        drop(stop);
        if mismatch {
            let gst::MessageView::Error(error) = message.view() else {
                panic!("mismatched domains must fail")
            };
            assert!(
                error
                    .error()
                    .message()
                    .contains("reference timestamp domains")
            );
        } else {
            assert!(
                matches!(message.view(), gst::MessageView::Eos(_)),
                "valid reference metadata should survive our conversion path"
            );
        }
    }
}
