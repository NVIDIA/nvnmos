// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GObject enum types and defaults shared between `nmossrc` and
//! `nmossink`.

use gstreamer::glib;

/// Transport family for the inner data path.
///
/// Today only MXL shared-memory transport is wired up. Additional
/// variants (UDP/RTP, nvdsudp/Rivermax) are added by follow-up
/// branches as their inner chains land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, glib::Enum)]
#[repr(i32)]
#[enum_type(name = "GstNmosTransport")]
pub enum Transport {
    /// MXL shared-memory transport (`mxlsrc` and `mxlsink`).
    #[default]
    #[enum_value(name = "MXL shared-memory transport", nick = "mxl")]
    Mxl = 0,
}

/// NMOS Flow format family carried in `flow_def.format`.
///
/// Used by `nmossrc` to route the resolved `mxl-flow-id` into the
/// matching `mxlsrc` property (`video-flow-id` / `audio-flow-id` /
/// `data-flow-id`). `Unspecified` means the format is not known yet
/// — neither the property nor a `transport-file` pinned it — and the
/// element falls back to its placeholder data path until a later
/// reconfiguration supplies one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, glib::Enum)]
#[repr(i32)]
#[enum_type(name = "GstNmosFlowFormat")]
pub enum FlowFormat {
    /// Format not pinned. Default for `mxl-flow-format`.
    #[default]
    #[enum_value(name = "Unspecified", nick = "unspecified")]
    Unspecified = 0,
    /// `urn:x-nmos:format:video`.
    #[enum_value(name = "Video", nick = "video")]
    Video = 1,
    /// `urn:x-nmos:format:audio`.
    #[enum_value(name = "Audio", nick = "audio")]
    Audio = 2,
    /// `urn:x-nmos:format:data`.
    #[enum_value(name = "Data", nick = "data")]
    Data = 3,
}

impl FlowFormat {
    /// `urn:x-nmos:format:*` string for this format, or `None` for
    /// [`FlowFormat::Unspecified`].
    pub fn as_format_urn(self) -> Option<&'static str> {
        match self {
            Self::Unspecified => None,
            Self::Video => Some("urn:x-nmos:format:video"),
            Self::Audio => Some("urn:x-nmos:format:audio"),
            Self::Data => Some("urn:x-nmos:format:data"),
        }
    }

    /// Parse a `urn:x-nmos:format:*` string. Unknown formats map to
    /// [`FlowFormat::Unspecified`] so the element falls through to
    /// its placeholder path rather than failing hard.
    pub fn from_format_urn(s: &str) -> Self {
        match s {
            "urn:x-nmos:format:video" => Self::Video,
            "urn:x-nmos:format:audio" => Self::Audio,
            "urn:x-nmos:format:data" => Self::Data,
            _ => Self::Unspecified,
        }
    }
}

/// Default `daemon-uri` if the user doesn't override it.
///
/// Matches `nvnmosd`'s own default UDS path so a developer can run
/// both with zero configuration.
pub(crate) const DEFAULT_DAEMON_URI: &str = "unix:/tmp/nvnmosd.sock";
