// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Streaming event detection and bounded, one-to-one event matching. Detection
//! uses recovered content and timestamps, never the generator's signal phase.

use gstreamer as gst;

const NS: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stream {
    Video,
    Audio,
    Caption,
}

impl Stream {
    pub const ALL: [Self; 3] = [Self::Video, Self::Audio, Self::Caption];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Caption => "caption",
        }
    }

    /// Request pad name to stream: the `anc` (ST 2038) and `caption` (CDP)
    /// pads both carry the caption stream.
    pub fn for_pad(name: &str) -> Self {
        match name {
            "video" => Self::Video,
            "audio" => Self::Audio,
            _ => Self::Caption,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub stream: Stream,
    pub media_ns: u64,
    pub available_ns: u64,
    pub uncertainty_ns: u64,
    pub details: gst::Structure,
}

impl Event {
    pub fn structure(&self) -> gst::Structure {
        gst::Structure::builder("avsync-event")
            .field("stream", self.stream.as_str())
            .field("media_ns", self.media_ns)
            .field("available_ns", self.available_ns)
            .field("uncertainty_ns", self.uncertainty_ns)
            .field("details", &self.details)
            .build()
    }
}

#[derive(Default)]
pub struct VideoTracker {
    previous: Option<(f64, u64)>,
}

impl VideoTracker {
    pub fn reset(&mut self) {
        self.previous = None;
    }

    pub fn feed(
        &mut self,
        x: f64,
        width: u32,
        pts: u64,
        available: u64,
        duration: u64,
    ) -> Option<Event> {
        let previous = self.previous.replace((x, pts))?;
        if (x - previous.0).abs() > f64::from(width) / 2.0 || pts <= previous.1 {
            return None;
        }
        let target = f64::from(width) / 2.0;
        if previous.0 >= target || x < target {
            return None;
        }
        // The bar sweeps linearly, so interpolate the crossing between the two
        // frames that straddle the centre instead of snapping to one of them.
        let gap = pts - previous.1;
        let fraction = (target - previous.0) / (x - previous.0);
        let media_ns = previous.1 + (fraction * gap as f64).round() as u64;
        // Consecutive frames: error is about one column of sweep. Missing
        // frames: the motion between them is unobserved, so stay conservative.
        let uncertainty_ns = if gap > duration + duration / 2 {
            gap / 2
        } else {
            (gap as f64 / (x - previous.0)).ceil() as u64
        };
        Some(Event {
            stream: Stream::Video,
            media_ns,
            available_ns: available,
            uncertainty_ns,
            details: gst::Structure::builder("video")
                .field("previous_x", previous.0)
                .field("center_x", x)
                .field("target_x", target)
                .build(),
        })
    }
}

#[derive(Default)]
pub struct PipTracker {
    onset: Option<Event>,
    last_active: u64,
    last_sign: Option<bool>,
    crossings: u32,
    first_crossing: u64,
    last_crossing: u64,
}

impl PipTracker {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    #[allow(clippy::too_many_arguments)] // Keep the buffer format and sample timebase explicit.
    pub fn feed(
        &mut self,
        samples: &[f32],
        rate: u32,
        channels: u32,
        channel: u32,
        pts: u64,
        available: u64,
        frequency: f64,
    ) -> Result<Vec<Event>, String> {
        if rate == 0
            || channels == 0
            || channel >= channels
            || !samples.len().is_multiple_of(channels as usize)
        {
            return Err("Invalid audio rate/channels or truncated interleaved audio".into());
        }
        let mut events = Vec::new();
        for (index, frame) in samples.chunks_exact(channels as usize).enumerate() {
            let value = frame[channel as usize];
            if !value.is_finite() {
                return Err("Non-finite audio sample".into());
            }
            let t = pts
                .checked_add(index as u64 * NS / u64::from(rate))
                .ok_or("Audio timestamp overflow")?;
            if value.abs() >= 0.02 {
                self.onset.get_or_insert_with(|| Event {
                    stream: Stream::Audio,
                    media_ns: t,
                    available_ns: available,
                    uncertainty_ns: NS / u64::from(rate),
                    details: gst::Structure::builder("audio")
                        .field("channel", channel)
                        .field("sample_rate", rate)
                        .build(),
                });
                let sign = value >= 0.0;
                if self.last_sign.is_some_and(|previous| previous != sign) {
                    self.crossings += 1;
                    if self.crossings == 1 {
                        self.first_crossing = t;
                    }
                    self.last_crossing = t;
                }
                self.last_sign = Some(sign);
                self.last_active = t;
            } else if self.onset.is_some() && t.saturating_sub(self.last_active) >= NS / 1000 {
                let mut event = self.onset.take().expect("onset checked");
                let span = self.last_active.saturating_sub(event.media_ns);
                let crossing_span = self.last_crossing.saturating_sub(self.first_crossing);
                let measured = if self.crossings > 1 && crossing_span > 0 {
                    f64::from(self.crossings - 1) * NS as f64 / (2.0 * crossing_span as f64)
                } else {
                    0.0
                };
                if span >= NS / 1000 && (measured - frequency).abs() <= frequency * 0.25 {
                    event
                        .details
                        .set("estimated_frequency_hz", (measured * 10.0).round() / 10.0);
                    event.details.set("tone_onset_media_ns", event.media_ns);
                    // The generator centres the pip on the video event, so the
                    // tone centre (not its onset) is the sync reference.
                    event.media_ns += span / 2;
                    event
                        .details
                        .set("tone_last_active_media_ns", self.last_active);
                    events.push(event);
                }
                self.reset();
            }
        }
        Ok(events)
    }
}

pub fn distribution(mut values: Vec<f64>) -> Option<gst::Structure> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    let median = if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    };
    let mut abs: Vec<_> = values.iter().map(|v| v.abs()).collect();
    abs.sort_by(f64::total_cmp);
    Some(
        gst::Structure::builder("distribution")
            .field("min_ms", values[0])
            .field("median_ms", median)
            .field("max_ms", values[n - 1])
            .field("p95_abs_ms", abs[((n as f64 * 0.95).ceil() as usize) - 1])
            .field("peak_to_peak_ms", values[n - 1] - values[0])
            .build(),
    )
}

