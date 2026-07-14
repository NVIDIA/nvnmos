// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GStreamer essence caps helpers shared by the MXL (`flow_def`) and
//! SDP transport paths.

use gstreamer as gst;

/// Build GStreamer essence caps from parsed transport state: optional
/// fields are filled in where absent (audio `channel-mask` and
/// `channel-order`; video and ANC pass through unchanged).
///
/// `caps` is the minimal essence shape (from [`crate::flow_def::caps_from`]
/// or SDP parse). When `rtp_caps` is supplied, audio `channel-order`
/// is taken from the fmtp slot on the companion `application/x-rtp`
/// caps; otherwise audio defaults use channel count (`M` / `ST` / `Uxx`).
pub(crate) fn caps_from(caps: &gst::Caps, rtp_caps: Option<&gst::Caps>) -> gst::Caps {
    if caps.structure(0).is_some_and(|s| s.name() == "audio/x-raw") {
        let channel_order = rtp_caps
            .and_then(|rtp| rtp.structure(0))
            .and_then(channel_order_from_rtp_structure);
        audio_caps_from(caps, channel_order.as_deref())
    } else {
        caps.clone()
    }
}

/// Copy the caps features (e.g. `memory:NVMM`) from `source`'s first
/// structure onto a clone of `base`.
///
/// Transport files (SDP, MXL `flow_def`) have no representation for
/// caps features, so essence caps reconstructed from a transport file
/// have an empty feature set (which GStreamer treats as the default
/// system-memory feature). When a `caps` property requests a non-default
/// feature set, this re-attaches it onto the file-derived essence caps
/// that configure the inner element. A `source` with an empty feature
/// set or `ANY` leaves `base` unchanged.
pub(crate) fn overlay_features(base: &gst::Caps, source: Option<&gst::Caps>) -> gst::Caps {
    let Some(features) = source.and_then(|c| c.features(0)) else {
        return base.clone();
    };
    // No structure 0 to attach to (empty or any base), or nothing to
    // overlay (empty or any source feature set).
    if base.structure(0).is_none() || features.is_any() || features.is_empty() {
        return base.clone();
    }
    let features = features.to_owned();
    let mut out = base.clone();
    out.make_mut().set_features(0, Some(features));
    out
}

/// Clone `caps` with every structure's feature set emptied (the
/// system-memory default). The essence-shape cross-check intersects
/// the `caps` property against caps parsed from the transport file;
/// because the file cannot express features, a requested feature must
/// be dropped here so it does not read as a shape mismatch. Complements
/// [`overlay_features`].
pub(crate) fn without_features(caps: &gst::Caps) -> gst::Caps {
    let mut out = caps.clone();
    let out_mut = out.make_mut();
    for i in 0..out_mut.size() {
        out_mut.set_features(i, Some(gst::CapsFeatures::new_empty()));
    }
    out
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
    s.get::<&str>("channel-order").ok().map(str::to_owned)
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
    use crate::test_support::init_gst;

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
        let caps =
            gst::Caps::from_str("video/x-raw,format=UYVY,width=1920,height=1080,framerate=25/1")
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
    fn overlay_features_reattaches_non_default_feature() {
        init_gst();
        let base = gst::Caps::from_str("video/x-raw,format=UYVY,width=1920,height=1080")
            .expect("base caps");
        let source =
            gst::Caps::from_str("video/x-raw(memory:NVMM),format=UYVY").expect("source caps");
        let out = overlay_features(&base, Some(&source));
        assert!(out.features(0).unwrap().contains("memory:NVMM"));
        // essence fields untouched
        assert_eq!(out.structure(0).unwrap().get::<i32>("width").unwrap(), 1920);
    }

    #[test]
    fn overlay_features_ignores_system_memory_and_none() {
        init_gst();
        let base = gst::Caps::from_str("video/x-raw,format=UYVY").expect("base caps");
        let plain = gst::Caps::from_str("video/x-raw,format=UYVY").expect("plain caps");
        assert_eq!(overlay_features(&base, None).to_string(), base.to_string());
        assert_eq!(
            overlay_features(&base, Some(&plain)).to_string(),
            base.to_string(),
        );
    }

    #[test]
    fn without_features_resets_to_system_memory() {
        init_gst();
        let caps = gst::Caps::from_str("video/x-raw(memory:NVMM),format=UYVY").expect("caps");
        let out = without_features(&caps);
        assert!(out.features(0).unwrap().is_empty());
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
            caps.structure(0)
                .unwrap()
                .get::<gst::Bitmask>("channel-mask")
                .unwrap(),
        );
    }
}
