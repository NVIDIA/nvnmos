// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::analysis::{Event, PipTracker, Stream, VideoTracker, summarize, summary_text};
use crate::{analyze, captions, signal};
use cdp_types::CDPParser;
use gst::{glib, prelude::*, subclass::prelude::*};
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_audio as gst_audio;
use gstreamer_video as gst_video;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstAvSyncTimestampMode")]
pub enum TimestampMode {
    #[default]
    #[enum_value(name = "Sample PTS mapped to running time", nick = "running-time")]
    RunningTime,
    #[enum_value(name = "GstReferenceTimestampMeta timestamp", nick = "reference")]
    Reference,
}

impl TimestampMode {
    fn nick(self) -> &'static str {
        match self {
            Self::RunningTime => "running-time",
            Self::Reference => "reference",
        }
    }
}

#[derive(Clone, Copy)]
struct Settings {
    tolerance: f64,
    window: f64,
    interval: f64,
    min_events: u32,
    history: u32,
    scanline: i32,
    channel: u32,
    frequency: f64,
    timestamp_mode: TimestampMode,
    require_anc: bool,
    print_summary: bool,
    report_cycles: u32,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            tolerance: 20.,
            window: 250.,
            interval: signal::DEFAULT_PIP_INTERVAL.mseconds() as f64,
            min_events: 3,
            history: 6000,
            scanline: -1,
            channel: 0,
            frequency: signal::DEFAULT_PIP_FREQ_HZ,
            timestamp_mode: TimestampMode::default(),
            require_anc: false,
            print_summary: true,
            report_cycles: 0,
        }
    }
}

struct InputStamp {
    available: u64,
    reference: Option<(gst::Caps, u64)>,
}

/// Per-input bookkeeping, keyed by [`Stream`] in [`Runtime::streams`].
#[derive(Default)]
struct StreamState {
    /// Input availability (and reference timestamp) by raw input PTS, captured
    /// before internal conversion.
    arrivals: BTreeMap<u64, InputStamp>,
    buffers: u64,
    discontinuities: u64,
    first_pts: Option<u64>,
    last_pts: Option<u64>,
    last_end: Option<u64>,
    eos: bool,
}

