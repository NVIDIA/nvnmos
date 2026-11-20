// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Video colorimetry helpers shared by the MXL (`flow_def`) and SDP
//! transport paths.
//!
//! The flow_def attributes `colorspace`, `transfer_characteristic` and
//! `component_range` take the same values as the ST 2110-20 fmtp
//! parameters `colorimetry`, `TCS` and `RANGE`. For the values it
//! recognises, the mapping to and from GStreamer matches gst-plugins-rs
//! `rtpvrawpay2` / `rtpvrawdepay2` and gst-mxl-rs `mxlsink` / `mxlsrc`.
//! It returns `None` for any other value.

use gstreamer_video as gst_video;
use gstreamer_video::{
    VideoColorMatrix, VideoColorPrimaries, VideoColorRange, VideoColorimetry, VideoTransferFunction,
};

/// The colorspace, transfer characteristic and component range of a
/// video flow.
pub(crate) struct Colorimetry {
    pub(crate) colorspace: &'static str,
    pub(crate) transfer_characteristic: &'static str,
    pub(crate) component_range: &'static str,
}

/// The values for a GStreamer `colorimetry`, or `None` when they cannot
/// express it. `mxlsink` refuses the same colorimetries.
pub(crate) fn from_gst(colorimetry: &gst_video::VideoColorimetry) -> Option<Colorimetry> {
    let component_range = match colorimetry.range() {
        VideoColorRange::Range16_235 => "NARROW",
        VideoColorRange::Range0_255 => "FULL",
        _ => return None,
    };
    let transfer_characteristic = match colorimetry.transfer() {
        VideoTransferFunction::Bt709
        | VideoTransferFunction::Bt601
        | VideoTransferFunction::Smpte240m
        | VideoTransferFunction::Bt202010
        | VideoTransferFunction::Bt202012 => "SDR",
        VideoTransferFunction::Smpte2084 => "PQ",
        VideoTransferFunction::AribStdB67 => "HLG",
        _ => return None,
    };
    let colorspace = match (colorimetry.matrix(), colorimetry.primaries()) {
        (VideoColorMatrix::Bt601, VideoColorPrimaries::Smpte170m) => "BT601",
        (VideoColorMatrix::Bt709, VideoColorPrimaries::Bt709) => "BT709",
        (VideoColorMatrix::Smpte240m, VideoColorPrimaries::Smpte240m) => "SMPTE240M",
        (VideoColorMatrix::Bt2020, VideoColorPrimaries::Bt2020)
            if transfer_characteristic == "SDR" =>
        {
            "BT2020"
        }
        (VideoColorMatrix::Bt2020, VideoColorPrimaries::Bt2020) => "BT2100",
        _ => return None,
    };
    Some(Colorimetry {
        colorspace,
        transfer_characteristic,
        component_range,
    })
}

