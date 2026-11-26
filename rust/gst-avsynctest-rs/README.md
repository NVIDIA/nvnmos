<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# AV-sync sources and analyzer

The `avsynctest` plugin contains the phase-locked video/audio generators and
`avsyncanalyzer`, a sink that measures A/V sync on received streams. The analyzer
uses the existing `analyze` helpers to read received video pixels and decode captions.
It never uses the source's expected phase or embedded frame counter to decide
when the video bar crosses the center.

## Build

From the repository's `rust` directory, with the pinned Rust toolchain and
GStreamer development libraries installed:

```sh
cargo build --release -p gst-avsynctest-rs --locked
export GST_PLUGIN_PATH="$PWD/target/release${GST_PLUGIN_PATH:+:$GST_PLUGIN_PATH}"
gst-inspect-1.0 avsyncanalyzer
cargo test -p gst-avsynctest-rs --locked
```

On Linux the native plugin is `target/release/libgstavsynctest.so`; on macOS it
is `target/release/libgstavsynctest.dylib`. The runtime Dockerfile already
builds and installs this library.

## Direct source baseline

This finite example analyzes the real generator's video, audio and ancillary
outputs without RTP, MXL or NMOS:

```sh
gst-launch-1.0 -m \
  avsyncanalyzer name=check require-anc=true \
  avsyncvideotestsrc is-live=true num-buffers=600 \
    width=1920 height=1080 framerate=60/1 \
    ! video/x-raw,format=UYVP,width=1920,height=1080,framerate=60/1 \
    ! st2038extractor name=split remove-ancillary-meta=true \
  split.src ! check.video \
  split.st2038 ! check.anc \
  avsyncaudiotestsrc is-live=true num-buffers=1000 samples-per-buffer=480 \
    ! audio/x-raw,format=F32LE,rate=48000,channels=2,layout=interleaved \
    ! check.audio
```

`st2038extractor`, `st2038anctocc`, and `ccconverter` must be installed for this
ANC path. The source module's tests can also supply CDPs directly on the
`caption` pad without those elements. For A/V only, omit the ANC branch and
leave `require-anc=false`.

## Existing RTP streams and MXL flows

Create three receivers using the same configuration conventions as the
repository's existing `nmossrc` pipelines:

```text
nmossrc (video) -> system-memory raw video -> check.video
nmossrc (audio) -------------------------> check.audio
nmossrc (ANC) ---------------------------> check.anc
```

Use `transport=nvdsudp` or `transport=udp2` with each stream's active SDP and
media-interface IP, or `transport=mxl` with the existing domain/flow IDs.
The analyzer has no transport-specific code. Video in NVMM/GPU memory needs
`nvvideoconvert ! video/x-raw,format=UYVP` before the analyzer.

Set `require-anc=true` when captions are required. This prevents a missing
late-created ANC pad from silently producing an A/V-only pass. Measure ANC on
its independent receiver branch before `st2038combiner`, whose pairing or
dropping behavior could hide the original ancillary offset.

The receiver must reconstruct **one common media clock**. For nvdsudp, use
`use-rtp-timestamp=true` and the same `adjust-leap-seconds` setting on all
receivers; use the realtime pipeline clock where required by the receiver.
Use unbatched audio (`payload-multiple=1` with the appropriate audio RTP packet
geometry) when the installed receiver timestamps a batch using its final
packet's timestamp. The inspected runtime otherwise introduced a 15 ms bias
for a 16-packet, 1 ms audio batch.

Ordinary packet-arrival PTS or independently zeroed streams cannot establish
sender content sync. `timestamp-mode=reference` requires
`GstReferenceTimestampMeta` from the same domain on every input and errors if
it is missing or inconsistent. Reference metadata is captured at each input
before internal conversion. `timestamp-mode=running-time` uses each sample's
PTS and TIME segment; correct sender-clock mapping remains a precondition,
particularly for udp/udp2 receivers.

## Measurement and reports

For every video frame the analyzer reads one scanline, recovers the bright
bar centroid, and detects a left-to-right midpoint crossing. The crossing time
is interpolated between the two frames either side of the midpoint. Native UYVP/v210 decoding reads only that row; it does
not convert or copy the full frame. An absent bar or a discontinuity resets
tracking. Video event uncertainty is one bar column of sweep time between
consecutive frames, or half the gap when frames are missing.

Audio is converted to F32LE internally. Streaming detection finds pip onset,
checks its frequency, and times the event at the center between the first/last
detectable tone samples (the sample-level onset is kept in `details`). The amplitude threshold is
0.02; the bar threshold is 0.86 of the luma range. These target the supplied
white-bar / 1 kHz pip pattern, not arbitrary programme material.