struct Runtime {
    streams: BTreeMap<Stream, StreamState>,
    video: VideoTracker,
    audio: PipTracker,
    cdp: CDPParser,
    /// DTVCC sequence -> (media time, availability, frame duration) of the
    /// CDP that started the packet.
    pending_caption: BTreeMap<u8, (u64, u64, u64)>,
    events: VecDeque<Event>,
    /// Latest media time of any event dropped by `history-events`; cycles at
    /// or before it may be missing a partner and are treated as boundary.
    evicted_until: Option<u64>,
    timeline_discontinuity: bool,
    reference: Option<gst::Caps>,
    started: u64,
    reported: bool,
    bad_cdp: u64,
    error: Option<String>,
    /// End of the last interim report window, and how many were posted.
    window_end: Option<u64>,
    windows: u64,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            streams: BTreeMap::new(),
            video: VideoTracker::default(),
            audio: PipTracker::default(),
            cdp: CDPParser::new(),
            pending_caption: BTreeMap::new(),
            events: VecDeque::new(),
            evicted_until: None,
            timeline_discontinuity: false,
            reference: None,
            started: now(),
            reported: false,
            bad_cdp: 0,
            error: None,
            window_end: None,
            windows: 0,
        }
    }
}
impl Runtime {
    /// The state for `kind`, registering the stream on first use.
    fn stream(&mut self, kind: Stream) -> &mut StreamState {
        self.streams.entry(kind).or_default()
    }
    fn reset(&mut self, kind: Stream) {
        self.stream(kind).discontinuities += 1;
        match kind {
            Stream::Video => self.video.reset(),
            Stream::Audio => self.audio.reset(),
            Stream::Caption => self.reset_caption(),
        }
    }
    fn reset_caption(&mut self) {
        self.cdp = CDPParser::new();
        self.pending_caption.clear();
    }
    /// Runtime problems that make a report inconclusive.
    fn faults(&self) -> Vec<String> {
        let mut faults = Vec::new();
        if self.timeline_discontinuity {
            faults.push("timeline discontinuity".to_string());
        }
        if self.bad_cdp > 0 {
            faults.push(format!("{} invalid caption CDPs", self.bad_cdp));
        }
        if let Some(error) = &self.error {
            faults.push(format!("error: {error}"));
        }
        faults
    }
    /// Summaries of every complete `report-cycles` window of video cycles.
    /// A window closes once every started stream has passed its end by
    /// `max-offset-ms` (so late partners have arrived), or video has passed
    /// it by two intervals (a stalled or disconnected stream counts as missing).
    fn interim_reports(&mut self, s: &Settings) -> Vec<gst::Structure> {
        let mut reports = Vec::new();
        if s.report_cycles == 0 {
            return reports;
        }
        let ns = |ms: f64| (ms * 1_000_000.0).round() as u64;
        let (interval, window) = (ns(s.interval), ns(s.window));
        loop {
            let after = self.window_end;
            let mut video: Vec<u64> = self
                .events
                .iter()
                .filter(|e| e.stream == Stream::Video && after.is_none_or(|a| e.media_ns > a))
                .map(|e| e.media_ns)
                .collect();
            video.sort_unstable();
            let Some(&last) = video.get(s.report_cycles as usize - 1) else {
                break;
            };
            let start = after.unwrap_or(video[0].saturating_sub(interval / 2));
            let end = last + interval / 2;
            let caught_up = self
                .streams
                .values()
                .all(|st| st.last_end.is_none_or(|e| e >= end + window));
            let video_moved_on = self
                .streams
                .get(&Stream::Video)
                .and_then(|v| v.last_end)
                .is_some_and(|e| e >= end + 2 * interval);
            if !caught_up && !video_moved_on {
                break;
            }
            let events: Vec<Event> = self
                .events
                .iter()
                .filter(|e| (start..end).contains(&e.media_ns))
                .cloned()
                .collect();
            let mut report = summarize(
                &events,
                self.streams.contains_key(&Stream::Caption),
                s.tolerance,
                s.window,
                s.min_events.min(s.report_cycles),
                Some((start, end)),
                self.faults(),
            );
            report.set_name("avsync-interim-summary");
            report.set("window_index", self.windows);
            report.set("window_start_ns", start);
            report.set("window_end_ns", end);
            self.window_end = Some(end);
            self.windows += 1;
            reports.push(report);
        }
        reports
    }
    fn caption_events(&mut self, data: &[u8], t: u64, available: u64, frame: u64) -> Vec<Event> {
        if self.cdp.parse(data).is_err() {
            self.bad_cdp += 1;
            self.reset_caption();
            return Vec::new();
        }
        // Associate a completed DTVCC packet with the CDP that started
        // it, not a later frame carrying its continuation/padding.
        if data.len() > 8 && data[4] & 0x40 != 0 {
            let offset = 7 + if data[4] & 0x80 != 0 { 5 } else { 0 };
            if let Some(count) = data.get(offset + 1) {
                if let Some(triples) =
                    data.get(offset + 2..offset + 2 + 3 * usize::from(count & 31))
                {
                    for triple in triples.chunks_exact(3) {
                        if triple[0] & 7 == 7 {
                            self.pending_caption
                                .insert(triple[1] >> 6, (t, available, frame));
                        }
                    }
                }
            }
        }
        let mut events = Vec::new();
        while let Some(packet) = self.cdp.pop_packet() {
            // Key by packet sequence: a lost continuation must not
            // assign an old, incomplete packet's time to a later pip.
            let Some((stamp, availability, frame)) =
                self.pending_caption.remove(&packet.sequence_no())
            else {
                continue;
            };
            let Some(text) = analyze::caption_packet_text(&packet) else {
                continue;
            };
            let text = text.trim();
            if [captions::CC_TICK, captions::CC_TOCK, "BEEP"]
                .iter()
                .any(|marker| text.eq_ignore_ascii_case(marker))
            {
                events.push(Event {
                    stream: Stream::Caption,
                    media_ns: stamp,
                    available_ns: availability,
                    // Only the carrying frame is known, not the instant within it.
                    uncertainty_ns: frame,
                    details: gst::Structure::builder("caption")
                        .field("text", text)
                        .field("timing", "caption-content")
                        .field("service", u32::from(captions::CC_SERVICE_NO))
                        .build(),
                });
            }
        }
        events
    }
}

