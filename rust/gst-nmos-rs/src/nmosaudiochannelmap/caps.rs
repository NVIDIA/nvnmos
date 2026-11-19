// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared `audio/x-raw` caps helpers for `nmosaudiochannelmap`.

use gstreamer as gst;

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
/// The channels have no known positions. Mono/stereo are left maskless: their
/// positions are implied (canonical mono / FL+FR), so by convention the
/// channel-mask is omitted. More channels are unpositioned (`0x0`), which also
/// suppresses "invalid channel positions" warnings from a count-only
/// multi-channel caps.
pub(crate) fn caps_with_channel_mask(channels: u32) -> gst::Caps {
    crate::essence_caps::with_channel_mask(&caps_with_channel_count(channels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::init_gst;

    #[test]
    fn caps_with_channel_count_fixates_count_only() {
        init_gst();
        let caps = caps_with_channel_count(8);
        let s = caps.structure(0).expect("structure");
        assert_eq!(s.get::<i32>("channels").unwrap(), 8);
        assert!(s.get::<gst::Bitmask>("channel-mask").is_err());
    }

    #[test]
    fn caps_with_channel_mask_fixates_count_and_unpositioned_mask() {
        init_gst();
        let caps = caps_with_channel_mask(8);
        let s = caps.structure(0).expect("structure");
        assert_eq!(s.get::<i32>("channels").unwrap(), 8);
        assert_eq!(
            s.get::<gst::Bitmask>("channel-mask").unwrap(),
            gst::Bitmask::new(0x0),
        );
    }

    #[test]
    fn caps_with_channel_mask_omits_mask_for_mono_and_stereo() {
        init_gst();
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
