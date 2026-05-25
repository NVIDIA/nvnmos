// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared enum types and defaults used by `nmossrc` and `nmossink`.
//!
//! [`Transport`] is exposed as a GObject enum property (`transport`
//! on both elements). [`FlowFormat`] is an internal-only helper used
//! to bridge between caps media-type names and `flow_def.format`
//! URNs — it isn't exposed as a property.

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
/// Internal type bridging between caps media-type names
/// (`video/x-raw`, `audio/x-raw`, `meta/x-st-2038`) and
/// `flow_def.format` URNs (`urn:x-nmos:format:{video,audio,data}`).
/// Used by `nmossrc` to route the resolved `mxl-flow-id` into the
/// matching `mxlsrc` property (`video-flow-id` / `audio-flow-id` /
/// `data-flow-id`). `Unspecified` means the format is not known yet
/// — neither the `caps` property nor a `transport-file` pinned it
/// — and the element falls back to its placeholder data path until
/// a later reconfiguration supplies one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FlowFormat {
    /// Format not pinned.
    #[default]
    Unspecified,
    /// `urn:x-nmos:format:video`.
    Video,
    /// `urn:x-nmos:format:audio`.
    Audio,
    /// `urn:x-nmos:format:data`.
    Data,
}

impl FlowFormat {
    /// `urn:x-nmos:format:*` string for this format, or `None` for
    /// [`FlowFormat::Unspecified`].
    pub(crate) fn as_format_urn(self) -> Option<&'static str> {
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
    pub(crate) fn from_format_urn(s: &str) -> Self {
        match s {
            "urn:x-nmos:format:video" => Self::Video,
            "urn:x-nmos:format:audio" => Self::Audio,
            "urn:x-nmos:format:data" => Self::Data,
            _ => Self::Unspecified,
        }
    }

    /// Map the first structure of a [`gstreamer::Caps`] to its
    /// `FlowFormat`. Mirrors the dispatch in
    /// [`crate::flow_def::build_from_caps`] (`video/x-raw` → Video,
    /// `audio/x-raw` → Audio, `meta/x-st-2038` → Data). Returns
    /// [`FlowFormat::Unspecified`] for empty/ANY caps and for any
    /// other media type — the caller is responsible for falling
    /// back to the placeholder.
    pub(crate) fn from_caps(caps: &gstreamer::Caps) -> Self {
        let Some(structure) = caps.structure(0) else {
            return Self::Unspecified;
        };
        match structure.name().as_str() {
            "video/x-raw" => Self::Video,
            "audio/x-raw" => Self::Audio,
            "meta/x-st-2038" => Self::Data,
            _ => Self::Unspecified,
        }
    }
}

/// Default `daemon-uri` if the user doesn't override it.
///
/// Matches `nvnmosd`'s own default UDS path so a developer can run
/// both with zero configuration.
pub(crate) const DEFAULT_DAEMON_URI: &str = "unix:/tmp/nvnmosd.sock";