/// A sample's content, decoded before taking the runtime lock.
enum Decoded<'a> {
    Video {
        width: u32,
        x: Option<f64>,
    },
    Audio {
        rate: u32,
        channels: u32,
        samples: std::borrow::Cow<'a, [f32]>,
    },
    Caption(&'a [u8]),
}

#[derive(Default)]
pub struct AvSyncAnalyzer {
    settings: Mutex<Settings>,
    runtime: Mutex<Runtime>,
    branches: Mutex<BTreeMap<String, gst::Bin>>,
}

#[glib::object_subclass]
impl ObjectSubclass for AvSyncAnalyzer {
    const NAME: &'static str = "GstAvSyncAnalyzer";
    type Type = super::AvSyncAnalyzer;
    type ParentType = gst::Bin;
}

impl ObjectImpl for AvSyncAnalyzer {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let d = Settings::default();
            vec![
                glib::ParamSpecDouble::builder("tolerance-ms")
                    .minimum(0.001)
                    .maximum(100000.)
                    .default_value(d.tolerance)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecDouble::builder("max-offset-ms")
                    .minimum(0.001)
                    .maximum(100000.)
                    .default_value(d.window)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecDouble::builder("interval-ms")
                    .minimum(0.001)
                    .maximum(1000000.)
                    .default_value(d.interval)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("min-events")
                    .minimum(1)
                    .default_value(d.min_events)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("history-events")
                    .minimum(3)
                    .maximum(1000000)
                    .default_value(d.history)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecInt::builder("scanline")
                    .minimum(-1)
                    .maximum(100000)
                    .default_value(d.scanline)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("audio-channel")
                    .maximum(255)
                    .default_value(d.channel)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecDouble::builder("beep-frequency")
                    .minimum(1.)
                    .maximum(96000.)
                    .default_value(d.frequency)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("timestamp-mode", d.timestamp_mode)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("require-anc")
                    .default_value(d.require_anc)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("print-summary")
                    .nick("Print summary")
                    .blurb("Print a human-readable summary to stdout when the report is produced")
                    .default_value(d.print_summary)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("report-cycles")
                    .nick("Report cycles")
                    .blurb("Also post an avsync-interim-summary every N video cycles (0 = only at the end)")
                    .default_value(d.report_cycles)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
                    .read_only()
                    .build(),
            ]
        });
        PROPS.as_ref()
    }
    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut s = self.settings.lock().unwrap();
        match pspec.name() {
            "tolerance-ms" => s.tolerance = value.get().unwrap(),
            "max-offset-ms" => s.window = value.get().unwrap(),
            "interval-ms" => s.interval = value.get().unwrap(),
            "min-events" => s.min_events = value.get().unwrap(),
            "history-events" => s.history = value.get().unwrap(),
            "scanline" => s.scanline = value.get().unwrap(),
            "audio-channel" => s.channel = value.get().unwrap(),
            "beep-frequency" => s.frequency = value.get().unwrap(),
            "timestamp-mode" => s.timestamp_mode = value.get().unwrap(),
            "require-anc" => s.require_anc = value.get().unwrap(),
            "print-summary" => s.print_summary = value.get().unwrap(),
            "report-cycles" => s.report_cycles = value.get().unwrap(),
            _ => unreachable!("unknown property"),
        }
    }
    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        if pspec.name() == "stats" {
            let r = self.runtime.lock().unwrap();
            return gst::Structure::builder("avsync-stats")
                .field("buffers", counts("buffers", &r.streams, |s| s.buffers))
                .field("bad_caption_cdps", r.bad_cdp)
                .build()
                .to_value();
        }
        let s = self.settings.lock().unwrap();
        match pspec.name() {
            "tolerance-ms" => s.tolerance.to_value(),
            "max-offset-ms" => s.window.to_value(),
            "interval-ms" => s.interval.to_value(),
            "min-events" => s.min_events.to_value(),
            "history-events" => s.history.to_value(),
            "scanline" => s.scanline.to_value(),
            "audio-channel" => s.channel.to_value(),
            "beep-frequency" => s.frequency.to_value(),
            "timestamp-mode" => s.timestamp_mode.to_value(),
            "require-anc" => s.require_anc.to_value(),
            "print-summary" => s.print_summary.to_value(),
            "report-cycles" => s.report_cycles.to_value(),
            _ => unreachable!("unknown property"),
        }
    }
}
impl GstObjectImpl for AvSyncAnalyzer {}

