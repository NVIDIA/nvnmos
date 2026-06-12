// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GStreamer essence caps helpers shared by the MXL (`flow_def`) and
//! SDP transport paths.

use gstreamer as gst;

/// Build GStreamer essence caps from parsed transport state: optional
/// fields are filled in where absent (audio `channel-mask` and
/// `channel-order`; video and ANC pass through unchanged).
///
/// `raw_caps` is the minimal essence shape (from [`crate::flow_def::caps_from`]
/// or SDP parse). When `rtp_caps` is supplied, audio `channel-order`
/// is taken from the fmtp slot on the companion `application/x-rtp`
/// caps; otherwise audio defaults use channel count (`M` / `ST` / `Uxx`).
pub(crate) fn caps_from(raw_caps: &gst::Caps, rtp_caps: Option<&gst::Caps>) -> gst::Caps {
    if raw_caps
        .structure(0)
        .is_some_and(|s| s.name() == "audio/x-raw")
    {
        let channel_order = rtp_caps
            .and_then(|rtp| rtp.structure(0))
            .and_then(channel_order_from_rtp_structure);
        audio_caps_from(raw_caps, channel_order.as_deref())
    } else {
        raw_caps.clone()
    }
}

fn audio_caps_from(caps: &gst::Caps, channel_order: Option<&str>) -> gst::Caps {
    let Some(s) = caps.structure(0) else {
        return caps.clone();
    };
    debug_assert_eq!(s.name(), "audio/x-raw");
    let needs_mask = s.get::<gst::Bitmask>("channel-mask").is_err();
    let needs_order = s.get::<&str>("channel-order").is_err();
    if !needs_mask && !needs_order {
        return caps.clone();
    }
    let channels = s.get::<i32>("channels").unwrap_or(1);
    let mut out = caps.clone();
    let out_mut = out.make_mut();
    let Some(s) = out_mut.structure_mut(0) else {
        return out;
    };
    if needs_mask {
        if let Some(mask) = default_channel_mask(channels) {
            s.set("channel-mask", mask);
        }
    }
    if needs_order {
        let order = channel_order
            .map(str::to_owned)
            .unwrap_or_else(|| default_smpte2110_channel_order(channels));
        s.set("channel-order", &order);
    }
    out
}

/// Default SMPTE ST 2110-30 `channel-order` fmtp for a channel count
/// when none is supplied: `M` for mono, `ST` for stereo, and the
/// Undefined group `Uxx` for all other layouts (3–64 channels).
pub(crate) fn default_smpte2110_channel_order(channels: i32) -> String {
    match channels {
        1 => "SMPTE2110.(M)".to_owned(),
        2 => "SMPTE2110.(ST)".to_owned(),
        n if (3..=64).contains(&n) => format!("SMPTE2110.(U{n:02})"),
        _ => "SMPTE2110.(U01)".to_owned(),
    }
}

/// Read `channel-order` from parsed `application/x-rtp` caps (the
/// fmtp `channel-order=` slot from SDP).
pub(crate) fn channel_order_from_rtp_structure(s: &gst::StructureRef) -> Option<String> {
    s.get::<&str>("channel-order")
        .ok()
        .map(str::to_owned)
}

fn default_channel_mask(channels: i32) -> Option<gst::Bitmask> {
    if !(1..=64).contains(&channels) {
        return None;
    }
    let mask = if channels == 64 {
        u64::MAX
    } else {
        (1u64 << channels) - 1
    };
    Some(gst::Bitmask::new(mask))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn init_gst() {
        let _ = gst::init();
    }

    fn raw_audio_caps(format: &str, rate: i32, channels: i32) -> gst::Caps {
        gst::Caps::from_str(&format!(
            "audio/x-raw,format={format},rate={rate},channels={channels},layout=interleaved",
        ))
        .expect("audio caps")
    }

    #[test]
    fn default_smpte2110_channel_order_uses_m_st_or_uxx() {
        assert_eq!(default_smpte2110_channel_order(1), "SMPTE2110.(M)");
        assert_eq!(default_smpte2110_channel_order(2), "SMPTE2110.(ST)");
        assert_eq!(default_smpte2110_channel_order(3), "SMPTE2110.(U03)");
        assert_eq!(default_smpte2110_channel_order(6), "SMPTE2110.(U06)");
        assert_eq!(default_smpte2110_channel_order(8), "SMPTE2110.(U08)");
    }

    #[test]
    fn caps_from_leaves_video_unchanged() {
        init_gst();
        let caps = gst::Caps::from_str(
            "video/x-raw,format=UYVY,width=1920,height=1080,framerate=25/1",
        )
        .expect("video caps");
        let out = caps_from(&caps, None);
        assert_eq!(out.to_string(), caps.to_string());
    }

    #[test]
    fn caps_from_adds_sequential_channel_mask_for_audio() {
        init_gst();
        let caps = raw_audio_caps("S24BE", 48_000, 6);
        let out = caps_from(&caps, None);
        let s = out.structure(0).expect("structure");
        assert_eq!(
            s.get::<gst::Bitmask>("channel-mask").unwrap(),
            gst::Bitmask::new((1u64 << 6) - 1),
        );
    }

    #[test]
    fn caps_from_preserves_explicit_channel_mask() {
        init_gst();
        let caps = gst::Caps::from_str(
            "audio/x-raw,format=S24BE,rate=48000,channels=6,layout=interleaved,\
             channel-mask=(bitmask)0x000000000000000f",
        )
        .expect("caps");
        let out = caps_from(&caps, None);
        let s = out.structure(0).expect("structure");
        assert_eq!(
            s.get::<gst::Bitmask>("channel-mask").unwrap(),
            caps.structure(0).unwrap().get::<gst::Bitmask>("channel-mask").unwrap(),
        );
    }
}