Caption decoding uses the existing `cdp-types` parser and shared service-1
text extraction. Marker packets (`TICK`, `TOCK`, `BEEP`) may span CDPs; an event
is timestamped to the CDP that began its DTVCC packet. This is packet/content
timing, not a caption renderer's presentation time. Invalid CDPs make a run
inconclusive.

The element posts `avsync-event` and `avsync-summary` bus messages as typed
`GstStructure` fields. The summary is emitted at EOS or on transition to READY.
With `report-cycles=N`, an `avsync-interim-summary` (same fields plus
`window_index`, `window_start_ns`, `window_end_ns`) is also posted for every N
consecutive video cycles, so long-running receivers can be watched live. A
window whose streams are not all flowing yet reports `inconclusive`.
Read the summary `status` field for pass/fail: a measurement failure does not change gst-launch's process exit
code. Configuration/streaming errors are normal GStreamer error messages.

- `audio_minus_video_ms`: tone center minus video crossing time (used for pass/fail).
- `audio_onset_minus_video_ms`: beep onset minus video crossing time.
- `caption_minus_video_ms`: caption packet time minus video crossing time. A
  caption is stamped with the frame carrying it, so it may lead by up to one
  frame (its uncertainty) beyond `tolerance-ms` and still pass.
- `*_availability_ms`: relative analyzer-input buffer availability, before
  its internal conversions. These are software observations, not NIC arrival,
  network latency or physical speaker/display measurements.

Positive values mean audio/captions are late. A symmetric 4 ms pip centered
on the video event has center approximately 0 ms and onset approximately 2 ms
early. Reports include per-cycle measurements, min/median/max, absolute p95,
peak-to-peak variation, input-buffer counts and discontinuities.

Events match one-to-one within `max-offset-ms`. Missing required events,
unmatched interior events, excessive frame uncertainty or too few matched
cycles produce `inconclusive`; offsets outside tolerance produce `fail`.
Cycles near capture boundaries are excluded conservatively using the common
input media-time coverage. This works for both paced/live input and unpaced
direct-source tests. Once `history-events` drops old events, the report covers
only the retained window: cycles up to the newest dropped event are excluded as
boundary cycles. Clock/timeline discontinuities prevent a pass.
True offsets must be less than half the configured pattern interval: periodic
signals cannot distinguish an entire-cycle delay from zero.

Timestamps and counts are `guint64` nanoseconds/integers and measurements are
`gdouble` milliseconds. `reasons` (a `GstValueArray` of strings) explains a
non-pass status, and `cycles` holds one structure per matched cycle. Empty
distributions and unavailable optional fields are omitted.

When `print-summary` is true (the default) the element also prints a short
human-readable summary to stdout alongside the bus message, for example:

```text
check: FAIL | 12 matched cycles, tolerance 20 ms
  reason: 12 cycles out of tolerance
  audio        minus video: median +41.98 ms, range [+41.50, +42.40], p95 |x| 42.30 ms
  audio_onset  minus video: median +39.98 ms, range [+39.50, +40.40], p95 |x| 40.30 ms
```

Applications read fields using GStreamer's structure API and may export JSON
or write report files themselves; set `print-summary=false` when embedding.
`gst-launch-1.0 -m` prints the full native bus structures.

## Properties

Set properties before PLAYING:

| Property | Default | Meaning |
| --- | --- | --- |
| `tolerance-ms` | 20 | Maximum absolute audio-center/caption offset for pass |
| `max-offset-ms` | 250 | Pairing window; less than half `interval-ms` |
| `interval-ms` | 1000 | Actual generator pip/sweep period |
| `min-events` | 3 | Minimum matched video cycles |
| `history-events` | 6000 | Maximum retained detected events |
| `scanline` | -1 | Video row; -1 selects the middle |
| `audio-channel` | 0 | Zero-based audio channel |
| `beep-frequency` | 1000 | Expected pip frequency in Hz |
| `require-anc` | false | Require captions even if the ANC pad never appears |
| `print-summary` | true | Print a human-readable summary to stdout at the end |
| `report-cycles` | 0 | Also report every N video cycles; 0 reports only at the end |
| `timestamp-mode` | running-time | `running-time` or `reference` |
| `stats` | read-only | `GstStructure` with nested buffer counts and invalid-CDP count |

Request sink pads are `video`, `audio`, optional `anc` (`meta/x-st-2038`), or
optional `caption` (`closedcaption/x-cea-708,format=cdp`). Use either ANC or CDP,
not both. Video and audio must be linked before PLAYING. Caption/ANC pads may
appear later, as they do with dynamic ancillary extractors. Unused optional
pads are not created and do not prevent EOS. History and input-stamp retention
are bounded; long-running reports cover the retained event window.