impl ElementImpl for AvSyncAnalyzer {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "AV sync analyzer",
                "Sink/Analyzer",
                "Measure A/V sync of received video bar, audio pip and captions",
                "NVIDIA Corporation",
            )
        });
        Some(&META)
    }
    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PADS: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            [
                (
                    "video",
                    "video/x-raw,format=(string){UYVP,v210,UYVY,YUY2,GRAY8}",
                ),
                (
                    "audio",
                    "audio/x-raw,format=(string){F32LE,S24BE,S16BE},layout=interleaved",
                ),
                ("anc", "meta/x-st-2038"),
                ("caption", "closedcaption/x-cea-708,format=cdp"),
            ]
            .iter()
            .map(|(name, caps)| {
                gst::PadTemplate::new(
                    name,
                    gst::PadDirection::Sink,
                    gst::PadPresence::Request,
                    &caps.parse::<gst::Caps>().unwrap(),
                )
                .unwrap()
            })
            .collect()
        });
        PADS.as_ref()
    }
    fn request_new_pad(
        &self,
        template: &gst::PadTemplate,
        _name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let name = template.name_template();
        let kind = Stream::for_pad(name);
        {
            let branches = self.branches.lock().unwrap();
            if branches.contains_key(name)
                || (kind == Stream::Caption
                    && branches
                        .keys()
                        .any(|b| Stream::for_pad(b) == Stream::Caption))
                || (self.obj().current_state() > gst::State::Ready && kind != Stream::Caption)
            {
                return None;
            }
        }
        let conversion = match name {
            "anc" => " ! st2038anctocc ! ccconverter ! closedcaption/x-cea-708,format=cdp",
            "audio" => " ! audioconvert ! audio/x-raw,format=F32LE,layout=interleaved",
            _ => "",
        };
        let description = format!(
            "identity name=arrival{conversion} ! appsink name=measure sync=false async=false max-buffers=8 drop=false"
        );
        let branch = match gst::parse::bin_from_description(&description, false) {
            Ok(b) => b,
            Err(e) => {
                self.fail(&e.to_string());
                return None;
            }
        };
        let identity = branch.by_name("arrival")?;
        let input = identity.static_pad("sink")?;
        branch
            .add_pad(
                &gst::GhostPad::builder_with_target(&input)
                    .ok()?
                    .name("sink")
                    .build(),
            )
            .ok()?;
        let sink = branch
            .by_name("measure")?
            .downcast::<gst_app::AppSink>()
            .ok()?;
        let weak = self.obj().downgrade();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let obj = weak.upgrade().ok_or(gst::FlowError::Flushing)?;
                    match obj.imp().sample(sink, kind) {
                        Ok(()) => Ok(gst::FlowSuccess::Ok),
                        Err(error) => {
                            obj.imp().runtime.lock().unwrap().error = Some(error.clone());
                            obj.imp().fail(&error);
                            Err(gst::FlowError::Error)
                        }
                    }
                })
                .build(),
        );
        let weak = self.obj().downgrade();
        input.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let available = now();
            if let (Some(obj), Some(buffer)) = (weak.upgrade(), info.buffer()) {
                if let Some(pts) = buffer.pts() {
                    // Only reference mode needs the meta. Capture it here:
                    // audio/ANC conversion need not preserve it.
                    let reference = (obj.imp().settings.lock().unwrap().timestamp_mode
                        == TimestampMode::Reference)
                        .then(|| buffer.meta::<gst::ReferenceTimestampMeta>())
                        .flatten()
                        .map(|meta| (meta.reference().to_owned(), meta.timestamp().nseconds()));
                    let mut r = obj.imp().runtime.lock().unwrap();
                    let stamps = &mut r.stream(kind).arrivals;
                    stamps.insert(
                        pts.nseconds(),
                        InputStamp {
                            available,
                            reference,
                        },
                    );
                    while stamps.len() > 512 {
                        stamps.pop_first();
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
        self.obj().add(&branch).ok()?;
        let ghost = gst::GhostPad::builder_from_template(template)
            .name(name)
            .build();
        ghost.set_target(branch.static_pad("sink").as_ref()).ok()?;
        self.runtime.lock().unwrap().stream(kind);
        self.branches
            .lock()
            .unwrap()
            .insert(name.into(), branch.clone());
        self.obj().add_pad(&ghost).ok()?;
        branch.sync_state_with_parent().ok()?;
        Some(ghost.upcast())
    }
    fn release_pad(&self, pad: &gst::Pad) {
        if self.obj().current_state() > gst::State::Ready {
            return;
        }
        let branch = self.branches.lock().unwrap().remove(pad.name().as_str());
        if let Some(branch) = branch {
            if pad.parent().as_ref() == Some(self.obj().upcast_ref()) {
                let _ = self.obj().remove_pad(pad);
            }
            // GstBin disposal may already have removed its child before
            // releasing our ghost request pad. Do not remove it twice.
            if branch.parent().as_ref() == Some(self.obj().upcast_ref()) {
                let _ = branch.set_state(gst::State::Null);
                let _ = self.obj().remove(&branch);
            }
        }
    }
    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::ReadyToPaused {
            let settings = *self.settings.lock().unwrap();
            let branches = self.branches.lock().unwrap();
            if !branches.contains_key("video")
                || !branches.contains_key("audio")
                || !branches
                    .keys()
                    .all(|name| self.obj().static_pad(name).is_some_and(|p| p.is_linked()))
                || settings.window >= settings.interval / 2.
                || [
                    settings.tolerance,
                    settings.window,
                    settings.interval,
                    settings.frequency,
                ]
                .iter()
                .any(|v| !v.is_finite())
            {
                drop(branches);
                self.fail(
                    "Link video/audio (and all requested pads) and use max-offset-ms < interval-ms/2",
                );
                return Err(gst::StateChangeError);
            }
            let mut r = Runtime::default();
            for name in branches.keys() {
                r.stream(Stream::for_pad(name));
            }
            if settings.require_anc {
                r.stream(Stream::Caption);
            }
            *self.runtime.lock().unwrap() = r;
        }
        let result = self.parent_change_state(transition);
        if transition == gst::StateChange::PausedToReady {
            self.report();
        }
        result
    }
}
impl BinImpl for AvSyncAnalyzer {
    fn handle_message(&self, message: gst::Message) {
        if matches!(message.view(), gst::MessageView::Eos(_)) {
            let (kind, inputs) = {
                let branches = self.branches.lock().unwrap();
                let kind = branches
                    .iter()
                    .find(|(_, branch)| message.src() == Some(branch.upcast_ref()))
                    .map(|(name, _)| Stream::for_pad(name));
                let inputs: Vec<_> = branches.keys().map(|n| Stream::for_pad(n)).collect();
                (kind, inputs)
            };
            if let Some(kind) = kind {
                let complete = {
                    let mut r = self.runtime.lock().unwrap();
                    r.stream(kind).eos = true;
                    inputs
                        .iter()
                        .all(|k| r.streams.get(k).is_some_and(|s| s.eos))
                };
                if complete {
                    self.report();
                }
            }
        }
        self.parent_handle_message(message);
    }
}