fn delta_ms(a: u64, b: u64) -> f64 {
    (i128::from(a) - i128::from(b)) as f64 / 1_000_000.0
}

fn set_distribution(target: &mut gst::Structure, key: &str, values: Vec<f64>) {
    if let Some(value) = distribution(values) {
        target.set(key, value);
    }
}

/// Match each video event one-to-one with the nearest audio (and caption)
/// event within `max_offset`, then decide the status once. `faults` are
/// runtime problems that make the run inconclusive regardless of the matches.
pub fn summarize(
    events: &[Event],
    caption: bool,
    tolerance: f64,
    max_offset: f64,
    min_events: u32,
    bounds: Option<(u64, u64)>,
    faults: Vec<String>,
) -> gst::Structure {
    let required: &[Stream] = if caption {
        &[Stream::Audio, Stream::Caption]
    } else {
        &[Stream::Audio]
    };
    let sorted = |kind: Stream| {
        let mut group: Vec<_> = events.iter().filter(|e| e.stream == kind).collect();
        group.sort_by_key(|e| e.media_ns);
        group
    };
    let groups: Vec<_> = required.iter().map(|&kind| sorted(kind)).collect();
    let mut used: Vec<_> = groups.iter().map(|g| vec![false; g.len()]).collect();
    let window = (max_offset * 1_000_000.0).round() as u64;
    let inside = |t: u64, margin: u64| {
        bounds.is_none_or(|(start, end)| {
            t >= start.saturating_add(margin) && t <= end.saturating_sub(margin)
        })
    };
    let mut offsets = vec![Vec::new(); required.len()];
    let mut availability = vec![Vec::new(); required.len()];
    let mut onsets = Vec::new();
    let mut cycles = Vec::new();
    let (mut unmatched, mut boundary, mut failures, mut uncertain) = (0u64, 0u64, 0u64, false);
    for video in sorted(Stream::Video) {
        if !inside(video.media_ns, window) {
            boundary += 1;
            continue;
        }
        let Some(matches) = groups
            .iter()
            .zip(&used)
            .map(|(group, used)| {
                // Groups are sorted, so only the slice within the window is scanned.
                let start =
                    group.partition_point(|e| e.media_ns < video.media_ns.saturating_sub(window));
                group[start..]
                    .iter()
                    .enumerate()
                    .take_while(|(_, e)| e.media_ns <= video.media_ns.saturating_add(window))
                    .filter(|(i, _)| !used[start + i])
                    .min_by_key(|(_, e)| e.media_ns.abs_diff(video.media_ns))
                    .map(|(i, e)| (start + i, *e))
            })
            .collect::<Option<Vec<_>>>()
        else {
            unmatched += 1;
            continue;
        };
        let video_uncertainty = video.uncertainty_ns as f64 / 1_000_000.0;
        uncertain |= video_uncertainty > tolerance;
        let mut row = gst::Structure::builder("cycle")
            .field("video_media_ns", video.media_ns)
            .field("video_uncertainty_ms", video_uncertainty)
            .build();
        let mut out_of_tolerance = false;
        for (k, (index, event)) in matches.into_iter().enumerate() {
            used[k][index] = true;
            let kind = required[k].as_str();
            let offset = delta_ms(event.media_ns, video.media_ns);
            let available = delta_ms(event.available_ns, video.available_ns);
            // A caption is stamped with the frame that carries it, so it can
            // lead the interpolated crossing by up to one frame (its uncertainty).
            let early = match required[k] {
                Stream::Caption => event.uncertainty_ns as f64 / 1_000_000.0,
                _ => 0.0,
            };
            out_of_tolerance |= offset > tolerance || offset < -(tolerance + early);
            offsets[k].push(offset);
            availability[k].push(available);
            row.set(format!("{kind}_minus_video_ms"), offset);
            row.set(format!("{kind}_minus_video_availability_ms"), available);
            match required[k] {
                Stream::Audio => {
                    if let Ok(onset) = event.details.get::<u64>("tone_onset_media_ns") {
                        let onset = delta_ms(onset, video.media_ns);
                        onsets.push(onset);
                        row.set("audio_onset_minus_video_ms", onset);
                    }
                }
                Stream::Caption => {
                    if let Ok(text) = event.details.get::<String>("text") {
                        row.set("caption_text", text);
                    }
                }
                Stream::Video => {}
            }
        }
        failures += u64::from(out_of_tolerance);
        cycles.push(row);
    }
    // Reasons in this list make the run inconclusive; a tolerance failure is
    // appended afterwards so the printed summary can explain a fail too.
    let mut reasons = Vec::new();
    if cycles.len() < min_events as usize {
        reasons.push("too few matched cycles".to_string());
    }
    if unmatched > 0 {
        reasons.push(format!("{unmatched} video cycles without a matching event"));
    }
    let mut orphaned = gst::Structure::new_empty("unmatched-events");
    for (k, kind) in required.iter().enumerate() {
        let count = groups[k]
            .iter()
            .zip(&used[k])
            // An event up to `window` inside the edge may belong to a video
            // cycle excluded as boundary, so orphans use a double margin.
            .filter(|(event, used)| !**used && inside(event.media_ns, 2 * window))
            .count() as u64;
        orphaned.set(kind.as_str(), count);
        if count > 0 {
            reasons.push(format!("{count} unmatched {} events", kind.as_str()));
        }
    }
    if uncertain {
        reasons.push("video timing uncertainty exceeds tolerance (dropped frames?)".into());
    }
    reasons.extend(faults);
    let status = if !reasons.is_empty() {
        "inconclusive"
    } else if failures > 0 {
        "fail"
    } else {
        "pass"
    };
    if failures > 0 {
        reasons.push(format!("{failures} cycles out of tolerance"));
    }
    let mut offset_dists = gst::Structure::new_empty("offsets");
    let mut availability_dists = gst::Structure::new_empty("availability-offsets");
    for ((kind, offsets), availability) in required.iter().zip(offsets).zip(availability) {
        set_distribution(&mut offset_dists, kind.as_str(), offsets);
        set_distribution(&mut availability_dists, kind.as_str(), availability);
    }
    set_distribution(&mut offset_dists, "audio_onset", onsets);
    let mut counts = gst::Structure::new_empty("event-counts");
    for kind in Stream::ALL {
        counts.set(
            kind.as_str(),
            events.iter().filter(|e| e.stream == kind).count() as u64,
        );
    }
    gst::Structure::builder("avsync-summary")
        .field("status", status)
        .field("reasons", gst::Array::new(reasons))
        .field("event_counts", counts)
        .field("matched_cycles", cycles.len() as u64)
        .field("min_events", min_events)
        .field("unmatched_video_cycles", unmatched)
        .field("boundary_cycles_excluded", boundary)
        .field("unmatched_events", orphaned)
        .field("out_of_tolerance_cycles", failures)
        .field("measurement_uncertainty_exceeds_tolerance", uncertain)
        .field("tolerance_ms", tolerance)
        .field("max_offset_ms", max_offset)
        .field(
            "sign_convention",
            "positive means audio/caption is later than video",
        )
        .field("caption_timing", "caption-content, not rendered display")
        .field(
            "availability_timing",
            "analyzer input buffer availability, not network arrival",
        )
        .field("offsets", offset_dists)
        .field("availability_offsets", availability_dists)
        .field("cycles", gst::Array::new(cycles))
        .build()
}