/// The GStreamer `colorimetry` for a colorspace, transfer characteristic
/// and component range, compared case-insensitively, or `None` when
/// GStreamer has no equivalent (for example `XYZ` or `LINEAR`).
///
/// `depth` is the bit depth. It selects between the BT.2020 SDR
/// transfer functions, which GStreamer names `bt2020` (12 bits or more)
/// and `bt2020-10`. `BT2100` with an SDR transfer gives the same values.
/// `FULLPROTECT` has no GStreamer equivalent and is taken as full range.
pub(crate) fn to_gst(
    colorspace: &str,
    transfer_characteristic: &str,
    component_range: &str,
    depth: u32,
) -> Option<VideoColorimetry> {
    let colorspace = colorspace.to_ascii_uppercase();
    let (matrix, primaries) = match colorspace.as_str() {
        "BT601-5" | "BT601" => (VideoColorMatrix::Bt601, VideoColorPrimaries::Smpte170m),
        "BT709-2" | "BT709" => (VideoColorMatrix::Bt709, VideoColorPrimaries::Bt709),
        "SMPTE240M" => (VideoColorMatrix::Smpte240m, VideoColorPrimaries::Smpte240m),
        "BT2020" | "BT2100" => (VideoColorMatrix::Bt2020, VideoColorPrimaries::Bt2020),
        _ => return None,
    };
    let transfer = match transfer_characteristic.to_ascii_uppercase().as_str() {
        "SDR" => match matrix {
            VideoColorMatrix::Bt601 => VideoTransferFunction::Bt601,
            VideoColorMatrix::Bt709 => VideoTransferFunction::Bt709,
            VideoColorMatrix::Smpte240m => VideoTransferFunction::Smpte240m,
            _ if depth >= 12 => VideoTransferFunction::Bt202012,
            _ => VideoTransferFunction::Bt202010,
        },
        "PQ" => VideoTransferFunction::Smpte2084,
        "HLG" => VideoTransferFunction::AribStdB67,
        _ => return None,
    };
    let range = match component_range.to_ascii_uppercase().as_str() {
        "NARROW" => VideoColorRange::Range16_235,
        "FULL" | "FULLPROTECT" => VideoColorRange::Range0_255,
        _ => return None,
    };
    Some(VideoColorimetry::new(range, matrix, transfer, primaries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::init_gst;

    fn from_gst_str(colorimetry: &str) -> Option<(&'static str, &'static str, &'static str)> {
        let colorimetry: VideoColorimetry = colorimetry.parse().expect(colorimetry);
        from_gst(&colorimetry).map(|c| (c.colorspace, c.transfer_characteristic, c.component_range))
    }

    fn to_gst_string(colorspace: &str, tcs: &str, range: &str, depth: u32) -> Option<String> {
        to_gst(colorspace, tcs, range, depth).map(|c| c.to_string())
    }

    #[test]
    fn from_gst_pins_each_value() {
        init_gst();
        for (colorimetry, expected) in [
            ("bt601", ("BT601", "SDR", "NARROW")),
            ("bt709", ("BT709", "SDR", "NARROW")),
            ("smpte240m", ("SMPTE240M", "SDR", "NARROW")),
            // both BT.2020 transfers collapse to one value; depth is separate
            ("bt2020", ("BT2020", "SDR", "NARROW")),
            ("bt2020-10", ("BT2020", "SDR", "NARROW")),
            ("bt2100-pq", ("BT2100", "PQ", "NARROW")),
            ("bt2100-hlg", ("BT2100", "HLG", "NARROW")),
            // full range BT.709
            ("1:3:5:1", ("BT709", "SDR", "FULL")),
        ] {
            assert_eq!(from_gst_str(colorimetry), Some(expected), "{colorimetry}");
        }
    }

    #[test]
    fn from_gst_rejects_values_it_cannot_express() {
        init_gst();
        assert_eq!(from_gst_str("sRGB"), None);
        // BT.601 625-line primaries
        assert_eq!(from_gst_str("2:4:5:3"), None);
    }

    #[test]
    fn to_gst_bt2020_sdr_is_depth_aware() {
        init_gst();
        for colorspace in ["BT2020", "BT2100"] {
            assert_eq!(
                to_gst_string(colorspace, "SDR", "NARROW", 10).as_deref(),
                Some("bt2020-10"),
            );
            assert_eq!(
                to_gst_string(colorspace, "SDR", "NARROW", 12).as_deref(),
                Some("bt2020"),
            );
        }
    }

    #[test]
    fn to_gst_full_range_and_case() {
        init_gst();
        assert_eq!(
            to_gst_string("bt709", "sdr", "full", 10).as_deref(),
            Some("1:3:5:1")
        );
        assert_eq!(
            to_gst_string("BT709", "SDR", "FULLPROTECT", 10).as_deref(),
            Some("1:3:5:1"),
        );
        assert_eq!(to_gst_string("BT709", "SDR", "UNKNOWN", 10), None);
    }

    #[test]
    fn to_gst_round_trips_from_gst() {
        init_gst();
        for (colorimetry, depth) in [
            ("bt601", 10),
            ("bt709", 10),
            ("smpte240m", 10),
            ("bt2020", 12),
            ("bt2020-10", 10),
            ("bt2100-pq", 10),
            ("bt2100-hlg", 10),
            ("1:3:5:1", 10),
        ] {
            let gst_colorimetry: VideoColorimetry = colorimetry.parse().expect(colorimetry);
            let values = from_gst(&gst_colorimetry).expect(colorimetry);
            let back = to_gst(
                values.colorspace,
                values.transfer_characteristic,
                values.component_range,
                depth,
            )
            .expect(colorimetry);
            assert_eq!(back, gst_colorimetry, "{colorimetry}");
        }
    }
}