impl AvSyncAnalyzer {
    fn fail(&self, error: &str) {
        gst::element_error!(self.obj(), gst::StreamError::Failed, ("{error}"));
    }
    fn message(&self, structure: gst::Structure) {
        let _ = self.obj().post_message(
            gst::message::Element::builder(structure)
                .src(&*self.obj())
                .build(),
        );
    }
    fn report(&self) {
        let settings = *self.settings.lock().unwrap();
        let report = {
            let mut r = self.runtime.lock().unwrap();
            if r.reported || r.streams.is_empty() {
                return;
            }
            r.reported = true;
            let ended = now();
            let media_bounds = r
                .streams
                .values()
                .try_fold((0, u64::MAX), |(start, end), s| {
                    Some((start.max(s.first_pts?), end.min(s.last_end?)))
                })
                .map(|(start, end)| (start.max(r.evicted_until.unwrap_or(0)), end))
                .filter(|(start, end)| start <= end);
            let faults = r.faults();
            let caption = r.streams.contains_key(&Stream::Caption);
            let mut value = summarize(
                r.events.make_contiguous(),
                caption,
                settings.tolerance,
                settings.window,
                settings.min_events,
                media_bounds,
                faults,
            );
            value.set("history_events_limit", settings.history);
            value.set("interim_reports", r.windows);
            let mut capture = gst::Structure::builder("capture")
                .field("started_ns", r.started)
                .field("ended_ns", ended)
                .build();
            if let Some((start, end)) = media_bounds {
                capture.set("media_start_ns", start);
                capture.set("media_end_ns", end);
            }
            value.set("capture", capture);
            let mut runtime = gst::Structure::builder("runtime")
                .field("buffers", counts("buffers", &r.streams, |s| s.buffers))
                .field(
                    "discontinuities",
                    counts("discontinuities", &r.streams, |s| s.discontinuities),
                )
                .field("bad_caption_cdps", r.bad_cdp)
                .field("timeline_discontinuity", r.timeline_discontinuity)
                .field("timestamp_mode", settings.timestamp_mode.nick())
                .build();
            if let Some(reference) = &r.reference {
                runtime.set("reference_caps", reference.to_string());
            }
            if let Some(error) = &r.error {
                runtime.set("error", error);
            }
            value.set("runtime", runtime);
            value
        };
        self.publish(report, settings.print_summary);
    }
    fn publish(&self, report: gst::Structure, print: bool) {
        if print {
            let mut name = self.obj().name().to_string();
            if let Ok(index) = report.get::<u64>("window_index") {
                name.push_str(&format!(" [window {index}]"));
            }
            println!("{}", summary_text(&name, &report));
        }
        self.message(report);
    }
    fn sample(&self, sink: &gst_app::AppSink, kind: Stream) -> Result<(), String> {
        let sample = sink.pull_sample().map_err(|e| e.to_string())?;
        let buffer = sample.buffer().ok_or("Missing buffer")?;
        if buffer
            .flags()
            .intersects(gst::BufferFlags::CORRUPTED | gst::BufferFlags::GAP)
        {
            self.runtime.lock().unwrap().reset(kind);
            return Ok(());
        }
        let settings = *self.settings.lock().unwrap();
        let pts = buffer.pts().ok_or("Missing PTS")?;
        let caps = sample.caps().ok_or("Missing caps")?;
        let map = buffer.map_readable().map_err(|e| e.to_string())?;
        // Decode pixels/samples before locking: none of it needs shared state.
        let (duration, decoded) = match kind {
            Stream::Video => {
                let info = gst_video::VideoInfo::from_caps(caps).map_err(|e| e.to_string())?;
                let row = if settings.scanline < 0 {
                    info.height() / 2
                } else {
                    settings.scanline as u32
                };
                if row >= info.height() {
                    return Err("Scanline is outside video height".into());
                }
                let fps = info.fps();
                let duration = buffer
                    .duration()
                    .or_else(|| {
                        (fps.numer() > 0)
                            .then(|| {
                                gst::ClockTime::SECOND
                                    .mul_div_floor(fps.denom() as u64, fps.numer() as u64)
                            })
                            .flatten()
                    })
                    .ok_or("Video needs duration or framerate")?;
                let x = analyze::bar_centroid_at(&sample, row, 0.86);
                (
                    duration.nseconds(),
                    Decoded::Video {
                        width: info.width(),
                        x,
                    },
                )
            }
            Stream::Audio => {
                let info = gst_audio::AudioInfo::from_caps(caps).map_err(|e| e.to_string())?;
                let samples = analyze::f32le_samples_copied(map.as_slice());
                let frames = (samples.len() / info.channels() as usize) as u64;
                let duration = gst::ClockTime::SECOND
                    .mul_div_floor(frames, u64::from(info.rate()))
                    .ok_or("Audio duration overflow")?;
                (
                    duration.nseconds(),
                    Decoded::Audio {
                        rate: info.rate(),
                        channels: info.channels(),
                        samples,
                    },
                )
            }
            Stream::Caption => (
                buffer.duration().map_or(0, |d| d.nseconds()),
                Decoded::Caption(map.as_slice()),
            ),
        };
        let mut r = self.runtime.lock().unwrap();
        let input = r
            .stream(kind)
            .arrivals
            .remove(&pts.nseconds())
            .ok_or("Cannot associate input availability with sample PTS")?;
        let available = input.available;
        let t = match settings.timestamp_mode {
            TimestampMode::Reference => {
                let (reference, timestamp) = input.reference.ok_or(
                    "Missing GstReferenceTimestampMeta; reconstruct a common sender clock",
                )?;
                if r.reference
                    .as_ref()
                    .is_some_and(|previous| previous != &reference)
                {
                    return Err("Mismatched reference timestamp domains".into());
                }
                r.reference = Some(reference);
                timestamp
            }
            TimestampMode::RunningTime => sample
                .segment()
                .and_then(|s| s.downcast_ref::<gst::ClockTime>())
                .and_then(|s| s.to_running_time(pts))
                .ok_or("PTS is outside the sample's TIME segment")?
                .nseconds(),
        };
        let stream = r.stream(kind);
        // Time going backwards invalidates the run. A DISCONT flag (e.g. a lost
        // RTP packet) only resets this stream's detector.
        let backwards = stream.last_pts.is_some_and(|p| t <= p);
        let discont = buffer.flags().contains(gst::BufferFlags::DISCONT);
        stream.last_pts = Some(t);
        stream.first_pts.get_or_insert(t);
        stream.last_end = Some(t.saturating_add(duration));
        stream.buffers += 1;
        r.timeline_discontinuity |= backwards;
        if backwards || discont {
            r.reset(kind);
        }
        let events = match decoded {
            Decoded::Video { width, x: Some(x) } => r
                .video
                .feed(x, width, t, available, duration)
                .into_iter()
                .collect(),
            Decoded::Video { x: None, .. } => {
                r.video.reset();
                Vec::new()
            }
            Decoded::Audio {
                rate,
                channels,
                samples,
            } => r.audio.feed(
                &samples,
                rate,
                channels,
                settings.channel,
                t,
                available,
                settings.frequency,
            )?,
            Decoded::Caption(data) => {
                // CDP buffers may lack a duration; fall back to the video frame.
                let frame = if duration > 0 {
                    duration
                } else {
                    r.streams
                        .get(&Stream::Video)
                        .and_then(|v| Some(v.last_end? - v.last_pts?))
                        .unwrap_or(0)
                };
                r.caption_events(data, t, available, frame)
            }
        };
        r.events.extend(events.iter().cloned());
        while r.events.len() > settings.history as usize {
            if let Some(old) = r.events.pop_front() {
                r.evicted_until = r.evicted_until.max(Some(old.media_ns));
            }
        }
        let reports = r.interim_reports(&settings);
        drop(r);
        for event in &events {
            self.message(event.structure());
        }
        for report in reports {
            self.publish(report, settings.print_summary);
        }
        Ok(())
    }
}

fn counts(
    name: &str,
    streams: &BTreeMap<Stream, StreamState>,
    value: fn(&StreamState) -> u64,
) -> gst::Structure {
    let mut structure = gst::Structure::new_empty(name);
    for (kind, state) in streams {
        structure.set(kind.as_str(), value(state));
    }
    structure
}
