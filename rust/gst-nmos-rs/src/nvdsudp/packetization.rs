// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! ST 2110-20 / ST 2110-30 / ST 2110-40 packetization defaults for
//! DeepStream `nvdsudpsrc` / `nvdsudpsink` Mode 3 (built-in RTP
//! (de)payload).
//!
//! Semantics match DeepStream 9.0 docs and `nvds_nmos_bin`'s
//! `configure_nvdsudpsink_for_media_api()`:
//!
//! - `nvdsudpsink.payload-size` includes the 20-byte (video) or
//!   12-byte (audio) RTP + ST 2110 payload header.
//! - `nvdsudpsrc.payload-size` is the raw RTP payload size only.

use anyhow::{Context, bail};
use gstreamer as gst;
use gstreamer::prelude::ObjectExt;

use crate::types::FlowFormat;

/// ST 2110-20 video RTP + payload header size (bytes).
pub(crate) const VIDEO_HEADER_SIZE: u32 = 20;

/// ST 2110-30 audio RTP header size (bytes).
pub(crate) const AUDIO_HEADER_SIZE: u32 = 12;

/// ST 2110-40 RFC 8331 header (12-byte RTP + 8-byte payload header).
///
/// Set on `nvdsudpsrc` only — enables header/data split for ANC depayload.
/// Per-packet sizes are taken from the received packets; `payload-size`
/// stays at the plugin default.
pub(crate) const ANC_HEADER_SIZE: u32 = 20;

/// Default maximum raw RTP payload per video packet (bytes).
///
/// Ethernet MTU 1500 minus IPv4 (20), UDP (8), RTP (12), and the
/// ST 2110-20 single sample-row-data RTP payload header (8):
///
/// `1500 − 20 − 8 − 12 − 8 = 1452`
pub(crate) const DEFAULT_MAX_VIDEO_RTP_PAYLOAD: u32 = 1452;

/// Default `a=ptime:` when SDP omits it — 1 ms.
pub(crate) const DEFAULT_PTIME_NS: u64 = 1_000_000;

/// Default `nvdsudpsrc.payload-multiple` cadence: 16 ms of audio per
/// output buffer when `ptime` is 1 ms (matches nvds_nmos_bin examples).
pub(crate) const DEFAULT_AUDIO_BUFFER_NS: u64 = 16_000_000;