/// One short, human-readable rendering of an `avsync-summary` structure:
/// the verdict, its reasons, and the per-stream offsets.
pub fn summary_text(element: &str, summary: &gst::StructureRef) -> String {
    let mut out = format!(
        "{element}: {} | {} matched cycles, tolerance {} ms",
        summary
            .get::<String>("status")
            .unwrap_or_default()
            .to_uppercase(),
        summary.get::<u64>("matched_cycles").unwrap_or(0),
        summary.get::<f64>("tolerance_ms").unwrap_or(0.0),
    );
    let reasons: Vec<String> = summary
        .get::<gst::Array>("reasons")
        .map(|a| a.iter().filter_map(|r| r.get().ok()).collect())
        .unwrap_or_default();
    if !reasons.is_empty() {
        out.push_str(&format!("\n  reason: {}", reasons.join("; ")));
    }
    if let Ok(offsets) = summary.get::<gst::Structure>("offsets") {
        for (kind, value) in offsets.iter() {
            let Ok(d) = value.get::<gst::Structure>() else {
                continue;
            };
            let f = |key: &str| d.get::<f64>(key).unwrap_or(f64::NAN);
            out.push_str(&format!(
                "\n  {:<12} minus video: median {:+.2} ms, range [{:+.2}, {:+.2}], p95 |x| {:.2} ms",
                kind.as_str(),
                f("median_ms"),
                f("min_ms"),
                f("max_ms"),
                f("p95_abs_ms"),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(stream: Stream, t: u64) -> Event {
        Event {
            stream,
            media_ns: t,
            available_ns: t,
            uncertainty_ns: 0,
            details: gst::Structure::new_empty("details"),
        }
    }
    #[test]
    fn tracker_ignores_wrap_and_interpolates_crossing() {
        gst::init().unwrap();
        let mut t = VideoTracker::default();
        assert!(t.feed(90., 100, 0, 0, 10).is_none());
        assert!(t.feed(10., 100, 10, 10, 10).is_none());
        assert!(t.feed(40., 100, 20, 20, 10).is_none());
        assert_eq!(t.feed(56., 100, 30, 30, 10).unwrap().media_ns, 26);
        assert!(t.feed(60., 100, 40, 40, 10).is_none());
    }
    #[test]
    fn streaming_pip_across_buffers_is_sample_accurate() {
        gst::init().unwrap();
        let samples: Vec<_> = (0..400)
            .map(|i| {
                if (80..272).contains(&i) {
                    0.8 * ((i - 80) as f32 * std::f32::consts::TAU * 1000.0 / 48000.0).sin()
                } else {
                    0.0
                }
            })
            .collect();
        let mut tracker = PipTracker::default();
        let mut out = Vec::new();
        for (i, chunk) in samples.chunks(101).enumerate() {
            out.extend(
                tracker
                    .feed(chunk, 48000, 1, 0, i as u64 * 101 * NS / 48000, 0, 1000.0)
                    .unwrap(),
            );
        }
        assert_eq!(out.len(), 1);
        assert!(out[0].media_ns.abs_diff(176 * NS / 48000) <= 1);
        assert!(
            out[0]
                .details
                .get::<u64>("tone_onset_media_ns")
                .unwrap()
                .abs_diff(81 * NS / 48000)
                <= 1
        );
    }
    #[test]
    fn bounded_matching_does_not_reuse_events_and_explains_why() {
        gst::init().unwrap();
        let events = [
            event(Stream::Video, 10),
            event(Stream::Video, 20),
            event(Stream::Audio, 15),
        ];
        let r = summarize(&events, false, 20., 250., 3, None, vec![]);
        assert_eq!(r.get::<u64>("matched_cycles").unwrap(), 1);
        assert_eq!(r.get::<u64>("unmatched_video_cycles").unwrap(), 1);
        assert_eq!(r.get::<String>("status").unwrap(), "inconclusive");
        let text = summary_text("check", &r);
        assert!(
            text.starts_with("check: INCONCLUSIVE | 1 matched cycles"),
            "{text}"
        );
        assert!(text.contains("too few matched cycles"), "{text}");
        assert!(
            text.contains("1 video cycles without a matching event"),
            "{text}"
        );
        assert!(text.contains("audio        minus video: median"), "{text}");
    }
    #[test]
    fn runtime_faults_make_a_run_inconclusive() {
        gst::init().unwrap();
        let events: Vec<_> = (1..=3)
            .flat_map(|i| [event(Stream::Video, i * NS), event(Stream::Audio, i * NS)])
            .collect();
        let r = summarize(&events, false, 20., 250., 3, None, vec![]);
        assert_eq!(r.get::<String>("status").unwrap(), "pass");
        let r = summarize(
            &events,
            false,
            20.,
            250.,
            3,
            None,
            vec!["timeline discontinuity".into()],
        );
        assert_eq!(r.get::<String>("status").unwrap(), "inconclusive");
        assert!(summary_text("check", &r).contains("reason: timeline discontinuity"));
    }
    #[test]
    fn measures_signed_offsets_and_missing_captions() {
        gst::init().unwrap();
        for offset in [-80_000_000i64, 80_000_000] {
            let events: Vec<_> = (1..=4)
                .flat_map(|i| {
                    let t = i * NS;
                    [
                        event(Stream::Video, t),
                        event(Stream::Audio, (t as i64 + offset) as u64),
                        event(Stream::Caption, t),
                    ]
                })
                .collect();
            let r = summarize(&events, true, 20., 250., 3, None, vec![]);
            assert_eq!(r.get::<String>("status").unwrap(), "fail");
            assert_eq!(
                r.get::<gst::Structure>("offsets")
                    .unwrap()
                    .get::<gst::Structure>("audio")
                    .unwrap()
                    .get::<f64>("median_ms")
                    .unwrap(),
                offset as f64 / 1_000_000.
            );
            let missing: Vec<_> = events
                .into_iter()
                .filter(|e| e.stream != Stream::Caption)
                .collect();
            assert_eq!(
                summarize(&missing, true, 20., 250., 3, None, vec![])
                    .get::<String>("status")
                    .unwrap(),
                "inconclusive"
            );
        }
    }
    #[test]
    fn captions_may_lead_by_their_carrying_frame() {
        gst::init().unwrap();
        // 29.97 fps: the caption frame starts up to 33.4 ms before the pip.
        let frame = 33_366_667;
        for (lead, status) in [(30_000_000, "pass"), (60_000_000, "fail")] {
            let events: Vec<_> = (1..=3)
                .flat_map(|i| {
                    let mut caption = event(Stream::Caption, i * NS - lead);
                    caption.uncertainty_ns = frame;
                    [
                        event(Stream::Video, i * NS),
                        event(Stream::Audio, i * NS),
                        caption,
                    ]
                })
                .collect();
            let r = summarize(&events, true, 20., 250., 3, None, vec![]);
            assert_eq!(r.get::<String>("status").unwrap(), status, "{r:#}");
        }
    }
    #[test]
    fn partner_of_a_boundary_cycle_is_not_an_orphan() {
        gst::init().unwrap();
        // Video at 1.24 s is within `window` of the start, so it is excluded;
        // its late audio at 1.28 s is past the start + window edge.
        let late = 40_000_000;
        let events: Vec<_> = (0..5)
            .flat_map(|i| {
                let t = 1_240_000_000 + i * NS;
                [event(Stream::Video, t), event(Stream::Audio, t + late)]
            })
            .collect();
        let r = summarize(&events, false, 50., 250., 3, Some((NS, 6 * NS)), vec![]);
        assert_eq!(r.get::<u64>("boundary_cycles_excluded").unwrap(), 1);
        assert_eq!(r.get::<String>("status").unwrap(), "pass", "{r:#}");
    }
    #[test]
    fn frozen_video_does_not_pass_on_a_few_old_matches() {
        gst::init().unwrap();
        let mut events: Vec<_> = (1..=3).map(|i| event(Stream::Video, i * NS)).collect();
        events.extend((1..=6).map(|i| event(Stream::Audio, i * NS)));
        let r = summarize(&events, false, 20., 250., 3, None, vec![]);
        assert_eq!(r.get::<String>("status").unwrap(), "inconclusive");
        assert_eq!(
            r.get::<gst::Structure>("unmatched_events")
                .unwrap()
                .get::<u64>("audio")
                .unwrap(),
            3
        );
    }
    #[test]
    fn missing_frames_increase_video_uncertainty() {
        gst::init().unwrap();
        let mut tracker = VideoTracker::default();
        tracker.feed(40., 100, NS, NS, 10_000_000);
        let video = tracker
            .feed(60., 100, NS + 100_000_000, NS, 10_000_000)
            .unwrap();
        assert_eq!(video.uncertainty_ns, 50_000_000);
        let audio = event(Stream::Audio, video.media_ns);
        let r = summarize(&[video, audio], false, 20., 250., 1, None, vec![]);
        assert_eq!(r.get::<String>("status").unwrap(), "inconclusive");
    }
}
