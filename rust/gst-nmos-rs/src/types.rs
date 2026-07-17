// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared enum types and defaults used by `nmossrc` and `nmossink`.
//!
//! [`Transport`] is exposed as a GObject enum property (`transport`
//! on both elements). [`CapsMode`] is exposed as a GObject enum
//! property on `nmossrc` as `receiver-caps-mode`. [`FlowFormat`] is
//! an internal-only helper used to bridge between caps media-type
//! names and `flow_def.format` URNs — it isn't exposed as a property.

use gstreamer::glib;

/// Transport family for the inner data path.
///
/// [`Transport::Mxl`] uses the MXL shared-memory pair (`mxlsrc` /
/// `mxlsink`). [`Transport::Udp`] and [`Transport::Udp2`] both
/// drive an RTP-over-UDP chain (RFC 4175 video, ST 2110-30
/// audio, RFC 8331 / ST 2110-40 ancillary), differing only in
/// which factory family is preferred when both are installed:
/// `Udp` picks gst-plugins-good (`udpsrc` / `udpsink` + the
/// classic `rtpvrawpay` / `rtpL24pay` / … line-up) while `Udp2`
/// prefers gst-plugins-rs' newer high-performance siblings
/// (`udpsrc2` + the `*pay2` / `*depay2` family) and falls back
/// per-element to the V1 form for anything without a V2 sibling
/// yet — see [`crate::session::UdpVariant`] for the dispatch
/// detail. [`Transport::NvDsUdp`] uses DeepStream's `nvdsudp*`
/// family (kernel-bypass plus PTP-aligned timing for strict ST 2110).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, glib::Enum)]
#[repr(i32)]
#[enum_type(name = "GstNmosTransport")]
pub enum Transport {
    /// MXL shared-memory transport (`mxlsrc` and `mxlsink`).
    #[default]
    #[enum_value(name = "MXL shared-memory transport", nick = "mxl")]
    Mxl = 0,
    /// ST 2110 via gst-plugins-good `udpsrc` / `udpsink` plus
    /// the matching gst-plugins-good RTP (de)payloaders.
    #[enum_value(name = "RTP/UDP via gst-plugins-good", nick = "udp")]
    Udp = 1,
    /// ST 2110 via gst-plugins-rs' `udpsrc2` plus the
    /// gst-plugins-rs `rtp*pay2` / `rtp*depay2` RTP elements where
    /// available, falling back to gst-plugins-good per-element
    /// for any V1-only piece (notably `udpsink` — no `udpsink2`
    /// exists).
    #[enum_value(name = "RTP/UDP via gst-plugins-rs", nick = "udp2")]
    Udp2 = 2,
    /// ST 2110 via DeepStream's `nvdsudpsrc` / `nvdsudpsink` (Rivermax).
    #[enum_value(name = "NvDsUdp / Rivermax (DeepStream nvdsudp*)", nick = "nvdsudp")]
    NvDsUdp = 3,
}

/// How a Receiver advertises BCP-004-01 Receiver Caps on IS-04.
///
/// A **constrained** Receiver publishes `constraint_sets` derived
/// from the configuring transport file. An **unconstrained**
/// Receiver publishes none and accepts any compatible Sender of the
/// same format family. NvNmos encodes unconstrained receivers with
/// the `urn:x-nvnmos:tag:caps` flow-def tag (MXL) or media-level
/// `a=x-nvnmos-caps:` (SDP): presence means unconstrained, absence
/// means constrained. Today only `nmossrc` exposes this enum, as
/// `receiver-caps-mode`.
///
/// [`CapsMode::Auto`] is the default: it leaves the transport-file
/// marker untouched. The result is constrained when the file is
/// present and the marker is absent (and similarly constrained when
/// no transport file is in play, e.g. the caps-synthesis path), and
/// unconstrained when the marker is already present.
/// [`CapsMode::Constrained`] and [`CapsMode::Unconstrained`] force
/// the corresponding shape regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, glib::Enum)]
#[repr(i32)]
#[enum_type(name = "GstNmosCapsMode")]
pub enum CapsMode {
    /// Leave the transport-file caps marker untouched — constrained
    /// when the marker is absent (or no transport file is in play),
    /// unconstrained when the marker is present.
    #[default]
    #[enum_value(
        name = "Leave the transport file's caps marker untouched",
        nick = "auto"
    )]
    Auto = 0,
    /// Force constrained caps (strip the unconstrained marker from
    /// the transport file if present).
    #[enum_value(name = "Constrained Receiver Caps", nick = "constrained")]
    Constrained = 1,
    /// Force unconstrained caps (ensure the unconstrained marker is
    /// present on the transport file).
    #[enum_value(name = "Unconstrained Receiver Caps", nick = "unconstrained")]
    Unconstrained = 2,
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
/// — and the element falls back to its fake chain until a later
/// reconfiguration supplies one.
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
    /// its fake chain rather than failing hard.
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
    /// [`crate::flow_def::from_caps`] (`video/x-raw` → Video,
    /// `audio/x-raw` → Audio, `meta/x-st-2038` → Data). Returns
    /// [`FlowFormat::Unspecified`] for empty/ANY caps and for any
    /// other media type — the caller is responsible for falling
    /// back to the fake chain.
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