/// Minimum raw bytes per video packet (`nvdsudpsrc.payload-size`).
///
/// ST 2110-20 discourages datagrams under 1000 octets except at field
/// boundaries; Rivermax's smallest known working line split is 800 bytes
/// per packet (1280×720 UYVP, four packets per line). Lowering this would
/// only admit pathological divisor searches, not formats we target.
pub(crate) const MIN_SRC_PAYLOAD_PER_PACKET: u32 = 800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VideoPacketization {
    /// `nvdsudpsink.payload-size` (includes [`VIDEO_HEADER_SIZE`]).
    pub sink_payload_size: u32,
    /// `nvdsudpsrc.payload-size` (excludes header).
    pub src_payload_size: u32,
    /// `nvdsudpsink.packets-per-line`.
    pub packets_per_line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioPacketization {
    /// `nvdsudpsink.payload-size` (includes [`AUDIO_HEADER_SIZE`]).
    pub sink_payload_size: u32,
    /// `nvdsudpsrc.payload-size` (excludes header).
    pub src_payload_size: u32,
    /// `nvdsudpsrc.payload-multiple`.
    pub payload_multiple: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Packetization {
    Video(VideoPacketization),
    Audio(AudioPacketization),
    /// ST 2110-40 ANC — plugin defaults for `payload-size`; src needs
    /// [`ANC_HEADER_SIZE`] only.
    Anc,
}

/// RFC 4175 / ST 2110-20 packing group for a GStreamer `format` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VideoPgroup {
    pixels_per_group: u32,
    bytes_per_group: u32,
}

fn video_pgroup(format: &str) -> Result<VideoPgroup, anyhow::Error> {
    match format {
        // 8-bit 4:2:2 — two bytes per pixel.
        "UYVY" => Ok(VideoPgroup {
            pixels_per_group: 1,
            bytes_per_group: 2,
        }),
        // 10-bit 4:2:2 — five bytes per two pixels (40 bits packed).
        "UYVP" => Ok(VideoPgroup {
            pixels_per_group: 2,
            bytes_per_group: 5,
        }),
        // 8-bit RGB 4:4:4 (ST 2110-20) — three bytes per pixel.
        "RGB" => Ok(VideoPgroup {
            pixels_per_group: 1,
            bytes_per_group: 3,
        }),
        other => bail!("unsupported video format `{other}` for nvdsudp packetization"),
    }
}

/// Derive Mode-3 packetization properties from essence caps and (for
/// audio) the hoisted `a-ptime` field on `application/x-rtp` caps.
pub(crate) fn from_media(
    format: FlowFormat,
    caps: &gst::Caps,
    rtp_caps: &gst::Caps,
) -> Result<Packetization, anyhow::Error> {
    match format {
        FlowFormat::Video => {
            let pkt = video_from_caps(caps, DEFAULT_MAX_VIDEO_RTP_PAYLOAD)?;
            Ok(Packetization::Video(pkt))
        }
        FlowFormat::Audio => {
            let ptime_ns = ptime_ns_from_rtp_caps(rtp_caps)?;
            let pkt = audio_from_caps(caps, ptime_ns)?;
            Ok(Packetization::Audio(pkt))
        }
        FlowFormat::Data => anc_from_caps(caps),
        FlowFormat::Unspecified => {
            bail!("nvdsudp Mode 3 does not support unspecified essence format");
        }
    }
}

/// ST 2110-40 ANC (`meta/x-st-2038`). DeepStream Mode-3 ANC is inherently
/// per-frame, but the `nvdsudpsink`/`nvdsudpsrc` pads are `ANY` so they can't
/// carry that grouping in negotiation; it's materialised on the graph by the
/// `capsfilter` `build_nvdsudpsink` inserts and the output caps `build_nvdsudpsrc`
/// stamps. This build-time check complements them: classify the flow as ANC
/// (drives `header-size`) and reject an *explicit* non-`frame` alignment before a
/// chain is built. An absent alignment is fine —
/// transport-file caps never carry the field (an explicit one can only appear
/// when `caps` are applied directly rather than resynthesised).
pub(crate) fn anc_from_caps(caps: &gst::Caps) -> Result<Packetization, anyhow::Error> {
    let s = caps
        .structure(0)
        .ok_or_else(|| anyhow::anyhow!("ANC raw caps empty"))?;
    if s.name() != "meta/x-st-2038" {
        bail!(
            "nvdsudp ANC expects meta/x-st-2038 caps, got `{}`",
            s.name()
        );
    }
    match s.get::<&str>("alignment") {
        Ok("frame") | Err(_) => {}
        Ok(other) => bail!("nvdsudp ANC requires alignment=frame, got `{other}`"),
    }
    Ok(Packetization::Anc)
}

/// Line stride in bytes for one ST 2110-20 scan line.
///
/// When `width` is not a whole number of packing groups, the line is padded
/// up to the next group boundary (RFC 4175 / ST 2110-20 line padding).
pub(crate) fn video_line_stride(width: u32, format: &str) -> Result<u32, anyhow::Error> {
    let pg = video_pgroup(format)?;
    let padded_width = width.div_ceil(pg.pixels_per_group) * pg.pixels_per_group;
    let groups = padded_width / pg.pixels_per_group;
    groups
        .checked_mul(pg.bytes_per_group)
        .with_context(|| format!("line stride overflow for width={width}, format={format}"))
}

/// Compute video packetization for a `video/x-raw` caps structure.
pub(crate) fn video_from_caps(
    caps: &gst::Caps,
    max_rtp_payload: u32,
) -> Result<VideoPacketization, anyhow::Error> {
    let s = caps
        .structure(0)
        .ok_or_else(|| anyhow::anyhow!("raw video caps empty"))?;
    let format = s
        .get::<&str>("format")
        .context("raw video caps missing `format`")?;
    let width = u32::try_from(
        s.get::<i32>("width")
            .context("raw video caps missing `width`")?,
    )
    .context("`width` must be non-negative")?;
    if width == 0 {
        bail!("raw video caps `width` must be > 0");
    }

    let stride = video_line_stride(width, format)?;
    if stride == 0 {
        bail!("computed line stride is zero");
    }

    let packets_per_line = choose_packets_per_line(stride, max_rtp_payload)?;
    let src_payload_size = stride / packets_per_line;
    let sink_payload_size = src_payload_size + VIDEO_HEADER_SIZE;

    Ok(VideoPacketization {
        sink_payload_size,
        src_payload_size,
        packets_per_line,
    })
}

/// Smallest divisor of `stride` that is >= `ceil(stride / max_payload)` and
/// yields at least [`MIN_SRC_PAYLOAD_PER_PACKET`] bytes per packet.
fn choose_packets_per_line(stride: u32, max_rtp_payload: u32) -> Result<u32, anyhow::Error> {
    if max_rtp_payload == 0 {
        bail!("max_rtp_payload must be > 0");
    }
    let min_ppl = stride.div_ceil(max_rtp_payload);
    let max_ppl = stride / MIN_SRC_PAYLOAD_PER_PACKET;
    if min_ppl > max_ppl {
        bail!(
            "no packets-per-line satisfies stride {stride} with at least \
             {MIN_SRC_PAYLOAD_PER_PACKET} bytes per packet (min ppl {min_ppl}, \
             max ppl {max_ppl})"
        );
    }
    for ppl in min_ppl..=max_ppl {
        if stride % ppl == 0 {
            return Ok(ppl);
        }
    }
    bail!(
        "no packets-per-line divides line stride {stride} between {min_ppl} \
         and {max_ppl}"
    );
}

/// Parse `a-ptime:` from RTP caps (decimal milliseconds) into nanoseconds.
fn ptime_ns_from_rtp_caps(rtp_caps: &gst::Caps) -> Result<u64, anyhow::Error> {
    let Some(s) = rtp_caps.structure(0) else {
        return Ok(DEFAULT_PTIME_NS);
    };
    let Ok(ptime) = s.get::<&str>("a-ptime") else {
        return Ok(DEFAULT_PTIME_NS);
    };
    parse_ptime_ms_as_ns(ptime)
        .with_context(|| format!("parsing a-ptime=`{ptime}` for nvdsudp audio packetization"))
}

/// Parse SDP `a=ptime:` form (decimal ms) into nanoseconds.
fn parse_ptime_ms_as_ns(value: &str) -> Result<u64, anyhow::Error> {
    let v = value.trim();
    if v.is_empty() {
        bail!("empty ptime");
    }
    let ms: f64 = v.parse().with_context(|| format!("ptime `{v}`"))?;
    if !ms.is_finite() || ms <= 0.0 {
        bail!("ptime=`{v}` ms must be a finite positive value");
    }
    let ns = ms * 1_000_000.0;
    if ns > u64::MAX as f64 {
        bail!("ptime=`{v}` ms overflows nanoseconds");
    }
    Ok(ns.round() as u64)
}

fn audio_sample_bytes(format: &str) -> Result<u32, anyhow::Error> {
    match format {
        "S24BE" => Ok(3),
        "S16BE" => Ok(2),
        other => bail!("unsupported audio format `{other}` for nvdsudp packetization"),
    }
}

/// Compute audio packetization for an `audio/x-raw` caps structure.
pub(crate) fn audio_from_caps(
    caps: &gst::Caps,
    ptime_ns: u64,
) -> Result<AudioPacketization, anyhow::Error> {
    if ptime_ns == 0 {
        bail!("ptime_ns must be > 0");
    }
    let s = caps
        .structure(0)
        .ok_or_else(|| anyhow::anyhow!("raw audio caps empty"))?;
    let format = s
        .get::<&str>("format")
        .context("raw audio caps missing `format`")?;
    let rate = u32::try_from(
        s.get::<i32>("rate")
            .context("raw audio caps missing `rate`")?,
    )
    .context("`rate` must be non-negative")?;
    let channels = u32::try_from(
        s.get::<i32>("channels")
            .context("raw audio caps missing `channels`")?,
    )
    .context("`channels` must be non-negative")?;
    if rate == 0 || channels == 0 {
        bail!("audio `rate` and `channels` must be > 0");
    }

    let sample_bytes = audio_sample_bytes(format)?;
    let numerator = u64::from(rate)
        .checked_mul(ptime_ns)
        .and_then(|n| n.checked_mul(u64::from(sample_bytes)))
        .and_then(|n| n.checked_mul(u64::from(channels)))
        .context("audio payload size calculation overflow")?;
    // Round to nearest whole byte when ptime does not divide sample grid exactly.
    let src_payload = ((numerator + 500_000_000) / 1_000_000_000) as u32;
    if src_payload == 0 {
        bail!(
            "computed audio payload size is zero (rate={rate}, ptime_ns={ptime_ns}, \
             format={format}, channels={channels})"
        );
    }
    let sink_payload_size = src_payload + AUDIO_HEADER_SIZE;
    let payload_multiple = ((DEFAULT_AUDIO_BUFFER_NS + ptime_ns / 2) / ptime_ns).max(1) as u32;

    Ok(AudioPacketization {
        sink_payload_size,
        src_payload_size: src_payload,
        payload_multiple,
    })
}

/// Line stride implied by a sink's current `payload-size` and
/// `packets-per-line` (DeepStream includes the ST 2110 header in
/// `payload-size`).
pub(crate) fn video_configured_stride(sink_payload_size: u32, packets_per_line: u32) -> u32 {
    sink_payload_size
        .saturating_sub(VIDEO_HEADER_SIZE)
        .saturating_mul(packets_per_line)
}

/// Target line stride from `video/x-raw` caps.
pub(crate) fn video_target_stride(caps: &gst::Caps) -> Result<u32, anyhow::Error> {
    let s = caps
        .structure(0)
        .ok_or_else(|| anyhow::anyhow!("raw video caps empty"))?;
    let format = s
        .get::<&str>("format")
        .context("raw video caps missing `format`")?;
    let width = u32::try_from(
        s.get::<i32>("width")
            .context("raw video caps missing `width`")?,
    )
    .context("`width` must be non-negative")?;
    video_line_stride(width, format)
}

/// After `transport-properties` may have preset packetization, warn and
/// recalculate when the configured stride does not match the essence
/// (matches `nvds_nmos_bin::configure_nvdsudpsink_for_media_api`).
pub(crate) fn reconcile_sink_video_packetization(
    sink: &gst::Element,
    caps: &gst::Caps,
    cat: &gst::DebugCategory,
    element: &str,
) -> Result<(), anyhow::Error> {
    let calculated = video_from_caps(caps, DEFAULT_MAX_VIDEO_RTP_PAYLOAD)?;
    let target_stride = video_target_stride(caps)?;
    let user_payload: u32 = sink.property("payload-size");
    let user_ppl: u32 = sink.property("packets-per-line");
    let configured_stride = video_configured_stride(user_payload, user_ppl);
    if configured_stride == target_stride {
        return Ok(());
    }
    let s = caps
        .structure(0)
        .ok_or_else(|| anyhow::anyhow!("raw caps empty"))?;
    let format = s.get::<&str>("format").unwrap_or("?");
    let width = s.get::<i32>("width").unwrap_or(0);
    gst::warning!(
        cat,
        "{element}: nvdsudpsink packetization stride {configured_stride} \
         (payload-size={user_payload}, packets-per-line={user_ppl}) does not \
         match essence stride {target_stride} (width={width}, format={format}); \
         recalculating to payload-size={}, packets-per-line={}",
        calculated.sink_payload_size,
        calculated.packets_per_line,
    );
    sink.set_property("payload-size", calculated.sink_payload_size);
    sink.set_property("packets-per-line", calculated.packets_per_line);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use crate::test_support::init_gst;

    #[test]
    fn uyvp_1920x1080_matches_deepstream_table() {
        init_gst();
        let caps =
            gst::Caps::from_str("video/x-raw,format=UYVP,width=1920,height=1080,framerate=60/1")
                .unwrap();
        let pkt = video_from_caps(&caps, DEFAULT_MAX_VIDEO_RTP_PAYLOAD).unwrap();
        assert_eq!(pkt.packets_per_line, 4);
        assert_eq!(pkt.src_payload_size, 1200);
        assert_eq!(pkt.sink_payload_size, 1220);
    }

    #[test]
    fn uyvp_1280x720_matches_rivermax_minimum_packet_size() {
        init_gst();
        let caps =
            gst::Caps::from_str("video/x-raw,format=UYVP,width=1280,height=720,framerate=60/1")
                .unwrap();
        let pkt = video_from_caps(&caps, DEFAULT_MAX_VIDEO_RTP_PAYLOAD).unwrap();
        assert_eq!(pkt.packets_per_line, 4);
        assert_eq!(pkt.src_payload_size, 800);
        assert_eq!(pkt.sink_payload_size, 820);
    }

    #[test]
    fn video_configured_stride_detects_mismatch() {
        init_gst();
        let caps =
            gst::Caps::from_str("video/x-raw,format=UYVP,width=1920,height=1080,framerate=50/1")
                .unwrap();
        let target = video_target_stride(&caps).unwrap();
        let calculated = video_from_caps(&caps, DEFAULT_MAX_VIDEO_RTP_PAYLOAD).unwrap();
        assert_eq!(
            video_configured_stride(calculated.sink_payload_size, calculated.packets_per_line),
            target,
            "auto-calculated packetization must match essence stride",
        );
        assert_ne!(video_configured_stride(1220, 3), target);
    }

    #[test]
    fn uyvp_3840x2160_10bit_matches_calculated_stride() {
        init_gst();
        let caps =
            gst::Caps::from_str("video/x-raw,format=UYVP,width=3840,height=2160,framerate=60/1")
                .unwrap();
        let pkt = video_from_caps(&caps, DEFAULT_MAX_VIDEO_RTP_PAYLOAD).unwrap();
        assert_eq!(pkt.packets_per_line, 8);
        assert_eq!(pkt.src_payload_size, 1200);
        assert_eq!(pkt.sink_payload_size, 1220);
    }

    #[test]
    fn uyvy_1920x1080_8bit_matches_deepstream_table() {
        init_gst();
        let caps =
            gst::Caps::from_str("video/x-raw,format=UYVY,width=1920,height=1080,framerate=60/1")
                .unwrap();
        let pkt = video_from_caps(&caps, DEFAULT_MAX_VIDEO_RTP_PAYLOAD).unwrap();
        assert_eq!(pkt.packets_per_line, 3);
        assert_eq!(pkt.sink_payload_size, 1300);
        assert_eq!(pkt.src_payload_size, 1280);
    }

    #[test]
    fn l24_48k_2ch_ptime_1ms_matches_example() {
        init_gst();
        let caps = gst::Caps::from_str(
            "audio/x-raw,format=S24BE,rate=48000,channels=2,layout=interleaved",
        )
        .unwrap();
        let pkt = audio_from_caps(&caps, 1_000_000).unwrap();
        assert_eq!(pkt.src_payload_size, 288);
        assert_eq!(pkt.sink_payload_size, 300);
        assert_eq!(pkt.payload_multiple, 16);
    }

    #[test]
    fn l24_48k_2ch_ptime_6ms_rounds_payload_multiple() {
        init_gst();
        let caps = gst::Caps::from_str(
            "audio/x-raw,format=S24BE,rate=48000,channels=2,layout=interleaved",
        )
        .unwrap();
        let pkt = audio_from_caps(&caps, 6_000_000).unwrap();
        assert_eq!(pkt.payload_multiple, 3, "round(16 ms / 6 ms) = 3");
    }

    #[test]
    fn l24_48k_2ch_ptime_125us_matches_integer_payload() {
        init_gst();
        let caps = gst::Caps::from_str(
            "audio/x-raw,format=S24BE,rate=48000,channels=2,layout=interleaved",
        )
        .unwrap();
        let ptime_ns = parse_ptime_ms_as_ns("0.125").unwrap();
        let pkt = audio_from_caps(&caps, ptime_ns).unwrap();
        assert_eq!(pkt.src_payload_size, 36);
        assert_eq!(pkt.sink_payload_size, 48);
    }

    #[test]
    fn anc_from_meta_st2038_frame_alignment() {
        init_gst();
        let caps = gst::Caps::from_str("meta/x-st-2038,alignment=frame,framerate=60/1").unwrap();
        let pkt = from_media(FlowFormat::Data, &caps, &gst::Caps::new_empty())
            .expect("ANC packetization");
        assert!(matches!(pkt, Packetization::Anc));
    }

    #[test]
    fn anc_accepts_missing_alignment() {
        init_gst();
        // Transport-file caps omit `alignment` (a buffer-grouping detail); the
        // classifier tolerates that — the per-frame grouping is enforced on the
        // graph by the nvdsudp capsfilter / output caps, not here.
        let caps = gst::Caps::from_str("meta/x-st-2038,framerate=60/1").unwrap();
        let pkt = from_media(FlowFormat::Data, &caps, &gst::Caps::new_empty())
            .expect("ANC packetization with no alignment");
        assert!(matches!(pkt, Packetization::Anc));
    }

    #[test]
    fn anc_rejects_non_frame_alignment() {
        init_gst();
        let caps = gst::Caps::from_str("meta/x-st-2038,alignment=packets").unwrap();
        assert!(from_media(FlowFormat::Data, &caps, &gst::Caps::new_empty()).is_err());
    }

    #[test]
    fn unsupported_video_format_errors() {
        init_gst();
        let caps =
            gst::Caps::from_str("video/x-raw,format=v210,width=1920,height=1080,framerate=60/1")
                .unwrap();
        assert!(video_from_caps(&caps, DEFAULT_MAX_VIDEO_RTP_PAYLOAD).is_err());
    }
}
