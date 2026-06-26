// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared `audio/x-raw` caps helpers for `nmosaudiochannelmap`.

use gstreamer as gst;

/// Sequential channel mask for all channels; matches [`crate::essence_caps`] defaults.
pub(crate) fn sequential_channel_mask(channels: u32) -> Option<gst::Bitmask> {
    let ch = i32::try_from(channels).ok()?;
    if !(1..=64).contains(&ch) {
        return None;
    }
    let mask = if ch == 64 {
        u64::MAX
    } else {
        (1u64 << ch) - 1
    };
    Some(gst::Bitmask::new(mask))
}

/// Minimum `audio/x-raw` caps fixating channel count only. Format, rate, and
/// channel-mask remain open for negotiation with upstream / downstream.
pub(crate) fn caps_with_channel_count(channels: u32) -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("channels", channels as i32)
        .build()
}

/// Minimum `audio/x-raw` caps fixating channel count and mask. Format and rate
/// remain open for negotiation with upstream / downstream.
///
/// Mono/stereo are left maskless: their positions are implied (canonical mono /
/// FL+FR), so by convention the channel-mask is omitted, and a sequential mask
/// for 1 channel would wrongly read as front-left rather than mono. A mask only
/// matters for >2 channels, where it suppresses "invalid channel positions"
/// warnings from a count-only multi-channel caps.
pub(crate) fn caps_with_channel_mask(channels: u32) -> gst::Caps {
    let mut builder = gst::Caps::builder("audio/x-raw").field("channels", channels as i32);
    if channels > 2 {
        if let Some(mask) = sequential_channel_mask(channels) {
            builder = builder.field("channel-mask", mask);
        }
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_with_channel_count_fixates_count_only() {
        let _ = gst::init();
        let caps = caps_with_channel_count(8);
        let s = caps.structure(0).expect("structure");
        assert_eq!(s.get::<i32>("channels").unwrap(), 8);
        assert!(s.get::<gst::Bitmask>("channel-mask").is_err());
    }

    #[test]
    fn caps_with_channel_mask_fixates_count_and_sequential_mask() {
        let _ = gst::init();
        let caps = caps_with_channel_mask(8);
        let s = caps.structure(0).expect("structure");
        assert_eq!(s.get::<i32>("channels").unwrap(), 8);
        assert_eq!(
            s.get::<gst::Bitmask>("channel-mask").unwrap(),
            gst::Bitmask::new(0xff),
        );
    }

    #[test]
    fn caps_with_channel_mask_omits_mask_for_mono_and_stereo() {
        let _ = gst::init();
        for channels in [1u32, 2] {
            let caps = caps_with_channel_mask(channels);
            let s = caps.structure(0).expect("structure");
            assert_eq!(s.get::<i32>("channels").unwrap(), channels as i32);
            assert!(
                s.get::<gst::Bitmask>("channel-mask").is_err(),
                "mono/stereo caps should omit channel-mask (channels={channels})",
            );
        }
    }
}
