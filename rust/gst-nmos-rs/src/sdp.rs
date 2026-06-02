// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! SDP transport-file helpers.
//!
//! The NvNmos transport file for `Transport::Udp` / `Transport::Udp2`
//! is an SDP document (per AMWA IS-04 / IS-05 + the SMPTE ST 2110
//! profiles). This module is the UDP-flavoured analogue of
//! [`crate::flow_def`]: it parses a transport-file SDP into a
//! [`UdpMedia`] that the chain factories can consume, and builds a
//! *configuring* SDP from a [`UdpMedia`] that the element hands to
//! `nvnmosd` at `AddSender` / `AddReceiver` time. The daemon owns
//! the IS-04 / IS-05 publication; this module never writes
//! anything on the wire.
//!
//! Parsing uses `gstreamer-sdp`'s `SDPMessage::parse_buffer` plus
//! `SDPMedia::caps_from_media` for the RTP caps. Session-level
//! attributes are folded onto those caps with
//! `SDPMessage::attributes_to_caps`, mirroring the convention
//! `nvds_nmos_bin/src/helpers/sdp_helpers.cpp::parse_sdp`
//! established. IS-05 network params (`destination_ip`,
//! `destination_port`, `interface_ip`, `source_ip`, `source_port`)
//! are parsed from the `m=` and `c=` lines and from
//! `a=source-filter:`, `a=x-nvnmos-iface-ip:`, and
//! `a=x-nvnmos-src-port:`. `a=x-nvnmos-iface:` (IS-04 interface
//! identity) is emitted on the synthesis / override paths when a local
//! NIC IP resolves, but is not read back into [`UdpMedia`].
//!
//! Today the module handles single-media SDPs only. ST 2022-7
//! dual-media SDPs are detected and rejected with a clearly-
//! attributed error; redundancy parsing lands when the property
//! surface for the secondary leg is designed.
//!
//! Essence coverage so far:
//!   * Video: RFC 4175 `encoding-name=raw`,
//!     `sampling=YCbCr-4:2:2` at `depth=10` (→ `format=UYVP`) and
//!     `depth=8` (→ `format=UYVY`); other samplings and bit-depths
//!     (RGB/RGBA/BGR/BGRA, YCbCr-4:4:4, YCbCr-4:2:0, YCbCr-4:1:1)
//!     are not yet handled. (The RFC 4175 §6.7 BNF spells the
//!     encoding name lower-case; gst-sdp normalises rtpmap
//!     encoding-name to upper-case on parse, so internal caps
//!     carry `RAW` once a synthesised SDP has been round-tripped
//!     through [`parse_sdp`].)
//!   * Audio: ST 2110-30 / RFC 3190 `L24` (→ `S24BE`) and
//!     RFC 3551 `L16` (→ `S16BE`); `L8` is intentionally
//!     unsupported (out of scope for ST 2110-30).
//!   * ANC: RFC 8331 `encoding-name=SMPTE291` over
//!     `m=video` (SMPTE ST 2110-40), producing
//!     `meta/x-st-2038, alignment=frame[, framerate=N/D]`.
//!     Conversion happens via the
//!     [`rtpsmpte291pay`](https://gstreamer.freedesktop.org/documentation/rsrtp/rtpsmpte291pay.html)
//!     / `rtpsmpte291depay` elements from `gst-plugins-rs`'
//!     `rsrtp` plugin; there is no gst-plugins-good equivalent.
//!
//! `a=ptime:` / `a=maxptime:` are surfaced on the RTP caps as
//! `a-ptime` / `a-maxptime` so that `set_media_from_caps`
//! round-trips them as standalone `a=…:` lines on build — see
//! [`raw_caps_from_rtp_audio`] for the reasoning.


use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::LazyLock;

use gst_sdp::{SDPAttribute, SDPMedia, SDPMediaRef, SDPMessage};
use unicase::Ascii;
use gstreamer as gst;
use gstreamer_sdp as gst_sdp;
use thiserror::Error;

use crate::session::{Side, UdpLeg, UdpMedia};
use crate::types::{CapsMode, FlowFormat};

/// Canonical default values for SDP synthesis, sourced from
/// `nmos-cpp` so that the configuring SDPs produced by the
/// future `synthesise_or_passthrough_udp` (when no
/// `transport-file*` is set and no overriding GObject property
/// supplies a value) agree with what a peer nmos-cpp
/// implementation would emit.
///
/// Precedence model for SDP configuration at startup (see the
/// module-level doc for the full description):
///
/// ```text
/// Layer 1 (RTP overlay), lowest -> highest priority:
///     defaults  <  transport-file*  <  transport-caps  <
///     GObject properties
///
/// Layer 2 (essence cross-check):
///     `caps` must agree with the resolved Layer-1 state for
///     fixed-rate essence (video framerate / clock-rate,
///     RFC 8331 ANC clock-rate, width/height/colorimetry/PAR);
///     audio clock-rate / ptime / payload-type are Layer-1
///     overrideable.
///
/// Activation SDP supersedes Layers 1 + 2 at activation time.
/// ```
///
/// Each constant is consumed somewhere in the synthesis
/// stack: [`MULTICAST_TTL`](self::defaults::MULTICAST_TTL) by
/// [`build_sdp`], the per-essence payload-type constants by
/// [`resolve_payload_type`], [`AUDIO_PTIME_NS`](self::defaults::AUDIO_PTIME_NS)
/// by [`resolve_audio_ptime`], [`RTP_PORT`](self::defaults::RTP_PORT)
/// by [`udp_leg_from_input`], [`ORIGIN_ADDRESS`](self::defaults::ORIGIN_ADDRESS)
/// by [`from_caps`], and the video / ANC constants by
/// [`rtp_caps_from_raw_video`] / [`rtp_caps_from_raw_data`].
pub(crate) mod defaults {
    /// Default RTP payload type for `video/x-raw` (RFC 4175,
    /// ST 2110-20). Matches the nmos-cpp default.
    pub(crate) const VIDEO_PAYLOAD_TYPE: i32 = 96;

    /// Default RTP payload type for `audio/x-raw` (ST 2110-30
    /// L24/L16 PCM). Matches the nmos-cpp default.
    pub(crate) const AUDIO_PAYLOAD_TYPE: i32 = 97;

    /// Default RTP payload type for `meta/x-st-2038`
    /// (RFC 8331 SMPTE 291 ANC, ST 2110-40). Matches the
    /// nmos-cpp default. (98 is reserved for SMPTE ST 2022-6
    /// muxed RTP and 99 is unassigned, so the numbering is
    /// non-contiguous with video/audio.)
    pub(crate) const ANC_PAYLOAD_TYPE: i32 = 100;

    /// Default audio packet duration: 1 ms (1_000_000 ns) —
    /// the canonical ST 2110-30 packet duration and the
    /// nmos-cpp default. ST 2110-30 also permits 0.125 ms
    /// (125_000 ns) for very-low-latency audio; request it
    /// explicitly via `transport-caps` `a-ptime=0.125`.
    ///
    /// Stored in nanoseconds because that's what the GStreamer
    /// `rtp*pay` `min-ptime` / `max-ptime` properties accept;
    /// the SDP `a=ptime:` slot emits it as decimal
    /// milliseconds.
    pub(crate) const AUDIO_PTIME_NS: u64 = 1_000_000;

    /// Default RTP port: 5004. Matches IS-05's `auto_rtp_port`
    /// — i.e. the value the daemon substitutes for the
    /// `auto` sentinel in transport_params at activation
    /// time — and is the de-facto SMPTE ST 2110 convention.
    pub(crate) const RTP_PORT: u16 = 5004;

    /// `o=` line `<unicast-address>` fallback used before any
    /// caps negotiation pins a NIC. Per RFC 4566 §5.2 this
    /// slot may carry any address when the originator's real
    /// address isn't known; `0.0.0.0` is the canonical
    /// "unspecified" form. `build_sdp` substitutes the real
    /// address once an `interface_ip` / `source_ip` property
    /// or activation SDP resolves one.
    pub(crate) const ORIGIN_ADDRESS: &str = "0.0.0.0";

    /// TTL applied when the `c=` line address is multicast.
    /// Per RFC 4566 §5.7 the `<addr>/<ttl>` suffix MUST be
    /// present for IPv4 multicast and MUST be omitted for
    /// unicast — `build_sdp` relies on `gst-sdp`'s
    /// `add_connection` to suppress the suffix automatically
    /// for unicast destinations (pinned by
    /// `gst_sdp_strips_ttl_for_unicast_c_lines`), so this
    /// constant only takes effect for multicast `c=` lines.
    pub(crate) const MULTICAST_TTL: u32 = 64;

    /// RTP clock rate for RFC 4175 video (ST 2110-20 §6.2).
    /// Fixed at 90 kHz regardless of frame rate, depth, or
    /// sampling.
    pub(crate) const VIDEO_CLOCK_RATE: i32 = 90_000;

    /// RTP clock rate for RFC 8331 SMPTE 291 ANC
    /// (ST 2110-40 §4.4). Locked to 90 kHz so the same RTP
    /// timestamp lattice as the paired video flow is reused.
    pub(crate) const ANC_CLOCK_RATE: i32 = 90_000;

    /// ST 2110-20 §6.4 fmtp `PM=` slot. `2110GPM` is the
    /// General Packing Mode and the only value emitted by
    /// nmos-cpp's `make_video_sdp_parameters` /
    /// `nvds_nmos_bin`'s `sdp_from_caps`. The alternate
    /// `2110BPM` (Block Packing Mode) is technically valid
    /// but unused in practice.
    pub(crate) const ST2110_20_PM: &str = "2110GPM";

    /// ST 2110-20 §6.4 fmtp `SSN=` slot. Identifies the SDP
    /// specification revision the sender conforms to.
    /// `ST2110-20:2017` is the only published value and what
    /// nmos-cpp emits.
    pub(crate) const ST2110_20_SSN: &str = "ST2110-20:2017";

    // ST 2110-21 §8.1 `TP=` (video traffic profile) is intentionally omitted
    // from caps-only **video** synthesis for now — see
    // [`rtp_caps_from_raw_video`]. Without `TP=2110TPW` or `TP=2110TPN`
    // the ST 2110-20 video fmtp is not strictly valid per ST 2110-21.
    // We might want to pick the value based on the transport family
    // (e.g. `2110TPW` for `udp` / `udp2`, `2110TPN` for `nvdsudp`) per the
    // nvnmosd design notes.

    /// ST 2110-20 §6.4 fmtp `colorimetry=` default. The
    /// fmtp parameter is REQUIRED — nmos-cpp's
    /// `get_video_raw_parameters` (`Development/nmos/sdp_utils.cpp`)
    /// throws when absent and libnvnmos's
    /// `add_nmos_sender_to_node_server` catches that and
    /// silently returns `false`. When `from_caps`'s essence
    /// caps don't carry an explicit colorimetry, we have to
    /// pick one — ST 2110 SDR's BT709 is what nmos-cpp's
    /// `make_video_sdp_parameters` picks for a Flow without a
    /// colorimetry tag and what the SMPTE ST 2110-20 reference
    /// SDPs use, so a synthesised SDP with this default plays
    /// against both libnvnmos and any other ST 2110 peer.
    /// Callers wanting BT2020 / BT2100 colorimetry need to set
    /// it explicitly on the essence caps upstream of the
    /// `nmossink`.
    pub(crate) const ST2110_20_COLORIMETRY: &str = "BT709";
}

#[derive(Debug, Error)]
pub(crate) enum SdpError {
    #[error("SDP text could not be parsed: {0}")]
    Parse(String),
    #[error("SDP has no media lines")]
    NoMedia,
    #[error("SDP has {0} media lines; multi-leg SDPs (ST 2022-7) are not yet supported")]
    MultipleMedia(usize),
    #[error("SDP has {0} media lines; at most two same-essence legs are supported")]
    TooManyMediaBlocks(usize),
    #[error(
        "SDP mixes media types across `m=` blocks (e.g. video + audio); \
         each element handles a single essence"
    )]
    MultiMediaMixedEssence,
    #[error("SDP media is missing a payload-type / format slot")]
    MissingPt,
    #[error(
        "SDP media has no connection address (neither media-level nor session-level `c=` line)"
    )]
    MissingConnection,
    #[error("RTP caps could not be derived from SDP media: {0}")]
    CapsFromMedia(String),
    #[error("RTP caps lack the `media` field needed to dispatch on essence type")]
    MissingMediaField,
    #[error(
        "unsupported essence shape: {0}; today ST 2110-20 / RFC 4175 \
         `encoding-name=raw, sampling=YCbCr-4:2:2` (`depth=10` → \
         `UYVP`, `depth=8` → `UYVY`), ST 2110-30 / RFC 3551 \
         L24 / L16 audio, and ST 2110-40 / RFC 8331 \
         `encoding-name=SMPTE291` ANC are recognised"
    )]
    UnsupportedEssence(String),
    #[error("`set_media_from_caps` rejected the supplied RTP caps: {0}")]
    BuildMediaFromCaps(String),
    #[error("`gst_sdp_message_as_text` failed to serialise the constructed SDP: {0}")]
    Serialise(String),
    #[error(
        "RTP payload-type override {0} is outside the RFC 3551 §6 dynamic range \
         (96..=127); NMOS RTP transports only use dynamic payload types"
    )]
    InvalidPayloadType(u32),
    // Cross-check error variants — mirror
    // `FlowDefError::FormatMismatch` on the MXL path so both
    // transport stacks reject the same misconfigurations with
    // parallel attribution.
    #[error(
        "essence format mismatch: `caps` declares `{caps:?}` but SDP declares `{sdp:?}`"
    )]
    FormatMismatch { caps: FlowFormat, sdp: FlowFormat },
    #[error(
        "essence shape mismatch: `caps` `{caps}` does not intersect \
         SDP-derived essence caps `{sdp}`"
    )]
    EssenceShapeMismatch { caps: String, sdp: String },
    #[error(
        "transport-caps mismatch: `{transport_caps}` does not intersect \
         SDP-derived RTP caps `{sdp}` (override-class fields `payload`, \
         `a-ptime`, `a-maxptime` excluded from the comparison)"
    )]
    TransportCapsMismatch {
        transport_caps: String,
        sdp: String,
    },
}

/// Everything above the `m=` block of an SDP — i.e. the
/// session-level descriptors per RFC 4566 §5 — that this module's
/// media-side plumbing does not own. Caller-supplied because
/// these naturally come from the NMOS resource (label, sender id,
/// interface IP) which lives outside the SDP module.
///
/// Today only the two `o=` slots we vary (`<unicast-address>` and
/// `<sess-id>`; `<username>` / `<sess-version>` / `<nettype>` /
/// `<addrtype>` are hardcoded) and the `s=` line are modelled.
/// `i=` (session information) and session-level `a=` attributes
/// (e.g. `a=group:DUP` for ST 2022-7) will land here when the
/// integration path needs them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SdpSession<'a> {
    /// `o=` line `<unicast-address>` slot (RFC 4566 §5.2). For
    /// multicast Senders this is typically the local interface IP;
    /// `0.0.0.0` is a safe generic default when the interface is
    /// not known yet.
    pub origin_address: &'a str,
    /// `o=` line `<sess-id>` slot (RFC 4566 §5.2). RFC 4566 wants
    /// a non-zero numeric value. On the caps-only synthesis path
    /// this is derived from `(node_seed, side, name)` via
    /// [`stable_origin_session_id`]; passthrough SDPs keep the
    /// user's `o=` line unchanged.
    pub origin_session_id: &'a str,
    /// `s=` line session name (RFC 4566 §5.3). Typically the
    /// Sender's NMOS `label`; splices in via
    /// [`SdpOverrides::label`] so the NMOS `label` property
    /// reaches the configuring SDP. The field is `session_name`
    /// (the precise SDP term) rather than `label`, because the
    /// NMOS `label` is one possible value here and `s=` can in
    /// principle carry anything; the override layer above
    /// translates NMOS `label` → `session_name`.
    pub session_name: &'a str,
    /// `i=` line session information (RFC 4566 §5.4). `None`
    /// suppresses the line entirely. Typically the Sender's NMOS
    /// `description`; flows through the same property override
    /// path as `flow_def`'s `description` tag in
    /// [`crate::flow_def::FlowDefOverrides`].
    pub description: Option<&'a str>,
    /// Session-level `a=x-nvnmos-name:` value (the nvds-nmos
    /// vendor attribute that carries the NMOS Sender / Receiver
    /// `name`, distinct from the SDP-level `s=` session name).
    /// `None` suppresses the attribute entirely. Splices in via
    /// [`SdpOverrides::name`] so the NMOS resource `name`
    /// property reaches the configuring SDP without colliding
    /// with `label` → `s=`. Field name matches
    /// [`crate::flow_def::FlowDefOverrides::name`] (which lands
    /// in the equivalent `tags["urn:x-nvnmos:tag:name"]` slot in
    /// the MXL flow_def JSON) so the two transport paths read
    /// the same.
    ///
    /// Placement note: `nvnmos.h` documents this as a
    /// *session-level* attribute (see header lines 165, 341,
    /// 391) and libnvnmos's parser
    /// (`get_session_description_resource_name` in
    /// `nvnmos_impl.cpp:1687-1701`) reads it exclusively from
    /// `session_attributes`. Earlier revisions of this module
    /// emitted it at media level, which the daemon ignored
    /// silently because the gRPC `AddSender` / `AddReceiver`
    /// path carries the resource name as a separate proto
    /// field; SDP-only entry points would have failed with
    /// "Missing or empty x-nvnmos-name attribute in SDP".
    pub name: Option<&'a str>,
    /// Whether to emit a media-level `a=x-nvnmos-caps:<pt>` line
    /// for the resulting Receiver. `true` advertises *wide* caps
    /// (the daemon registers the IS-04 Receiver with the
    /// format-derived capability constraints omitted, so any
    /// compatible Sender of the same media type can connect);
    /// `false` advertises *narrow* caps (capability constraints
    /// derived from `media.raw_caps` / `media.rtp_caps`).
    ///
    /// Semantics match [`crate::flow_def::FlowDefMeta::caps`] on
    /// the MXL path. Wire form is canonical per
    /// `nvnmos_impl.cpp:1727-1731`:
    ///
    /// ```text
    /// a=x-nvnmos-caps:<pt> <constraints?>
    /// ```
    ///
    /// where `<pt>` is the RTP payload type (matches
    /// `a=rtpmap:<pt>` / `a=fmtp:<pt>`) and the constraints
    /// suffix is omitted to indicate "fully flexible". The
    /// daemon never synthesises this line itself — it only
    /// edits existing SDPs — so [`build_sdp`] is the single
    /// canonical writer in the system. libnvnmos's parser
    /// (`has_session_description_caps`) is presence-only and
    /// ignores the value, so the line is still parsed correctly
    /// even if `payload` is absent from `media.rtp_caps` (the
    /// fallback emits RFC 4566 flag form).
    ///
    /// On the MXL side, libnvnmos's `has_mxl_flow_def_caps`
    /// checks `!empty(array)` on the tag value. The IS-04
    /// `tags` schema (`resource_core.json`) doesn't enforce
    /// `minItems` on the value arrays, but in practice NMOS
    /// tag values are always non-empty (a key with no values
    /// carries no information), so the `!empty(array)` check
    /// is effectively a key-presence check under normal input.
    /// Our `[""]` emission satisfies it. The header doc's
    /// "presence-only" wording is shorthand for the same
    /// convention.
    ///
    /// The override layer ([`SdpOverrides::caps_mode`] /
    /// [`crate::types::CapsMode`]) resolves `Auto` against the
    /// input SDP's attribute presence and writes the resulting
    /// boolean here; callers who construct
    /// [`SdpSession`] directly (e.g. the future
    /// `synthesise_or_passthrough_udp`) set it from
    /// `receiver-caps-mode` already resolved against `Narrow`
    /// at no-transport-file time.
    pub advertise_caps: bool,
}

/// Parse an SDP transport file into a [`UdpMedia`].
///
/// Single-media SDPs only today; multi-media SDPs (ST 2022-7
/// redundancy) return [`SdpError::MultipleMedia`].
pub(crate) fn parse_sdp(text: &str) -> Result<UdpMedia, SdpError> {
    let msg = SDPMessage::parse_buffer(text.as_bytes())
        .map_err(|e| SdpError::Parse(e.to_string()))?;

    let num_medias = msg.medias_len() as usize;
    if num_medias == 0 {
        return Err(SdpError::NoMedia);
    }
    if num_medias > 1 {
        return Err(SdpError::MultipleMedia(num_medias));
    }
    let media = msg.media(0).ok_or(SdpError::NoMedia)?;

    let pt_str = media.format(0).ok_or(SdpError::MissingPt)?;
    let pt: i32 = pt_str.parse().map_err(|_| SdpError::MissingPt)?;

    let mut rtp_caps = media
        .caps_from_media(pt)
        .ok_or_else(|| SdpError::CapsFromMedia(format!("caps_from_media({pt}) returned None")))?;
    {
        let caps_mut = rtp_caps.make_mut();
        msg.attributes_to_caps(caps_mut)
            .map_err(|e| SdpError::CapsFromMedia(format!("attributes_to_caps failed: {e}")))?;
        if let Some(s) = caps_mut.structure_mut(0) {
            s.set_name("application/x-rtp");
            // `caps_from_media` only handles `rtpmap`/`fmtp`/
            // `framesize`; `a=ptime:` (and `a=maxptime:`) are
            // separate media-level attributes that GStreamer
            // expects on caps as `a-ptime` / `a-maxptime` so that
            // `set_media_from_caps` round-trips them as
            // standalone `a=…:` lines rather than folding them
            // into `a=fmtp:`. We hoist the values explicitly
            // here (rather than calling
            // `media.attributes_to_caps`) so that source-filter
            // and `x-nvnmos-*` — which we surface separately on
            // [`UdpLeg`] — don't end up double-emitted by
            // [`build_sdp`].
            if let Some(ptime) = media.attribute_val("ptime") {
                s.set("a-ptime", ptime);
            }
            if let Some(maxptime) = media.attribute_val("maxptime") {
                s.set("a-maxptime", maxptime);
            }
        }
    }

    let structure = rtp_caps
        .structure(0)
        .ok_or_else(|| SdpError::CapsFromMedia("rtp caps empty".to_owned()))?;
    let media_kind = structure
        .get::<&str>("media")
        .map_err(|_| SdpError::MissingMediaField)?;
    // `caps_from_media` upper-cases the rtpmap encoding-name via
    // `g_ascii_strup` (gstsdpmessage.c) so the match arms below
    // can use the canonical upper-case form. An absent field falls
    // through to "" which doesn't match any arm — the inner
    // `raw_caps_from_rtp_*` then surfaces the precise reason.
    let encoding_name = structure.get::<&str>("encoding-name").unwrap_or("");
    let format = match (media_kind, encoding_name) {
        // RFC 8331 / ST 2110-40 carries SMPTE 291 ANC under
        // `m=video`; only `encoding-name=SMPTE291` distinguishes it
        // from RFC 4175 raw video, so the dispatch has to be
        // (media, encoding-name)-keyed rather than media-keyed.
        ("video", enc) if enc.eq_ignore_ascii_case("SMPTE291") => FlowFormat::Data,
        ("video", _) => FlowFormat::Video,
        ("audio", _) => FlowFormat::Audio,
        (other, _) => {
            return Err(SdpError::UnsupportedEssence(format!(
                "media={other}, encoding-name={encoding_name}",
            )));
        }
    };

    let raw_caps = match format {
        FlowFormat::Video => raw_caps_from_rtp_video(&rtp_caps)?,
        FlowFormat::Audio => raw_caps_from_rtp_audio(&rtp_caps)?,
        FlowFormat::Data => raw_caps_from_rtp_data(&rtp_caps)?,
        FlowFormat::Unspecified => unreachable!(
            "format dispatch above never produces FlowFormat::Unspecified"
        ),
    };

    let connection = media
        .connection(0)
        .or_else(|| msg.connection())
        .ok_or(SdpError::MissingConnection)?;
    let destination_ip = connection
        .address()
        .ok_or(SdpError::MissingConnection)?
        .to_owned();
    let destination_port = media.port() as u16;

    let interface_ip = media.attribute_val("x-nvnmos-iface-ip").map(str::to_owned);
    let source_port = media
        .attribute_val("x-nvnmos-src-port")
        .and_then(|s| s.parse::<u16>().ok());
    let source_ip = media
        .attribute_val("source-filter")
        .and_then(extract_source_ip_from_filter);

    Ok(UdpMedia {
        format,
        primary: UdpLeg {
            destination_ip,
            destination_port,
            interface_ip,
            source_ip,
            source_port,
        },
        secondary: None,
        rtp_caps,
        raw_caps,
    })
}

/// Build an SDP transport-file text from a [`UdpMedia`] plus the
/// caller-supplied [`SdpSession`] session-level descriptors.
///
/// This is the inverse of [`parse_sdp`] and the UDP-flavoured
/// analogue of [`crate::flow_def::from_caps`]: it produces
/// the *configuring* transport file the element hands to
/// `nvnmosd` at `AddSender` / `AddReceiver` time, not anything
/// that goes on the wire. The daemon owns the IS-04 / IS-05
/// publication and may rewrite session-level fields before
/// advertising.
///
/// Two callers in mind:
///
/// * Deferred-mode Sender (or any element with `caps + properties`
///   but no `transport-file*`) — synthesise the configuring SDP
///   directly from the resolved [`UdpMedia`].
///
/// User-supplied transport files are mutated in place via
/// [`passthrough_with_overrides`] rather than rebuilt here.
///
/// Today the module emits single-media SDPs only. The
/// `media.secondary` leg is ignored if present; ST 2022-7
/// redundancy emits its second `m=` block when that work lands.
///
/// SDP output shape:
///
/// ```text
/// v=0
/// o=nvnmos <session.origin_session_id> 0 IN IP4 <session.origin_address>
/// s=<session.session_name>
/// i=<session.description>                                     ← if Some
/// t=0 0                                                       ← implicit from
///                                                             `gst_sdp_message_as_text`
///                                                             when no `t=` block was added
/// a=x-nvnmos-name:<session.name>                              ← if Some (session-level)
/// m=<media> <destination_port> RTP/AVP <pt>
/// c=IN IP4 <destination_ip>/64
/// a=rtpmap:<pt> ...      ← from `set_media_from_caps`
/// a=fmtp:<pt> ...        ← from `set_media_from_caps`
/// a=mediaclk:direct=0                                         ← synthesis only
/// a=source-filter: incl IN IP4 <destination_ip> <source_ip>   ← if source_ip
/// a=x-nvnmos-iface-ip:<interface_ip>                          ← if interface_ip
/// a=x-nvnmos-iface:<name> <port-id>                           ← if interface_ip resolves locally
/// a=x-nvnmos-src-port:<source_port>                           ← if source_port
/// a=x-nvnmos-caps:<pt>                                        ← if session.advertise_caps
/// ```
pub(crate) fn build_sdp(media: &UdpMedia, session: SdpSession<'_>) -> Result<String, SdpError> {
    let mut msg = SDPMessage::new();
    msg.set_version("0");
    msg.set_session_name(session.session_name);
    if let Some(description) = session.description {
        msg.set_information(description);
    }
    msg.set_origin(
        "nvnmos",
        session.origin_session_id,
        "0",
        "IN",
        "IP4",
        session.origin_address,
    );
    // Do not call [`SDPMessage::add_time`] here. `gst_sdp_message_as_text`
    // already emits `t=0 0` when the message carries no time blocks (see
    // gst-plugins-base `gstsdpmessage.c`). An explicit `add_time("0", "0",
    // &[])` from Rust has been observed to leave libgstsdp's internal time
    // entry in a corrupt shape and crash inside `as_text()` during
    // serialisation — see `build_sdp_emits_implicit_time_line_without_add_time`.

    // Session-level `a=x-nvnmos-name:` — placement per
    // `nvnmos.h` and the daemon's own SDP synthesis at
    // `nvnmos_impl.cpp:1536` (`push_back(session_attributes, ...)`).
    // libnvnmos's `get_session_description_resource_name` reads
    // exclusively from `session_attributes`, so a media-level
    // emission would be invisible to SDP-only entry points.
    if let Some(name) = session.name {
        msg.add_attribute("x-nvnmos-name", Some(name));
    }

    let leg = &media.primary;
    let mut m = SDPMedia::new();
    m.set_proto("RTP/AVP");
    m.set_port_info(u32::from(leg.destination_port), 1);
    // Per RFC 4566 §5.7 the ttl suffix must be present for
    // multicast `c=` lines and must be omitted for unicast.
    // `gst-sdp`'s `add_connection` already strips the suffix
    // for unicast destinations regardless of the `ttl`
    // argument (see `gst_sdp_strips_ttl_for_unicast_c_lines`),
    // so we can pass `MULTICAST_TTL` unconditionally.
    m.add_connection("IN", "IP4", &leg.destination_ip, defaults::MULTICAST_TTL, 0);
    m.set_media_from_caps(&media.rtp_caps)
        .map_err(|e| SdpError::BuildMediaFromCaps(e.to_string()))?;
    // `gstreamer-sdp`'s SDP → caps direction (`caps_from_media`)
    // upper-cases the rtpmap encoding-name and lower-cases every
    // fmtp parameter key as an unconditional normalisation step.
    // `nmos-cpp`'s `find_fmtp` / `get_format` parsers expect the
    // canonical case.
    canonicalise_st2110_wire_case(&mut m);

    // ST 2110-10 §6.2 direct media clock — emitted on the synthesis
    // path only (`build_sdp`). Passthrough SDPs keep the user's
    // `a=mediaclk:` (or its absence) verbatim.
    m.add_attribute("mediaclk", Some("direct=0"));

    // Media-level attribute order matches the canonical
    // configuring-SDP order emitted by
    // `nvds_nmos_bin::sdp_from_caps` for the slots it covers:
    //   a=source-filter (RFC 4607 SSM include)
    //   a=x-nvnmos-iface-ip (egress / join NIC IP)
    //   a=x-nvnmos-iface (IS-04 identity when IP resolves locally)
    //   a=x-nvnmos-src-port (sender RTP source port)
    //   a=x-nvnmos-caps  (Receiver wide-caps advertisement)
    if let Some(src) = leg.source_ip.as_deref() {
        let value = format!(" incl IN IP4 {dest} {src}", dest = leg.destination_ip);
        m.add_attribute("source-filter", Some(&value));
    }
    if let Some(iface) = leg.interface_ip.as_deref() {
        m.add_attribute("x-nvnmos-iface-ip", Some(iface));
        if let Some(value) = crate::iface::x_nvnmos_iface_value_for_ip(iface) {
            m.add_attribute("x-nvnmos-iface", Some(&value));
        }
    }
    if let Some(port) = leg.source_port {
        m.add_attribute("x-nvnmos-src-port", Some(&port.to_string()));
    }
    if session.advertise_caps {
        // Canonical form per `nvnmos_impl.cpp:1727-1731`'s
        // comment block:
        //   a=x-nvnmos-caps:<format> <format-specific constraints>
        // where `<format>` is the RTP payload type (matching
        // `a=rtpmap:<pt>` / `a=fmtp:<pt>`) and the constraints
        // section may be omitted to indicate "fully flexible /
        // wide". libnvnmos's parser
        // (`has_session_description_caps` at
        // `nvnmos_impl.cpp:1742-1745`) is presence-only — it
        // doesn't actually look at the value — but the daemon
        // itself only ever *edits* existing SDPs and never
        // synthesises this attribute from scratch, so we owe
        // downstream consumers (and the comment block above)
        // the documented form. Pt comes from `media.rtp_caps`'s
        // `payload` field, which `parse_sdp` always populates
        // (RFC 4566 requires the `m=` line's first format
        // token to be the RTP pt) and `synthesise_or_passthrough
        // _udp` will likewise populate when it lands.
        let pt = media
            .rtp_caps
            .structure(0)
            .and_then(|s| s.get::<i32>("payload").ok());
        let value = match pt {
            Some(pt) => pt.to_string(),
            // Fallback: no pt available (shouldn't happen for
            // SDPs that came through `parse_sdp`, but the type
            // system doesn't enforce it). Emit RFC 4566 flag
            // form via `Some("")` — `gstreamer-sdp` collapses
            // that to `a=x-nvnmos-caps` on the wire which still
            // satisfies libnvnmos's presence-only check.
            None => String::new(),
        };
        m.add_attribute("x-nvnmos-caps", Some(&value));
    }

    msg.add_media(m);

    msg.as_text().map_err(|e| SdpError::Serialise(e.to_string()))
}

/// Canonical wire-form spellings for ST 2110 `a=fmtp:` keys.
/// `gstreamer-sdp`'s `caps_from_media` lower-cases every fmtp key
/// and `nmos-cpp`'s `find_fmtp` is case-sensitive.
static ST_2110_UPPERCASE_FMTP_KEYS: LazyLock<HashSet<Ascii<&'static str>>> = LazyLock::new(|| {
    [
        // ST 2110-20 §7.2 / §7.3
        "PM", "SSN", "TCS", "RANGE", "PAR",
        // ST 2110-21 §8.1 / §8.2 (also -40 for TROFF, TSMODE, TSDELAY)
        "TP", "TROFF", "CMAX", "MAXUDP", "TSMODE", "TSDELAY",
        // ST 2110-40 §6
        "DID_SDID", "VPID_Code", "TM",
    ]
    .into_iter()
    .map(Ascii::new)
    .collect()
});

/// Canonical lower-case spellings for ST 2110 `a=rtpmap:`
/// encoding-names. `gstreamer-sdp` upper-cases the encoding-name
/// on the SDP → caps direction; `nmos-cpp`'s `get_format`
/// expects the canonical case.
static ST_2110_LOWERCASE_RTPMAP_NAMES: LazyLock<HashSet<Ascii<&'static str>>> = LazyLock::new(|| {
    [
        "raw",      // RFC 4175 / ST 2110-20
        "jxsv",     // RFC 9134 / ST 2110-22
        "smpte291", // RFC 8331 / ST 2110-40
    ]
    .into_iter()
    .map(Ascii::new)
    .collect()
});


/// Rewrite the `rtpmap` and `fmtp` attributes that
/// `set_media_from_caps` just populated on `m` into the canonical
/// wire-form expected by `nmos-cpp`'s case-sensitive parser.
fn canonicalise_st2110_wire_case(m: &mut SDPMedia) {
    for idx in 0..m.attributes_len() {
        canonicalise_media_attribute_at(m, idx);
    }
}

/// Canonicalise a single media attribute we just wrote (rtpmap/fmtp
/// wire case only). Used by the synthesis builder and the passthrough
/// override path so user-supplied attributes are never rewritten.
pub(crate) fn canonicalise_media_attribute_at(m: &mut SDPMediaRef, idx: u32) {
    let rewrite = m.attribute(idx).and_then(|attr| {
        let value = attr.value()?;
        match attr.key() {
            "rtpmap" => canonicalise_rtpmap_value(value).map(|v| ("rtpmap", v)),
            "fmtp" => canonicalise_fmtp_value(value).map(|v| ("fmtp", v)),
            _ => None,
        }
    });
    if let Some((key, value)) = rewrite {
        let _ = m.replace_attribute(idx, SDPAttribute::new(key, Some(&value)));
    }
}

/// Rewrite the rtpmap attribute value
/// (`<pt> <encoding-name>/<rate>[/<encoding-params>]`, RFC 4566
/// §6) when the encoding-name is not in canonical case.
/// Returns `None` when the value is already canonical (no rewrite
/// needed).
fn canonicalise_rtpmap_value(value: &str) -> Option<String> {
    let (pt, rest) = value.split_once(' ')?;
    let (encoding, after_slash) = rest.split_once('/')?;
    let canonical = **ST_2110_LOWERCASE_RTPMAP_NAMES.get(&Ascii::new(encoding))?;
    (canonical != encoding).then(|| format!("{pt} {canonical}/{after_slash}"))
}

/// Rewrite the fmtp attribute value
/// (`<pt> <key>=<value>(;<key>=<value>)*`, RFC 4566 §6) when any
/// key is not in canonical case. Values are preserved verbatim.
/// Returns `None` when no key needs rewriting.
fn canonicalise_fmtp_value(value: &str) -> Option<String> {
    let (pt, rest) = value.split_once(' ')?;
    let mut changed = false;
    let fixed = rest
        .split(';')
        .map(|kv| {
            let Some((key, val)) = kv.split_once('=') else { return kv.to_owned() };
            match ST_2110_UPPERCASE_FMTP_KEYS.get(&Ascii::new(key)).map(|m| **m) {
                Some(canonical) if canonical != key => {
                    changed = true;
                    format!("{canonical}={val}")
                }
                _ => kv.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join(";");
    changed.then(|| format!("{pt} {fixed}"))
}

/// Scalar overrides applied to an SDP transport file during
/// [`passthrough_with_overrides`]. Mirrors
/// [`crate::flow_def::FlowDefOverrides`]'s field shape on the
/// MXL path so the two splice call sites read identically:
///
/// * Top-level descriptors (`label`, `description`, `name`) cover
///   the NMOS Sender / Receiver resource metadata that also lands
///   in the IS-04 publication. `label` ↔ `s=`, `description` ↔
///   `i=`, `name` ↔ session-level `a=x-nvnmos-name` (the
///   nvds-nmos vendor attribute that carries the NMOS resource
///   `name`, distinct from `label`).
/// * IS-05 RTP transport_params slots (`interface_ip`,
///   `destination_ip`, `destination_port`, `source_ip`,
///   `source_port`) cover the GObject properties that operators
///   can set directly on `nmossink` / `nmossrc` without rewriting
///   the configuring SDP by hand. Names follow the IS-05 RTP
///   transport_params schema verbatim (sender semantic):
///   `source_ip` is the *local egress NIC*, `destination_ip` is
///   the *remote / multicast group*, etc. On a Receiver,
///   `source_ip` instead carries the SSM include filter
///   (remote sender's address); the conversion from
///   IS-05-sender-vocabulary to the [`UdpLeg`] field names
///   happens at the property layer in `nmossink` / `nmossrc`,
///   not here — by the time [`passthrough_with_overrides`] runs every
///   field already means what it does on [`UdpLeg`].
///
/// * Transport-caps overrides (`payload_type`,
///   `audio_clock_rate`, `a_ptime`, `a_maxptime`) cover the
///   override-class fields of the `transport-caps` property
///   (`application/x-rtp,...` GStreamer caps). Per the
///   override-vs-cross-check rule on
///   [`crate::session::CommonSettings::transport_caps`]:
///   * `payload_type` (RFC 3551 dynamic range 96..=127)
///     rewrites the `m=` line's first format token,
///     `a=rtpmap:<pt>`, `a=fmtp:<pt>`, and `a=x-nvnmos-caps:<pt>`
///     atomically — they're all driven from
///     `media.primary.rtp_caps`'s `payload` field via
///     `set_media_from_caps`. Applies to all essences.
///   * `audio_clock_rate` rewrites `a=rtpmap:<pt> L24/<rate>/<n>`.
///     Audio-only — silently ignored for video raw / ANC
///     because those carry a fixed 90 kHz clock per RFC 4175 /
///     RFC 8331 and the override category for fixed-clock
///     essences is cross-check, not override.
///   * `a_ptime` / `a_maxptime` rewrite `a=ptime:` /
///     `a=maxptime:` (millisecond decimal string per the
///     GStreamer `a-ptime` / `a-maxptime` caps convention).
///     Audio-only — for the same reason: video raw / ANC
///     don't carry packet-time metadata.
///
/// `None` on a slot leaves the input SDP's value untouched.
/// `Some(...)` replaces it. The split between "absent" and
/// "explicit override to empty/zero" follows the MXL convention:
/// callers map empty GObject string properties to `None` before
/// constructing the struct.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SdpOverrides<'a> {
    pub label: Option<&'a str>,
    pub description: Option<&'a str>,
    pub name: Option<&'a str>,
    pub interface_ip: Option<&'a str>,
    pub destination_ip: Option<&'a str>,
    pub destination_port: Option<u16>,
    pub source_ip: Option<&'a str>,
    pub source_port: Option<u16>,
    /// RTP payload type override (RFC 3551 §6 dynamic range
    /// 96..=127). All essences honour this. Out-of-range
    /// values are rejected by [`passthrough_with_overrides`] with
    /// [`SdpError::InvalidPayloadType`] rather than silently
    /// dropped — `transport-caps` is user-facing and a stale
    /// pt is a misconfiguration worth surfacing.
    pub payload_type: Option<u8>,
    /// Audio clock-rate override (Hz). Honoured only for
    /// audio essence (L16/L24); ignored for video raw / ANC
    /// (those carry a fixed 90 kHz clock per RFC 4175 /
    /// RFC 8331 and fall under the cross-check rule, not
    /// override).
    pub audio_clock_rate: Option<u32>,
    /// `a=ptime:` override — millisecond decimal string per
    /// the GStreamer `a-ptime` caps convention (e.g. `"0.125"`
    /// for 125 µs, `"1"` for 1 ms). Audio-only.
    pub a_ptime: Option<&'a str>,
    /// `a=maxptime:` override — same string convention as
    /// `a_ptime`. Audio-only. Independent slot per RFC 4566
    /// — for ST 2110-30 typically equal to `a_ptime`, but
    /// callers may set them distinctly.
    pub a_maxptime: Option<&'a str>,
    /// Wide / narrow / auto resolution for the Receiver
    /// advertisement (`a=x-nvnmos-caps:` media-level attribute).
    /// Mirrors [`crate::flow_def::FlowDefOverrides::caps_mode`]
    /// on the MXL path so callers can hand the same
    /// `receiver-caps-mode` GObject property value to either
    /// splice without translation:
    ///
    /// * [`CapsMode::Auto`] (default) — leave the input SDP's
    ///   `a=x-nvnmos-caps:` attribute presence untouched. Narrow
    ///   when the attribute is absent, wide when present.
    /// * [`CapsMode::Narrow`] — strip `a=x-nvnmos-caps:` if the
    ///   input SDP carries it; otherwise no-op.
    /// * [`CapsMode::Wide`] — add `a=x-nvnmos-caps:` (empty
    ///   value) if the input SDP doesn't carry it; otherwise
    ///   no-op (the existing value is *not* preserved — see the
    ///   [`SdpSession::advertise_caps`] doc for the canonical
    ///   empty-value form).
    ///
    /// Field uses [`CapsMode`] directly (not `Option<CapsMode>`)
    /// because `Auto` is already a "no override" sentinel; this
    /// matches the MXL side where `flow_def::splice_overrides`
    /// also takes the enum by value and reads `Auto` as "leave
    /// it alone".
    pub caps_mode: CapsMode,
}

pub(crate) use crate::sdp_passthrough::passthrough_with_overrides;

/// Cross-check the parsed [`UdpMedia`] (post-splice) against
/// the user-supplied `caps` (essence shape) and
/// `transport-caps` (RTP-layer hints). Mirrors
/// [`crate::flow_def::resolve_mxl_flow_meta`]'s cross-check
/// pass on the MXL path.
///
/// **Override-vs-cross-check rule** (per
/// [`crate::session::CommonSettings::transport_caps`]):
///
/// | Field                      | Audio essence | Video / data |
/// |----------------------------|---------------|--------------|
/// | RTP `payload` (96..=127)   | Override      | Override     |
/// | RTP `clock-rate`           | **Override**  | Cross-check  |
/// | `a-ptime` / `a-maxptime`   | Override      | (Not present)|
/// | RTP `encoding-name`        | Cross-check   | Cross-check  |
/// | RTP `media` (`audio`/…)    | Cross-check   | Cross-check  |
/// | `format` / `width` / `height` / `sampling` / `depth` / `channels` / `framerate` (essence shape) | Cross-check | Cross-check |
///
/// Implementation: because [`passthrough_with_overrides`] has *already*
/// applied the override-class fields into
/// `media.primary.rtp_caps` (audio clock-rate, payload, ptime,
/// maxptime), the cross-check just intersects what's left with
/// the user's `transport-caps`. Fields stripped from both
/// sides before the intersect: `payload`, `a-ptime`,
/// `a-maxptime` (always-override). Audio-only `clock-rate`
/// override is implicit — passthrough has copied the value over,
/// so the intersect trivially passes; for video / data the
/// `clock-rate` stays at the SDP's value and any mismatch
/// against `transport-caps` surfaces as
/// [`SdpError::TransportCapsMismatch`].
///
/// Essence-caps cross-check intersects `caps` against
/// `media.raw_caps`; an empty intersection is
/// [`SdpError::EssenceShapeMismatch`]. Format-family mismatches
/// (e.g. audio caps + video SDP) surface as
/// [`SdpError::FormatMismatch`] before the shape intersect runs
/// so the error message is specific.
pub(crate) fn cross_check_essence(
    media: &UdpMedia,
    essence_caps: Option<&gst::Caps>,
    transport_caps: Option<&gst::Caps>,
) -> Result<(), SdpError> {
    if let Some(caps) = essence_caps {
        cross_check_essence_caps(media, caps)?;
    }
    if let Some(caps) = transport_caps {
        cross_check_transport_caps(media, caps)?;
    }
    Ok(())
}

/// Map the structure name of an essence caps (`video/x-raw`,
/// `audio/x-raw`, `meta/x-st-2038`, …) to the corresponding
/// [`FlowFormat`]. Used by [`cross_check_essence_caps`] to fire
/// [`SdpError::FormatMismatch`] before the costlier shape
/// intersect runs.
fn essence_caps_format(caps: &gst::Caps) -> Option<FlowFormat> {
    let s = caps.structure(0)?;
    match s.name().as_str() {
        "video/x-raw" => Some(FlowFormat::Video),
        "audio/x-raw" => Some(FlowFormat::Audio),
        "meta/x-st-2038" => Some(FlowFormat::Data),
        _ => None,
    }
}

fn cross_check_essence_caps(media: &UdpMedia, caps: &gst::Caps) -> Result<(), SdpError> {
    if let Some(caps_format) = essence_caps_format(caps) {
        if caps_format != media.format {
            return Err(SdpError::FormatMismatch {
                caps: caps_format,
                sdp: media.format,
            });
        }
    }
    let intersect = caps.intersect(&media.raw_caps);
    if intersect.is_empty() {
        return Err(SdpError::EssenceShapeMismatch {
            caps: caps.to_string(),
            sdp: media.raw_caps.to_string(),
        });
    }
    Ok(())
}

fn cross_check_transport_caps(media: &UdpMedia, transport_caps: &gst::Caps) -> Result<(), SdpError> {
    // Strip always-override fields from both sides so the
    // intersect only sees cross-check fields. The audio-only
    // `clock-rate` override is implicit: passthrough has already
    // copied the value into `media.rtp_caps` for
    // audio essences, so the intersect on `clock-rate`
    // trivially passes there; for video / data the field
    // stays at the SDP's value and any mismatch surfaces here.
    let mut tc = transport_caps.clone();
    let mut rtp = media.rtp_caps.clone();
    for caps in [&mut tc, &mut rtp] {
        let caps_mut = caps.make_mut();
        if let Some(s) = caps_mut.structure_mut(0) {
            s.remove_field("payload");
            s.remove_field("a-ptime");
            s.remove_field("a-maxptime");
        }
    }
    let intersect = tc.intersect(&rtp);
    if intersect.is_empty() {
        return Err(SdpError::TransportCapsMismatch {
            transport_caps: tc.to_string(),
            sdp: rtp.to_string(),
        });
    }
    Ok(())
}

/// Inputs to [`from_caps`]: everything needed to synthesise a
/// configuring SDP from a caps-only Sender / Receiver
/// configuration. Mirrors [`crate::flow_def::FlowDefBuildInput`]
/// on the MXL path: the orchestrator snapshots GObject properties
/// and `transport_caps` once per `validate_and_open` or
/// `make_activation_plan` call and hands them in.
///
/// The struct does not carry [`UdpLeg`] directly because the
/// per-side IS-05 ↔ SDP mapping (where `source_ip` means the
/// local egress NIC on a Sender but the SSM include filter on a
/// Receiver) is exactly the dispatch [`from_caps`] performs;
/// callers stay in IS-05 vocabulary.
///
/// `destination_ip` here is the wire destination on the `m=` /
/// `c=` lines:
/// * Sender: the egress destination (unicast peer or multicast
///   group) — `nmossink`'s `destination-ip` property.
/// * Receiver: the multicast group to join (or unicast bind
///   address) — `nmossrc`'s `multicast-ip` property. The two
///   GObject property names differ but the SDP wire slot is the
///   same.
///
/// `interface_ip` is the local NIC:
/// * Sender: unused — Senders re-use `source_ip` as the egress
///   NIC, so `interface_ip` should be empty.
/// * Receiver: the IGMP join NIC — `nmossrc`'s `interface-ip`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SdpBuildInput<'a> {
    /// Essence caps (`video/x-raw,…` / `audio/x-raw,…` /
    /// `meta/x-st-2038,…`). Drives essence-shape fmtp fields and
    /// the `format` field of the returned [`UdpMedia`].
    pub essence_caps: &'a gst::Caps,
    /// Optional `application/x-rtp,…` caps supplying the
    /// override-class fields (`payload`, audio `clock-rate`,
    /// `a-ptime`, `a-maxptime`). Cross-check-class fields the
    /// caps may also carry (`encoding-name`, video / ANC
    /// `clock-rate`) are ignored on the synthesis side — they're
    /// derived from `essence_caps` instead.
    pub transport_caps: Option<&'a gst::Caps>,
    /// Sender or Receiver — drives the per-side dispatch on
    /// `source_ip` / `interface_ip` into the produced
    /// [`UdpLeg`].
    pub side: Side,
    /// NMOS resource label → SDP `s=` line. Empty falls back to
    /// `"nvnmos"`; RFC 4566 §5.3 requires `s=` to be non-empty.
    pub label: &'a str,
    /// NMOS resource description → SDP `i=` line. Empty omits
    /// the `i=` line.
    pub description: &'a str,
    /// NMOS resource name → session-level `a=x-nvnmos-name:`.
    /// Empty omits the attribute (libnvnmos then falls back to
    /// the `s=` value at `get_session_description_resource_name`).
    pub name: &'a str,
    /// IS-05 `source_ip` — per-side meaning:
    /// * Sender: local egress NIC IP. Drives both
    ///   `a=source-filter:` include and the
    ///   `a=x-nvnmos-iface-ip:` slot (duplication mirrors
    ///   `property_overrides_udp`).
    /// * Receiver: SSM include source (remote sender's IP).
    pub source_ip: &'a str,
    /// IS-05 `source_port` — Sender's RTP source port. 0 = unset
    /// (Receiver should also pass 0).
    pub source_port: u16,
    /// IS-05 `destination_ip` — wire destination (see struct
    /// docstring for per-side meaning).
    pub destination_ip: &'a str,
    /// IS-05 `destination_port` — wire destination port
    /// (Sender egress port / Receiver bind port). 0 falls back to
    /// [`defaults::RTP_PORT`].
    pub destination_port: u16,
    /// IS-05 `interface_ip` — Receiver-only local NIC. Empty on
    /// Senders (per the struct docstring).
    pub interface_ip: &'a str,
    /// Whether the SDP advertises wide receiver-caps via
    /// `a=x-nvnmos-caps:<pt>` at the media level. Caller
    /// resolves `CapsMode::Auto` to `false` (the default
    /// configuring SDP is narrow until something says
    /// otherwise).
    pub advertise_caps: bool,
    /// NMOS Node seed (`node-seed` on the element). With [`name`]
    /// and [`side`](Self::side) drives a stable RFC 4566 `o=`
    /// `<sess-id>` on the synthesis path.
    pub node_seed: &'a str,
}

/// Synthesise a full configuring SDP from caps-only
/// configuration. UDP-transport counterpart of
/// [`crate::flow_def::from_caps`] on the MXL path.
///
/// Sequence:
///
/// 1. Dispatch on `essence_caps`'s structure name to a
///    [`FlowFormat`] and the matching `rtp_caps_from_raw_*`
///    primitive.
/// 2. Resolve override-class RTP parameters from
///    `transport_caps` falling back to [`defaults`]: payload
///    type (per essence default + RFC 3551 dynamic-range
///    validation), audio `clock-rate`, `a-ptime`, `a-maxptime`.
/// 3. Build the [`UdpMedia`] (with [`UdpLeg`] populated from the
///    IS-05 endpoint fields via [`udp_leg_from_input`]) and the
///    [`SdpSession`] (label → `s=`, description → `i=`, name →
///    session-level `a=x-nvnmos-name`, `interface_ip` /
///    `source_ip` → `o=` `<unicast-address>`).
/// 4. Serialise via [`build_sdp`].
///
/// Returns [`SdpError::UnsupportedEssence`] when `essence_caps`
/// is not one of the recognised essence shapes, or
/// [`SdpError::InvalidPayloadType`] when `transport_caps` carries
/// a payload-type outside RFC 3551's dynamic range.
pub(crate) fn from_caps(input: &SdpBuildInput<'_>) -> Result<String, SdpError> {
    let format = essence_caps_format(input.essence_caps).ok_or_else(|| {
        SdpError::UnsupportedEssence(format!("essence caps `{}`", input.essence_caps))
    })?;

    let payload_type = resolve_payload_type(format, input.transport_caps)?;

    let rtp_caps = match format {
        FlowFormat::Video => rtp_caps_from_raw_video(input.essence_caps, payload_type)?,
        FlowFormat::Audio => {
            let (ptime_ns, maxptime_ns) = resolve_audio_ptime(input.transport_caps);
            let resolved = resolved_audio_caps(input.essence_caps, input.transport_caps)?;
            rtp_caps_from_raw_audio(&resolved, payload_type, ptime_ns, maxptime_ns)?
        }
        FlowFormat::Data => rtp_caps_from_raw_data(input.essence_caps, payload_type)?,
        FlowFormat::Unspecified => {
            unreachable!("essence_caps_format never returns Unspecified")
        }
    };

    let media = UdpMedia {
        format,
        primary: udp_leg_from_input(input),
        secondary: None,
        rtp_caps,
        raw_caps: input.essence_caps.clone(),
    };

    let origin_address = if input.interface_ip.is_empty() {
        if input.source_ip.is_empty() {
            defaults::ORIGIN_ADDRESS
        } else {
            input.source_ip
        }
    } else {
        input.interface_ip
    };

    let session_name = if input.label.is_empty() {
        "nvnmos"
    } else {
        input.label
    };
    let description = if input.description.is_empty() {
        None
    } else {
        Some(input.description)
    };
    let name = if input.name.is_empty() {
        None
    } else {
        Some(input.name)
    };

    let origin_session_id = stable_origin_session_id(input.node_seed, input.side, input.name);
    let session = SdpSession {
        origin_address,
        origin_session_id: &origin_session_id,
        session_name,
        description,
        name,
        advertise_caps: input.advertise_caps,
    };

    build_sdp(&media, session)
}

/// Deterministic RFC 4566 `o=` `<sess-id>` for caps-only synthesis.
///
/// Hashes `(node_seed, side, resource name)` so the same NMOS resource
/// on a Node always gets the same configuring SDP session id, while
/// distinct resources (and sender vs receiver with the same name)
/// diverge. Never returns zero (RFC 4566 recommends a non-zero sess-id).
pub(crate) fn stable_origin_session_id(node_seed: &str, side: Side, name: &str) -> String {
    let mut hasher = DefaultHasher::new();
    node_seed.hash(&mut hasher);
    side.hash(&mut hasher);
    name.hash(&mut hasher);
    (hasher.finish() as u32).max(1).to_string()
}

/// Resolve the RTP payload type for a synthesis run: read
/// `payload` from `transport_caps` if present, otherwise fall
/// back to the per-essence default in [`defaults`]. The
/// override is rejected with [`SdpError::InvalidPayloadType`]
/// when outside RFC 3551 §6's dynamic range (`96..=127`),
/// matching [`passthrough_with_overrides`]'s validation on the
/// passthrough path.
fn resolve_payload_type(
    format: FlowFormat,
    transport_caps: Option<&gst::Caps>,
) -> Result<u8, SdpError> {
    if let Some(caps) = transport_caps {
        if let Some(s) = caps.structure(0) {
            if let Ok(pt) = s.get::<i32>("payload") {
                let pt_u32 = u32::try_from(pt).map_err(|_| SdpError::InvalidPayloadType(0))?;
                if !(96..=127).contains(&pt_u32) {
                    return Err(SdpError::InvalidPayloadType(pt_u32));
                }
                return Ok(pt_u32 as u8);
            }
        }
    }
    let default_pt = match format {
        FlowFormat::Video => defaults::VIDEO_PAYLOAD_TYPE,
        FlowFormat::Audio => defaults::AUDIO_PAYLOAD_TYPE,
        FlowFormat::Data => defaults::ANC_PAYLOAD_TYPE,
        FlowFormat::Unspecified => unreachable!(),
    };
    Ok(default_pt as u8)
}

/// Resolve audio `(ptime_ns, maxptime_ns)` from
/// `transport_caps`'s `a-ptime` / `a-maxptime` string slots,
/// falling back to [`defaults::AUDIO_PTIME_NS`] (1 ms) for
/// `ptime` and `None` for `maxptime`. The string values are
/// SDP wire form — decimal milliseconds per RFC 4566 §6
/// (`"1"`, `"0.125"`, `"4"`, …); unparseable values fall
/// back to the default rather than erroring, mirroring the
/// splice path's tolerance.
fn resolve_audio_ptime(transport_caps: Option<&gst::Caps>) -> (u64, Option<u64>) {
    let mut ptime_ns = defaults::AUDIO_PTIME_NS;
    let mut maxptime_ns = None;
    if let Some(caps) = transport_caps {
        if let Some(s) = caps.structure(0) {
            if let Ok(v) = s.get::<&str>("a-ptime") {
                if let Some(ns) = parse_ptime_ms_as_ns(v) {
                    ptime_ns = ns;
                }
            }
            if let Ok(v) = s.get::<&str>("a-maxptime") {
                maxptime_ns = parse_ptime_ms_as_ns(v);
            }
        }
    }
    (ptime_ns, maxptime_ns)
}

/// Parse an SDP `a=ptime:` / `a=maxptime:` wire value (decimal
/// milliseconds) into nanoseconds. Inverse of
/// [`format_ptime_ns_as_ms`]; returns `None` for empty /
/// non-numeric strings.
fn parse_ptime_ms_as_ns(value: &str) -> Option<u64> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    let ms: f64 = v.parse().ok()?;
    if !ms.is_finite() || ms < 0.0 {
        return None;
    }
    Some((ms * 1_000_000.0).round() as u64)
}

/// Clone `essence_caps` with the audio sample `rate` overridden
/// by `transport_caps`'s `clock-rate` slot (audio's only
/// override-class clock-rate field per RFC 3551). When
/// `transport_caps` carries no `clock-rate`, returns
/// `essence_caps` unchanged.
///
/// Per the override-vs-cross-check rule on
/// [`crate::session::CommonSettings::transport_caps`], an audio
/// `transport_caps.clock-rate` is an override (not a
/// cross-check) — the synthesised SDP advertises the overridden
/// rate via `a=rtpmap:<pt> L24/<rate>/<n>` even when it differs
/// from `essence_caps`'s `rate`. The rest of the synthesis chain
/// (especially [`rtp_caps_from_raw_audio`]) sees the overridden
/// rate on `essence_caps` and emits matching `clock-rate=` /
/// `rate=` on the produced caps.
fn resolved_audio_caps(
    essence_caps: &gst::Caps,
    transport_caps: Option<&gst::Caps>,
) -> Result<gst::Caps, SdpError> {
    let Some(tc) = transport_caps else {
        return Ok(essence_caps.clone());
    };
    let Some(ts) = tc.structure(0) else {
        return Ok(essence_caps.clone());
    };
    let Ok(rate) = ts.get::<i32>("clock-rate") else {
        return Ok(essence_caps.clone());
    };
    let mut cloned = essence_caps.clone();
    let cm = cloned.make_mut();
    if let Some(s) = cm.structure_mut(0) {
        s.set("rate", rate);
    }
    Ok(cloned)
}

/// Build a [`UdpLeg`] from [`SdpBuildInput`]'s IS-05 endpoint
/// fields, dispatching on `side` for the `source_ip` /
/// `interface_ip` per-side meaning. Mirrors
/// [`crate::session::property_overrides_udp`]'s per-side
/// dispatch so the splice (parse + override) and synthesise
/// paths populate `UdpLeg` identically.
fn udp_leg_from_input(input: &SdpBuildInput<'_>) -> UdpLeg {
    let destination_port = if input.destination_port == 0 {
        defaults::RTP_PORT
    } else {
        input.destination_port
    };
    let source_ip = if input.source_ip.is_empty() {
        None
    } else {
        Some(input.source_ip.to_owned())
    };
    let interface_ip = match input.side {
        // Sender: the egress NIC IP comes in via `source_ip`
        // and duplicates into `interface_ip` (mirrors
        // `property_overrides_udp`'s Sender dispatch and
        // produces `a=x-nvnmos-iface-ip:<sender's NIC>` in the
        // emitted SDP).
        Side::Sender => source_ip.clone(),
        // Receiver: `interface_ip` is the join NIC, a distinct
        // GObject property from `source_ip`.
        Side::Receiver => {
            if input.interface_ip.is_empty() {
                None
            } else {
                Some(input.interface_ip.to_owned())
            }
        }
    };
    let source_port = match input.side {
        Side::Sender if input.source_port != 0 => Some(input.source_port),
        _ => None,
    };
    UdpLeg {
        destination_ip: input.destination_ip.to_owned(),
        destination_port,
        interface_ip,
        source_ip,
        source_port,
    }
}

/// Extract the single included source-IP from an RFC 4607
/// `a=source-filter:` value.
///
/// Value format per RFC 4607:
///
/// ```text
/// <filter-mode> <nettype> <addrtype> <dest-address> <src-list>
/// ```
///
/// where `filter-mode` is `incl` or `excl` and `src-list` is one or
/// more whitespace-separated source addresses. NMOS's RTP
/// transport-params `source_ip` is a single string by definition
/// (single-source include-mode); exclude-mode filters or filters
/// with multiple sources are out of scope for the receiver model
/// and yield `None`.
fn extract_source_ip_from_filter(value: &str) -> Option<String> {
    let mut tokens = value.split_whitespace();
    let mode = tokens.next()?;
    if mode != "incl" {
        return None;
    }
    let _nettype = tokens.next()?;
    let _addrtype = tokens.next()?;
    let _dest = tokens.next()?;
    let src = tokens.next()?;
    if tokens.next().is_some() {
        return None;
    }
    Some(src.to_owned())
}

/// Map an ST 2110-20 fmtp `colorimetry` (and optional `TCS` for
/// `BT2100`) to the GStreamer `colorimetry` caps preset name.
///
/// Both depay variants need this on the raw `video/x-raw` caps,
/// but for different reasons:
///
/// * V2 `rtpvrawdepay2` (gst-plugins-rs `>=0.15`) reads SDP
///   `colorimetry` from the RTP caps and emits the equivalent
///   GStreamer preset on its `video/x-raw` output. Pinning the
///   same preset on the tail `capsfilter` keeps the intersection
///   non-empty; pinning a strictly *narrower* preset than what
///   V2 emits is also fine, because the intersection just adds
///   the constraint.
/// * V1 `rtpvrawdepay` (gst-plugins-good) **ignores** SDP
///   colorimetry — `gst_rtp_vraw_depay_setcaps` only reads
///   width/height/depth/sampling/interlace and lets
///   `gst_video_info_set_format` pick the format's default,
///   which for UYVY is `bt601`. The tail `capssetter` then
///   merges the preset emitted here over V1's wrong default.
///
/// Coverage mirrors
/// `net/rtp/src/raw_video/depay/imp.rs::set_sink_caps` in
/// gst-plugins-rs `>=0.15`:
///
/// | SDP `colorimetry`     | extra hints  | caps preset      |
/// |-----------------------|--------------|------------------|
/// | `BT601-5`, `BT601`    | -            | `bt601`          |
/// | `BT709-2`, `BT709`    | -            | `bt709`          |
/// | `SMPTE240M`           | -            | `smpte240m`      |
/// | `BT2020`              | `depth<10`   | `bt2020`         |
/// | `BT2020`              | `depth>=10`  | `bt2020-10`      |
/// | `BT2100`              | `TCS=PQ`     | `bt2100-pq`      |
/// | `BT2100`              | `TCS=HLG`    | `bt2100-hlg`     |
/// | `BT2100`              | TCS missing/unknown | `bt2100-pq` (assume PQ) |
/// | `UNSPECIFIED`, `ST2065-1`, `ST2065-3`, `XYZ` | - | `None` |
///
/// One deliberate divergence from V2: V2's fallback for
/// `BT2100` with missing/unknown `TCS` attempts to construct
/// `VideoColorimetry::from_str("bt2100-pg")` — a typo for
/// `…-pq` — which fails, so V2 silently emits no colorimetry.
/// The surrounding warning ("assuming PQ") shows the intent was
/// "assume PQ"; we honour that intent here and return
/// `bt2100-pq`. This stays V2-output-compatible because
/// intersecting our `bt2100-pq` against V2's missing-colorimetry
/// caps still yields `bt2100-pq`, while the V1 capssetter path
/// now actually honours the assumed transfer characteristic
/// instead of leaving `bt601` (the UYVY format default) in
/// place. Tracked at
/// <https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/work_items/805>.
///
/// Note that the returned preset always implies the standard
/// narrow range (`16_235`) of its colorimetry tuple. ST 2110-21
/// `RANGE=FULL` / `FULLPROTECT` is **not** propagated; see
/// `raw_caps_from_rtp_video`'s docstring for the rationale (V2
/// doesn't read `RANGE` either, so plumbing it asymmetrically
/// via the V1 capssetter would diverge V1/V2 output).
fn caps_colorimetry_from_sdp(sdp: &str, depth: u32, tcs: Option<&str>) -> Option<&'static str> {
    match sdp.to_ascii_uppercase().as_str() {
        "BT601-5" | "BT601" => Some("bt601"),
        "BT709-2" | "BT709" => Some("bt709"),
        "SMPTE240M" => Some("smpte240m"),
        "BT2020" if depth >= 10 => Some("bt2020-10"),
        "BT2020" => Some("bt2020"),
        "BT2100" if tcs.is_some_and(|t| t.eq_ignore_ascii_case("HLG")) => Some("bt2100-hlg"),
        "BT2100" => Some("bt2100-pq"),
        _ => None,
    }
}

/// Inverse of [`caps_colorimetry_from_sdp`]: given a
/// `video/x-raw,colorimetry=<preset>` GStreamer colorimetry
/// preset, return the matching ST 2110-20 fmtp `colorimetry=`
/// value and the optional `TCS=` companion.
///
/// The mapping is not strictly bijective: `bt2020` (8-bit) and
/// `bt2020-10` (10-bit) both map back to SDP `BT2020`, with the
/// bit-depth carried separately by the `depth=` fmtp parameter.
/// `bt2100-pq` and `bt2100-hlg` differ only in the companion
/// `TCS=` value, which we surface as the second tuple element.
///
/// Custom non-preset forms (e.g. `1:3:5:1`) and unrecognised
/// presets return `None`, leaving the SDP without explicit
/// `colorimetry=` / `TCS=` parameters; standards-compliant
/// receivers then fall back to ST 2110-20's "unspecified" entry.
fn sdp_colorimetry_from_caps(caps_colorimetry: &str) -> Option<(&'static str, Option<&'static str>)> {
    match caps_colorimetry {
        "bt601" => Some(("BT601", None)),
        "bt709" => Some(("BT709", None)),
        "smpte240m" => Some(("SMPTE240M", None)),
        "bt2020" | "bt2020-10" => Some(("BT2020", None)),
        "bt2100-pq" => Some(("BT2100", Some("PQ"))),
        "bt2100-hlg" => Some(("BT2100", Some("HLG"))),
        _ => None,
    }
}

/// Derive `video/x-raw,...` caps from an `application/x-rtp,...`
/// caps that describes an RFC 4175 video media.
///
/// Currently handles `encoding-name=raw` (case-insensitive
/// match — gst-sdp upper-cases the rtpmap encoding-name on
/// parse, so internal caps carry `RAW` after a round-trip
/// through [`parse_sdp`]) with the YCbCr-4:2:2 samplings the
/// `rtpvrawpay` / `rtpvrawdepay` wire format exposes:
///
/// | SDP `sampling` | SDP `depth` | `video/x-raw` `format` |
/// |---|---|---|
/// | `YCbCr-4:2:2` | `8`  | `UYVY` |
/// | `YCbCr-4:2:2` | `10` | `UYVP` |
///
/// Other RFC 4175 samplings (RGB / RGBA / BGR / BGRA / YCbCr-4:4:4 /
/// 4:2:0 / 4:1:1) are not yet handled; see
/// `nvds_nmos_bin/src/helpers/sdp_caps_to_raw_caps.cpp::get_raw_video_caps_from_sdp_caps`
/// for the reference mapping table.
///
/// Optional fields (added only when the SDP provides a recognised
/// value):
///   * `colorimetry` — translated from fmtp `colorimetry` (+ `TCS`
///     for `BT2100`) via [`caps_colorimetry_from_sdp`]. Required for
///     correctness on the V1 receiver path where the depayloader
///     ignores SDP colorimetry; see [`caps_colorimetry_from_sdp`]'s
///     docs for the V1/V2 split rationale.
///
/// Deliberately omitted (kept off the raw caps even when the SDP
/// carries a value):
///   * `pixel-aspect-ratio` (RFC 4175 §6.1 `par=<n>:<d>`) —
///     neither V1 nor V2 depayloader reads the fmtp `par` (V2's
///     `set_sink_caps` only consumes width/height/depth/sampling/
///     colorimetry/exactframerate/chroma-position/interlace) and
///     both emit `pixel-aspect-ratio=1/1` from
///     `gst_video_info_set_format`. ST 2110-20 is square-pixel by
///     normative reference (ITU-R BT.709 / BT.2020 picture geom),
///     so 1/1 is the correct value in practice. Plumbing a
///     non-1/1 PAR onto raw caps would *break* negotiation on the
///     V2-capsfilter path (V2 still emits 1/1, so the
///     intersection collapses), and on the V1-capssetter path it
///     would lie about content that the depay actually treats as
///     square. Once one of the depayloaders honours `par`, we
///     can revisit.
///   * `chroma-site` (RFC 4175 §6.1 `chroma-position=<n>`) — V2
///     reads it; V1 emits the format default (`jpeg` /
///     `MPEG2_CHROMA_SITE_HORIZONTAL` for UYVY) which conflicts
///     with the SMPTE ST 2110-20 norm of co-sited. Adding it
///     would require the V1-capssetter trick too, but unlike
///     colorimetry there's no production-impacting evidence yet
///     that downstream consumers rely on it; defer until a
///     specific consumer needs it. (`identity` / `videoconvert`
///     ignore `chroma-site` mismatch in our smoke matrix.)
///   * SMPTE ST 2110-21 `RANGE=NARROW|FULL|FULLPROTECT` — *neither*
///     V1 (`rtpvrawdepay`) nor V2 (`rtpvrawdepay2`) reads this
///     fmtp parameter; both produce raw caps whose colorimetry
///     preset (`bt709`, `bt2020-10`, `bt2100-pq`, …) carries the
///     standard preset's *narrow* range. Honouring `RANGE=FULL`
///     would require us to bake an explicit
///     `<range>:<matrix>:<transfer>:<primaries>` form (e.g.
///     `1:3:5:1` for full-range BT.709) into raw_caps. That works
///     for the V1 capssetter (it would actively override the
///     depay's narrow preset) but breaks the V2 capsfilter
///     intersection, because V2 also emits the narrow preset and
///     `narrow ∩ full = ∅`. Rather than diverge V1 and V2 output,
///     we drop `RANGE` on the floor here and defer until V2
///     reads `RANGE` upstream; at that point both paths can
///     advertise full-range raw caps uniformly. Track alongside
///     the V2 BT2100 typo fix
///     (<https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/work_items/805>).
///     In practice ST 2110 SDR/HDR broadcast workflows are
///     `RANGE=NARROW` by far the most often; FULL/FULLPROTECT
///     mostly shows up in IPMX graphics / computer-generated
///     content where `videoconvert` will still produce
///     visually-correct output (just at the wrong dynamic range
///     mapping).
fn raw_caps_from_rtp_video(rtp_caps: &gst::Caps) -> Result<gst::Caps, SdpError> {
    let s = rtp_caps
        .structure(0)
        .ok_or_else(|| SdpError::CapsFromMedia("rtp caps empty".to_owned()))?;
    let encoding = s.get::<&str>("encoding-name").unwrap_or("");
    if !encoding.eq_ignore_ascii_case("RAW") {
        return Err(SdpError::UnsupportedEssence(format!(
            "video encoding-name={encoding}"
        )));
    }
    let sampling = s.get::<&str>("sampling").unwrap_or("");
    let depth: u32 = s.get::<&str>("depth").unwrap_or("").parse().unwrap_or(0);
    let format_str = match (sampling, depth) {
        ("YCbCr-4:2:2", 8) => "UYVY",
        ("YCbCr-4:2:2", 10) => "UYVP",
        _ => {
            return Err(SdpError::UnsupportedEssence(format!(
                "video sampling={sampling}, depth={depth}",
            )));
        }
    };
    let width: i32 = s
        .get::<&str>("width")
        .unwrap_or("")
        .parse()
        .map_err(|_| SdpError::UnsupportedEssence("missing or non-integer width".to_owned()))?;
    let height: i32 = s
        .get::<&str>("height")
        .unwrap_or("")
        .parse()
        .map_err(|_| SdpError::UnsupportedEssence("missing or non-integer height".to_owned()))?;
    let (fr_num, fr_den) = parse_exact_framerate(s.get::<&str>("exactframerate").unwrap_or(""))
        .ok_or_else(|| SdpError::UnsupportedEssence("missing or unparseable exactframerate".to_owned()))?;
    // RFC 4175 §6.1's `interlace` flag is a value-less fmtp token;
    // `gst_sdp_media_get_caps_from_media` translates value-less
    // params into `<param>=(string)"1"` on the caps, so a missing
    // field is unambiguously progressive and any `"1"` value is
    // unambiguously interlaced.
    let interlaced = s
        .get::<&str>("interlace")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|v| v != 0);
    let interlace_mode = if interlaced {
        "interleaved"
    } else {
        "progressive"
    };
    let colorimetry_caps = caps_colorimetry_from_sdp(
        s.get::<&str>("colorimetry").unwrap_or(""),
        depth,
        // ST 2110-20 fmtp `TCS=` lands as `tcs=(string)…` on the
        // caps after `caps_from_media`'s lower-case-ification.
        s.get::<&str>("tcs").ok(),
    );
    let mut caps_text = format!(
        "video/x-raw,format={format_str},width={width},height={height},\
         framerate={fr_num}/{fr_den},interlace-mode={interlace_mode}",
    );
    if let Some(c) = colorimetry_caps {
        caps_text.push_str(&format!(",colorimetry={c}"));
    }
    gst::Caps::from_str(&caps_text)
        .map_err(|e| SdpError::UnsupportedEssence(format!("constructing raw caps: {e}")))
}

/// Derive `audio/x-raw,...` caps from an `application/x-rtp,...`
/// caps that describes an ST 2110-30 audio media.
///
/// | SDP `encoding-name` | `audio/x-raw` `format` |
/// |---|---|
/// | `L16` | `S16BE` |
/// | `L24` | `S24BE` |
///
/// ST 2110-30 is restricted to 16- and 24-bit linear PCM; `L8` and
/// other RFC 3551 encodings are out of scope and return
/// [`SdpError::UnsupportedEssence`].
///
/// `rate` comes from `clock-rate` and `channels` from
/// `encoding-params` — RFC 3551's `a=rtpmap:<pt> <enc>/<rate>[/<ch>]`
/// rule says the channel count must be present when `>1` and may
/// be omitted for `1`, so an absent `encoding-params` field
/// canonically denotes mono. `caps_from_media` exposes the third
/// rtpmap slot as a string field named `encoding-params`.
///
/// `ptime` (and `maxptime`) are kept on the RTP caps as
/// `a-ptime` / `a-maxptime` by [`parse_sdp`] — the depayloader
/// reads them from there, and [`build_sdp`]'s
/// `set_media_from_caps` round-trips them back out as standalone
/// `a=ptime:` / `a=maxptime:` lines. We do not copy them onto
/// `audio/x-raw` because the format has no native ptime field.
fn raw_caps_from_rtp_audio(rtp_caps: &gst::Caps) -> Result<gst::Caps, SdpError> {
    let s = rtp_caps
        .structure(0)
        .ok_or_else(|| SdpError::CapsFromMedia("rtp caps empty".to_owned()))?;
    let encoding = s.get::<&str>("encoding-name").unwrap_or("");
    let format_str = match encoding {
        "L16" => "S16BE",
        "L24" => "S24BE",
        _ => {
            return Err(SdpError::UnsupportedEssence(format!(
                "audio encoding-name={encoding}",
            )));
        }
    };
    let rate: i32 = s.get::<i32>("clock-rate").map_err(|_| {
        SdpError::UnsupportedEssence("audio caps missing clock-rate".to_owned())
    })?;
    let channels: i32 = s
        .get::<&str>("encoding-params")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(1);
    let caps_text = format!(
        "audio/x-raw,format={format_str},rate={rate},channels={channels},layout=interleaved",
    );
    gst::Caps::from_str(&caps_text)
        .map_err(|e| SdpError::UnsupportedEssence(format!("constructing raw caps: {e}")))
}

/// Derive `meta/x-st-2038,...` caps from an `application/x-rtp,...`
/// caps that describes an RFC 8331 / SMPTE ST 2110-40 ANC media.
///
/// The produced caps match the sink-pad template of `gst-plugins-rs`'
/// `rtpsmpte291pay` (and the src-pad template of `rtpsmpte291depay`)
/// and the existing `meta/x-st-2038, framerate=N/D` convention used
/// by [`crate::flow_def::build_data_body`]:
///
/// ```text
/// meta/x-st-2038, alignment=frame[, framerate=N/D]
/// ```
///
/// `framerate` is hoisted from the `exactframerate` token on the
/// `a=fmtp:` line — ST 2110-40 §6.4 reuses the same fmtp convention
/// as ST 2110-20 / RFC 4175 video rather than RFC 4566's separate
/// `a=framerate:` attribute, so the value flows through
/// `caps_from_media` onto `rtp_caps` as a string we just parse with
/// [`parse_exact_framerate`]. Absent `exactframerate` is fine: ANC
/// is clocked from the paired video flow at runtime and the caller
/// (typically the element's `caps` property or the caps-merge on the
/// `nmossrc` ghost src pad) supplies the framerate downstream.
fn raw_caps_from_rtp_data(rtp_caps: &gst::Caps) -> Result<gst::Caps, SdpError> {
    let s = rtp_caps
        .structure(0)
        .ok_or_else(|| SdpError::CapsFromMedia("rtp caps empty".to_owned()))?;
    let encoding = s.get::<&str>("encoding-name").unwrap_or("");
    if !encoding.eq_ignore_ascii_case("SMPTE291") {
        return Err(SdpError::UnsupportedEssence(format!(
            "data encoding-name={encoding}",
        )));
    }
    let framerate = s
        .get::<&str>("exactframerate")
        .ok()
        .and_then(parse_exact_framerate);
    let mut caps_text = String::from("meta/x-st-2038,alignment=frame");
    if let Some((num, den)) = framerate {
        caps_text.push_str(&format!(",framerate={num}/{den}"));
    }
    gst::Caps::from_str(&caps_text)
        .map_err(|e| SdpError::UnsupportedEssence(format!("constructing data caps: {e}")))
}

/// Parse an SDP `exactframerate` value into a (numerator,
/// denominator) pair. Accepts both integer (`50`) and rational
/// (`30000/1001`) forms; returns `None` for empty/malformed input
/// or zero denominator.
fn parse_exact_framerate(value: &str) -> Option<(u32, u32)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some((num_s, den_s)) = value.split_once('/') {
        let num: u32 = num_s.trim().parse().ok()?;
        let den: u32 = den_s.trim().parse().ok()?;
        if den == 0 {
            return None;
        }
        Some((num, den))
    } else {
        let num: u32 = value.parse().ok()?;
        Some((num, 1))
    }
}

/// Format a `(numerator, denominator)` framerate pair as the
/// canonical SDP `exactframerate` value. Inverse of
/// [`parse_exact_framerate`]: integer rates (`den == 1`) are
/// emitted bare (`"50"`), and rationals as `"<num>/<den>"`
/// (`"30000/1001"`).
fn format_exact_framerate(num: u32, den: u32) -> String {
    if den == 1 {
        format!("{num}")
    } else {
        format!("{num}/{den}")
    }
}

/// Format an `a=ptime:` (or `a=maxptime:`) value, given a
/// duration in nanoseconds, as decimal milliseconds suitable
/// for the SDP wire form. Whole-millisecond values render
/// bare (`"1"`); sub-millisecond values render with the
/// minimum fractional digits Rust's default `f64` `Display`
/// produces (`"0.125"`).
///
/// nmos-cpp's `make_sdp_audio_parameters` uses the same
/// "integer when whole, decimal otherwise" pattern; receivers
/// in the wild handle both forms because RFC 4566 §6 defines
/// the value as `<integer> | <floating-point>`.
fn format_ptime_ns_as_ms(ns: u64) -> String {
    if ns % 1_000_000 == 0 {
        format!("{}", ns / 1_000_000)
    } else {
        let ms = ns as f64 / 1_000_000.0;
        format!("{ms}")
    }
}

/// Build an `application/x-rtp,...` caps describing an RFC 4175
/// raw-video media that wraps the supplied `video/x-raw` caps.
/// Inverse of [`raw_caps_from_rtp_video`]: the synthesised RTP
/// caps round-trip back to an equivalent `video/x-raw` caps via
/// the parse-direction helper (modulo preset-collapsing
/// colorimetry — see [`sdp_colorimetry_from_caps`]).
///
/// Source `video/x-raw` fields consumed:
///
/// | `video/x-raw` field | `application/x-rtp` field           |
/// |---|---|
/// | `format` (`UYVY` / `UYVP`) | `sampling=YCbCr-4:2:2`, `depth=8` / `10` |
/// | `width`             | `width`                             |
/// | `height`            | `height`                            |
/// | `framerate`         | `exactframerate`                    |
/// | `interlace-mode`    | `interlace=1` (when `interleaved`)  |
/// | `colorimetry`       | `colorimetry=` + optional `tcs=`    |
///
/// Always-emitted ST 2110-20 §6.4 fmtp slots: `pm=2110GPM` and
/// `ssn=ST2110-20:2017` (see [`defaults::ST2110_20_PM`] /
/// [`defaults::ST2110_20_SSN`]). nmos-cpp's
/// `make_video_sdp_parameters` emits the same defaults so wire
/// interop with senders/receivers driven by `libnvnmos` is
/// preserved.
///
/// Returns [`SdpError::UnsupportedEssence`] for `format=` values
/// outside the {`UYVY`, `UYVP`} subset [`raw_caps_from_rtp_video`]
/// understands today; widening the matrix here without widening
/// the inverse first would break the round-trip contract.
fn rtp_caps_from_raw_video(raw_caps: &gst::Caps, payload_type: u8) -> Result<gst::Caps, SdpError> {
    let s = raw_caps
        .structure(0)
        .ok_or_else(|| SdpError::UnsupportedEssence("raw video caps empty".to_owned()))?;
    let format = s
        .get::<&str>("format")
        .map_err(|_| SdpError::UnsupportedEssence("raw video caps missing format".to_owned()))?;
    let (sampling, depth) = match format {
        "UYVY" => ("YCbCr-4:2:2", 8u32),
        "UYVP" => ("YCbCr-4:2:2", 10u32),
        other => {
            return Err(SdpError::UnsupportedEssence(format!(
                "video format={other}"
            )));
        }
    };
    let width = s
        .get::<i32>("width")
        .map_err(|_| SdpError::UnsupportedEssence("raw video caps missing width".to_owned()))?;
    let height = s
        .get::<i32>("height")
        .map_err(|_| SdpError::UnsupportedEssence("raw video caps missing height".to_owned()))?;
    let (fr_num, fr_den) = s
        .get::<gst::Fraction>("framerate")
        .map(|f| (f.numer() as u32, f.denom() as u32))
        .map_err(|_| {
            SdpError::UnsupportedEssence("raw video caps missing framerate".to_owned())
        })?;
    let exactframerate = format_exact_framerate(fr_num, fr_den);
    let interlace_mode = s.get::<&str>("interlace-mode").unwrap_or("progressive");
    let colorimetry_caps = s.get::<&str>("colorimetry").ok();
    let colorimetry_sdp = colorimetry_caps.and_then(sdp_colorimetry_from_caps);

    // `encoding-name=raw` (RFC 4175 §6.7) and `PM=`/`SSN=`
    // (ST 2110-20 §6.3) emitted in canonical wire case;
    // `set_media_from_caps` passes caps fields through verbatim.
    let mut caps_text = format!(
        "application/x-rtp,\
         media=(string)video,\
         clock-rate=(int){clk},\
         encoding-name=(string)raw,\
         payload=(int){pt},\
         sampling=(string){sampling},\
         depth=(string){depth},\
         width=(string){width},\
         height=(string){height},\
         exactframerate=(string){exactframerate},\
         PM=(string){pm},\
         SSN=(string){ssn}",
        clk = defaults::VIDEO_CLOCK_RATE,
        pt = payload_type,
        pm = defaults::ST2110_20_PM,
        ssn = defaults::ST2110_20_SSN,
    );
    if interlace_mode == "interleaved" {
        caps_text.push_str(",interlace=(string)1");
    }
    // `colorimetry=` is REQUIRED by ST 2110-20 / RFC 4175 fmtp.
    // nmos-cpp's `get_video_raw_parameters` throws if absent
    // (see `nmos-cpp/Development/nmos/sdp_utils.cpp::get_video_raw_parameters`),
    // and libnvnmos's `add_nmos_sender_to_node_server` catches
    // the throw and silently returns `false`. When the caller's
    // `video/x-raw` caps carry an explicit `colorimetry`, we
    // round-trip it via [`sdp_colorimetry_from_caps`]; when
    // they don't, we default to ST 2110 SDR's
    // [`defaults::ST2110_20_COLORIMETRY`] (`BT709`) — the same
    // value `nmos-cpp`'s `make_video_sdp_parameters` picks when
    // a Flow has no colorimetry tag set and the value our test
    // SDP fixtures use. Callers wanting BT2020 / BT2100 etc.
    // need to set `colorimetry=` upstream of the `nmossink`.
    let (colorimetry, tcs) =
        colorimetry_sdp.unwrap_or((defaults::ST2110_20_COLORIMETRY, None));
    caps_text.push_str(&format!(",colorimetry=(string){colorimetry}"));
    if let Some(tcs_value) = tcs {
        caps_text.push_str(&format!(",tcs=(string){tcs_value}"));
    }
    gst::Caps::from_str(&caps_text)
        .map_err(|e| SdpError::UnsupportedEssence(format!("constructing rtp caps: {e}")))
}

/// Build an `application/x-rtp,...` caps describing an
/// ST 2110-30 audio media that wraps the supplied `audio/x-raw`
/// caps. Inverse of [`raw_caps_from_rtp_audio`].
///
/// | `audio/x-raw` field | `application/x-rtp` field |
/// |---|---|
/// | `format=S16BE` | `encoding-name=L16` |
/// | `format=S24BE` | `encoding-name=L24` |
/// | `rate`         | `clock-rate`        |
/// | `channels`     | `encoding-params`   |
///
/// `ptime_ns` and `maxptime_ns` are not part of `audio/x-raw`;
/// the caller threads them through from `transport_caps` /
/// [`defaults::AUDIO_PTIME_NS`]. They land as `a-ptime` /
/// `a-maxptime` *string* caps fields that [`build_sdp`]'s
/// `set_media_from_caps` re-emits as standalone `a=ptime:` /
/// `a=maxptime:` lines.
///
/// Returns [`SdpError::UnsupportedEssence`] for `format=` values
/// outside ST 2110-30's {`S16BE`, `S24BE`} restriction. RFC 3551
/// L8, μ-law, A-law, etc. are out of scope.
fn rtp_caps_from_raw_audio(
    raw_caps: &gst::Caps,
    payload_type: u8,
    ptime_ns: u64,
    maxptime_ns: Option<u64>,
) -> Result<gst::Caps, SdpError> {
    let s = raw_caps
        .structure(0)
        .ok_or_else(|| SdpError::UnsupportedEssence("raw audio caps empty".to_owned()))?;
    let format = s
        .get::<&str>("format")
        .map_err(|_| SdpError::UnsupportedEssence("raw audio caps missing format".to_owned()))?;
    let encoding = match format {
        "S16BE" => "L16",
        "S24BE" => "L24",
        other => {
            return Err(SdpError::UnsupportedEssence(format!(
                "audio format={other}"
            )));
        }
    };
    let rate = s
        .get::<i32>("rate")
        .map_err(|_| SdpError::UnsupportedEssence("raw audio caps missing rate".to_owned()))?;
    let channels = s
        .get::<i32>("channels")
        .map_err(|_| SdpError::UnsupportedEssence("raw audio caps missing channels".to_owned()))?;

    let ptime = format_ptime_ns_as_ms(ptime_ns);
    let mut caps_text = format!(
        "application/x-rtp,\
         media=(string)audio,\
         clock-rate=(int){rate},\
         encoding-name=(string){encoding},\
         payload=(int){pt},\
         encoding-params=(string){channels},\
         a-ptime=(string){ptime}",
        pt = payload_type,
    );
    if let Some(max) = maxptime_ns {
        let maxptime = format_ptime_ns_as_ms(max);
        caps_text.push_str(&format!(",a-maxptime=(string){maxptime}"));
    }
    gst::Caps::from_str(&caps_text)
        .map_err(|e| SdpError::UnsupportedEssence(format!("constructing rtp caps: {e}")))
}

/// Build an `application/x-rtp,...` caps describing an RFC 8331 /
/// SMPTE ST 2110-40 ancillary-data media that wraps the supplied
/// `meta/x-st-2038` caps. Inverse of [`raw_caps_from_rtp_data`].
///
/// | `meta/x-st-2038` field | `application/x-rtp` field |
/// |---|---|
/// | (fixed)                | `media=video`, `encoding-name=SMPTE291` |
/// | `framerate` (optional) | `exactframerate`                        |
///
/// ANC is carried on `m=video` per RFC 8331 §3; the dispatch
/// over to ANC handling is keyed off `encoding-name=SMPTE291`
/// in [`parse_sdp`].
///
/// `framerate` is propagated to fmtp `exactframerate` when
/// present (it is on `nmossrc`'s `caps` property when the user
/// wants ANC clocked to a specific video frame rate) and
/// omitted otherwise — RFC 8331 / ST 2110-40 §6.4 makes the
/// parameter optional and the depayloader does not require it.
fn rtp_caps_from_raw_data(raw_caps: &gst::Caps, payload_type: u8) -> Result<gst::Caps, SdpError> {
    let s = raw_caps
        .structure(0)
        .ok_or_else(|| SdpError::UnsupportedEssence("ANC caps empty".to_owned()))?;
    if s.name() != "meta/x-st-2038" {
        return Err(SdpError::UnsupportedEssence(format!(
            "ANC essence={}",
            s.name()
        )));
    }
    let framerate = s.get::<gst::Fraction>("framerate").ok();
    let mut caps_text = format!(
        "application/x-rtp,\
         media=(string)video,\
         clock-rate=(int){clk},\
         encoding-name=(string)SMPTE291,\
         payload=(int){pt}",
        clk = defaults::ANC_CLOCK_RATE,
        pt = payload_type,
    );
    if let Some(f) = framerate {
        let exactframerate = format_exact_framerate(f.numer() as u32, f.denom() as u32);
        caps_text.push_str(&format!(",exactframerate=(string){exactframerate}"));
    }
    gst::Caps::from_str(&caps_text)
        .map_err(|e| SdpError::UnsupportedEssence(format!("constructing rtp caps: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdp_passthrough::reject_unsupported_multi_media;

    fn init_gst() {
        let _ = gst::init();
    }

    // `defaults` regression guards: every constant pins a
    // canonical `nmos-cpp` source location and the value
    // should change only when nmos-cpp's matching constant
    // changes. The tests are deliberately literal — they
    // exist to trip CI if a constant is bumped accidentally
    // (which would otherwise propagate silently through the
    // synthesis stack).

    #[test]
    fn defaults_payload_types_are_in_rfc_3551_dynamic_range() {
        // RFC 3551 §6: dynamic payload-type range is 96..=127.
        // ST 2110 / nmos-cpp pick from this range.
        for (name, pt) in [
            ("VIDEO", defaults::VIDEO_PAYLOAD_TYPE),
            ("AUDIO", defaults::AUDIO_PAYLOAD_TYPE),
            ("ANC", defaults::ANC_PAYLOAD_TYPE),
        ] {
            assert!(
                (96..=127).contains(&pt),
                "{name}_PAYLOAD_TYPE = {pt} is outside RFC 3551 §6 dynamic range (96..=127)",
            );
        }
    }

    #[test]
    fn defaults_payload_types_match_nmos_cpp() {
        // nmos-cpp/Development/nmos/sdp_utils.h:673-680
        assert_eq!(defaults::VIDEO_PAYLOAD_TYPE, 96);
        assert_eq!(defaults::AUDIO_PAYLOAD_TYPE, 97);
        assert_eq!(defaults::ANC_PAYLOAD_TYPE, 100);
    }

    #[test]
    fn defaults_audio_ptime_is_one_millisecond_in_nanoseconds() {
        // nmos-cpp/Development/nmos/sdp_utils.cpp:305:
        //   params.packet_time = packet_time ? *packet_time : 1;
        // (`packet_time` is `double` in ms; we represent in
        // ns because `rtp*pay`'s `min-ptime` / `max-ptime`
        // properties take nanoseconds.)
        assert_eq!(defaults::AUDIO_PTIME_NS, 1_000_000);
    }

    #[test]
    fn defaults_rtp_port_matches_nmos_cpp_auto_rtp_port() {
        // nmos-cpp/Development/nmos/connection_api.h:101:
        //   int auto_rtp_port = 5004
        assert_eq!(defaults::RTP_PORT, 5004);
    }

    #[test]
    fn defaults_origin_address_is_rfc_4566_unspecified() {
        // RFC 4566 §5.2: any IPv4 address is permitted when
        // the originator's real address is unknown; `0.0.0.0`
        // is the canonical "unspecified" form.
        assert_eq!(defaults::ORIGIN_ADDRESS, "0.0.0.0");
        // Sanity: parseable as `Ipv4Addr::UNSPECIFIED`.
        let parsed: std::net::Ipv4Addr = defaults::ORIGIN_ADDRESS
            .parse()
            .expect("ORIGIN_ADDRESS must parse as an Ipv4Addr");
        assert_eq!(parsed, std::net::Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn defaults_multicast_ttl_is_64() {
        assert_eq!(defaults::MULTICAST_TTL, 64);
    }

    /// Pins `gst-sdp`'s `SDPMedia::add_connection` behaviour so
    /// `build_sdp` can pass `MULTICAST_TTL` unconditionally: the
    /// emitter is expected to suppress the `/ttl` suffix for
    /// unicast IPv4 destinations (per RFC 4566 §5.7) and to
    /// emit it for multicast destinations. If a future
    /// `gstreamer-sdp` release stops honouring that, this test
    /// trips and we'll need to gate the call ourselves.
    #[test]
    fn gst_sdp_strips_ttl_for_unicast_c_lines() {
        init_gst();
        for (addr, expect_suffix) in [
            ("239.1.1.1", true), // multicast — TTL required
            ("192.0.2.10", false), // unicast — TTL must be omitted
        ] {
            let mut msg = SDPMessage::new();
            msg.set_version("0");
            let mut m = SDPMedia::new();
            m.set_media("video");
            m.set_proto("RTP/AVP");
            m.set_port_info(5004, 1);
            m.add_connection("IN", "IP4", addr, defaults::MULTICAST_TTL, 0);
            msg.add_media(m);
            let text = msg.as_text().unwrap();
            let c_line = text
                .lines()
                .find(|l| l.starts_with("c="))
                .expect("c= line must be present");
            let suffix = format!("/{}", defaults::MULTICAST_TTL);
            assert_eq!(
                c_line.contains(&suffix),
                expect_suffix,
                "{addr}: expected suffix `{suffix}` presence = {expect_suffix}, got `{c_line}`",
            );
        }
    }

    /// Round-trip guard: parse a multicast fixture SDP through
    /// `build_sdp` and confirm the `c=` line carries
    /// `/MULTICAST_TTL`. If the constant is ever refactored
    /// away from `add_connection` (e.g. accidentally back to a
    /// literal), or the multicast detection breaks, this trips.
    #[test]
    fn defaults_build_sdp_emits_multicast_ttl_for_multicast_c_line() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let text = build_sdp(&media, test_session()).expect("build");
        let expected = format!("/{}\r\n", defaults::MULTICAST_TTL);
        assert!(
            text.contains(&expected),
            "build_sdp must emit `c=...<addr>/{}` for the multicast fixture; got: {text}",
            defaults::MULTICAST_TTL,
        );
    }

    /// 1920×1080p50 SMPTE ST 2110-20 / RFC 4175 SDP carrying
    /// YCbCr-4:2:2 10-bit sampling (i.e. `format=UYVP` on the
    /// `rtpvrawpay`/`rtpvrawdepay` wire), modelled after the
    /// worked example in SMPTE ST 2110-20 plus the
    /// `nvds_nmos_bin` `x-nvnmos-*` attribute extensions.
    const VIDEO_YCBCR_422_10BIT_1080P50_SDP: &str = concat!(
        "v=0\r\n",
        "o=- 1234567890 0 IN IP4 192.0.2.10\r\n",
        "s=Example\r\n",
        "t=0 0\r\n",
        "m=video 5004 RTP/AVP 96\r\n",
        "c=IN IP4 239.1.1.1/64\r\n",
        "a=source-filter: incl IN IP4 239.1.1.1 192.0.2.20\r\n",
        "a=rtpmap:96 raw/90000\r\n",
        "a=fmtp:96 sampling=YCbCr-4:2:2; width=1920; height=1080;",
        " exactframerate=50; depth=10; TCS=SDR; colorimetry=BT709;",
        " PM=2110GPM; SSN=ST2110-20:2017\r\n",
        "a=mediaclk:direct=0\r\n",
        "a=ts-refclk:ptp=IEEE1588-2008:traceable\r\n",
        "a=x-nvnmos-iface-ip:192.0.2.11\r\n",
        "a=x-nvnmos-src-port:5005\r\n",
    );

    #[test]
    fn video_uyvp_happy_path() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        assert_eq!(media.format, FlowFormat::Video);
        assert_eq!(media.primary.destination_ip, "239.1.1.1");
        assert_eq!(media.primary.destination_port, 5004);
        assert_eq!(media.primary.interface_ip.as_deref(), Some("192.0.2.11"));
        assert_eq!(media.primary.source_ip.as_deref(), Some("192.0.2.20"));
        assert_eq!(media.primary.source_port, Some(5005));
        assert!(media.secondary.is_none());

        let rtp_s = media.rtp_caps.structure(0).expect("rtp caps");
        assert_eq!(rtp_s.name().as_str(), "application/x-rtp");
        assert_eq!(rtp_s.get::<&str>("media").unwrap(), "video");
        assert_eq!(rtp_s.get::<&str>("encoding-name").unwrap(), "RAW");
        assert_eq!(rtp_s.get::<i32>("payload").unwrap(), 96);
        assert_eq!(rtp_s.get::<i32>("clock-rate").unwrap(), 90_000);

        let raw_s = media.raw_caps.structure(0).expect("raw caps");
        assert_eq!(raw_s.name().as_str(), "video/x-raw");
        assert_eq!(raw_s.get::<&str>("format").unwrap(), "UYVP");
        assert_eq!(raw_s.get::<i32>("width").unwrap(), 1920);
        assert_eq!(raw_s.get::<i32>("height").unwrap(), 1080);
        assert_eq!(
            raw_s.get::<gst::Fraction>("framerate").unwrap(),
            gst::Fraction::new(50, 1)
        );
        assert_eq!(raw_s.get::<&str>("interlace-mode").unwrap(), "progressive");
        // The fixture's fmtp `colorimetry=BT709` must round-trip to
        // the GStreamer preset `bt709` on raw caps; otherwise the
        // V1-receiver capssetter fix-up has no value to inject and
        // the V2-receiver capsfilter intersection drops it.
        assert_eq!(raw_s.get::<&str>("colorimetry").unwrap(), "bt709");
    }

    #[test]
    fn source_filter_absent_yields_none_source_ip() {
        init_gst();
        let sdp = VIDEO_YCBCR_422_10BIT_1080P50_SDP.replace(
            "a=source-filter: incl IN IP4 239.1.1.1 192.0.2.20\r\n",
            "",
        );
        let media = parse_sdp(&sdp).expect("parse");
        assert_eq!(media.primary.source_ip, None);
    }

    #[test]
    fn exclude_mode_source_filter_yields_none_source_ip() {
        init_gst();
        let sdp = VIDEO_YCBCR_422_10BIT_1080P50_SDP.replace(
            "a=source-filter: incl IN IP4 239.1.1.1 192.0.2.20\r\n",
            "a=source-filter: excl IN IP4 239.1.1.1 192.0.2.99\r\n",
        );
        let media = parse_sdp(&sdp).expect("parse");
        assert_eq!(media.primary.source_ip, None);
    }

    #[test]
    fn x_nvnmos_iface_ip_absent_yields_none_interface_ip() {
        init_gst();
        let sdp = VIDEO_YCBCR_422_10BIT_1080P50_SDP.replace("a=x-nvnmos-iface-ip:192.0.2.11\r\n", "");
        let media = parse_sdp(&sdp).expect("parse");
        assert_eq!(media.primary.interface_ip, None);
    }

    #[test]
    fn x_nvnmos_src_port_absent_yields_none_source_port() {
        init_gst();
        let sdp = VIDEO_YCBCR_422_10BIT_1080P50_SDP.replace("a=x-nvnmos-src-port:5005\r\n", "");
        let media = parse_sdp(&sdp).expect("parse");
        assert_eq!(media.primary.source_port, None);
    }

    #[test]
    fn fractional_framerate_is_supported() {
        init_gst();
        let sdp = VIDEO_YCBCR_422_10BIT_1080P50_SDP.replace("exactframerate=50;", "exactframerate=30000/1001;");
        let media = parse_sdp(&sdp).expect("parse");
        let raw_s = media.raw_caps.structure(0).expect("raw caps");
        assert_eq!(
            raw_s.get::<gst::Fraction>("framerate").unwrap(),
            gst::Fraction::new(30_000, 1_001)
        );
    }

    /// Round-trip the colorimetry mapping through `parse_sdp` rather
    /// than calling `caps_colorimetry_from_sdp` directly; this keeps the
    /// fmtp-key-name plumbing (`colorimetry` vs `tcs`) covered too.
    fn colorimetry_via_parse(sdp_colorimetry: &str, depth: u32, tcs: Option<&str>) -> Option<String> {
        init_gst();
        let depth_str = depth.to_string();
        let mut sdp = VIDEO_YCBCR_422_10BIT_1080P50_SDP
            .replace("depth=10;", &format!("depth={depth_str};"))
            .replace("colorimetry=BT709;", &format!("colorimetry={sdp_colorimetry};"));
        // The fixture's `TCS=SDR;` lives just before `colorimetry=`,
        // so rewrite the TCS value too (or drop it).
        sdp = match tcs {
            Some(v) => sdp.replace("TCS=SDR;", &format!("TCS={v};")),
            None => sdp.replace(" TCS=SDR;", ""),
        };
        let media = parse_sdp(&sdp).expect("parse");
        media
            .raw_caps
            .structure(0)
            .and_then(|s| s.get::<&str>("colorimetry").ok().map(str::to_owned))
    }

    #[test]
    fn colorimetry_bt709_maps_to_bt709_caps() {
        // SDP fmtp `colorimetry=BT709` is the common case for HD
        // SDR; the V2 depay would also emit `bt709` on output, so
        // this needs to round-trip to keep the V2 capsfilter
        // intersection happy.
        assert_eq!(
            colorimetry_via_parse("BT709", 10, Some("SDR")).as_deref(),
            Some("bt709"),
        );
    }

    #[test]
    fn colorimetry_bt601_maps_to_bt601_caps() {
        assert_eq!(
            colorimetry_via_parse("BT601", 8, Some("SDR")).as_deref(),
            Some("bt601"),
        );
        // The `-5` suffix is the ITU-R version qualifier; both
        // spellings appear in the wild.
        assert_eq!(
            colorimetry_via_parse("BT601-5", 8, Some("SDR")).as_deref(),
            Some("bt601"),
        );
    }

    #[test]
    fn colorimetry_bt2020_is_depth_aware() {
        // At depth=10 (or higher) the GStreamer preset is
        // `bt2020-10` (matrix + 10-bit colorimetry); at depth=8 it
        // collapses to the looser `bt2020`. Mirrors V2's split.
        assert_eq!(
            colorimetry_via_parse("BT2020", 10, Some("SDR")).as_deref(),
            Some("bt2020-10"),
        );
        assert_eq!(
            colorimetry_via_parse("BT2020", 8, Some("SDR")).as_deref(),
            Some("bt2020"),
        );
    }

    #[test]
    fn colorimetry_bt2100_uses_tcs_or_assumes_pq() {
        // BT2100 implies a transfer characteristic — PQ for
        // HDR10, HLG for broadcast HLG-HDR.
        assert_eq!(
            colorimetry_via_parse("BT2100", 10, Some("PQ")).as_deref(),
            Some("bt2100-pq"),
        );
        assert_eq!(
            colorimetry_via_parse("BT2100", 10, Some("HLG")).as_deref(),
            Some("bt2100-hlg"),
        );
        // Missing TCS — V2's source intends "assume PQ" but a
        // typo (`bt2100-pg`) neutralises that to `None`. We
        // honour V2's stated intent here so the V1 capssetter
        // path doesn't fall back to `bt601` on UYVY/UYVP HDR
        // streams. The choice is V2-output-compatible because
        // V2 emits no colorimetry in this case and intersecting
        // "missing" with our `bt2100-pq` still gives
        // `bt2100-pq` downstream.
        assert_eq!(
            colorimetry_via_parse("BT2100", 10, None).as_deref(),
            Some("bt2100-pq"),
        );
        // Unknown TCS values also land on PQ.
        assert_eq!(
            colorimetry_via_parse("BT2100", 10, Some("LINEAR")).as_deref(),
            Some("bt2100-pq"),
        );
    }

    #[test]
    fn colorimetry_unspecified_or_unknown_yields_none() {
        // `UNSPECIFIED` and unrecognised values (`ST2065-1`, `XYZ`)
        // must NOT smuggle a guess onto the caps; the V1 depay's
        // format-default takes over and the V2 depay also emits
        // nothing. depth=10 here is just to stay inside the
        // `YCbCr-4:2:2` samplings `raw_caps_from_rtp_video` accepts
        // today — the depth value doesn't otherwise affect the
        // `XYZ` / `UNSPECIFIED` arms.
        assert_eq!(colorimetry_via_parse("UNSPECIFIED", 10, Some("SDR")), None);
        assert_eq!(colorimetry_via_parse("XYZ", 10, None), None);
    }

    #[test]
    fn malformed_text_is_parse_error() {
        init_gst();
        let err = parse_sdp("not an SDP at all").expect_err("must error");
        assert!(matches!(err, SdpError::NoMedia | SdpError::Parse(_)));
    }

    #[test]
    fn multi_media_sdp_is_rejected_with_attributed_error() {
        init_gst();
        let mut sdp = VIDEO_YCBCR_422_10BIT_1080P50_SDP.to_owned();
        sdp.push_str("m=video 5006 RTP/AVP 96\r\n");
        sdp.push_str("c=IN IP4 239.1.1.2/64\r\n");
        sdp.push_str("a=rtpmap:96 raw/90000\r\n");
        sdp.push_str(
            "a=fmtp:96 sampling=YCbCr-4:2:2; width=1920; height=1080;\
             exactframerate=50; depth=10\r\n",
        );
        let err = parse_sdp(&sdp).expect_err("must error");
        assert!(
            matches!(err, SdpError::MultipleMedia(2)),
            "expected MultipleMedia(2), got {err:?}"
        );
    }

    /// 48 kHz L24 stereo SMPTE ST 2110-30 SDP, modelled after the
    /// worked example in SMPTE ST 2110-30 plus the `nvds_nmos_bin`
    /// `x-nvnmos-*` attribute extensions.
    const AUDIO_L24_48K_STEREO_SDP: &str = concat!(
        "v=0\r\n",
        "o=- 1 0 IN IP4 192.0.2.10\r\n",
        "s=Example\r\n",
        "t=0 0\r\n",
        "m=audio 5004 RTP/AVP 97\r\n",
        "c=IN IP4 239.2.2.2/64\r\n",
        "a=source-filter: incl IN IP4 239.2.2.2 192.0.2.30\r\n",
        "a=rtpmap:97 L24/48000/2\r\n",
        "a=fmtp:97 channel-order=SMPTE2110.(ST)\r\n",
        "a=ptime:0.125\r\n",
        "a=mediaclk:direct=0\r\n",
        "a=ts-refclk:ptp=IEEE1588-2008:traceable\r\n",
        "a=x-nvnmos-iface-ip:192.0.2.11\r\n",
        "a=x-nvnmos-src-port:5007\r\n",
    );

    #[test]
    fn audio_l24_stereo_happy_path() {
        init_gst();
        let media = parse_sdp(AUDIO_L24_48K_STEREO_SDP).expect("parse");
        assert_eq!(media.format, FlowFormat::Audio);
        assert_eq!(media.primary.destination_ip, "239.2.2.2");
        assert_eq!(media.primary.destination_port, 5004);
        assert_eq!(media.primary.interface_ip.as_deref(), Some("192.0.2.11"));
        assert_eq!(media.primary.source_ip.as_deref(), Some("192.0.2.30"));
        assert_eq!(media.primary.source_port, Some(5007));
        assert!(media.secondary.is_none());

        let rtp_s = media.rtp_caps.structure(0).expect("rtp caps");
        assert_eq!(rtp_s.name().as_str(), "application/x-rtp");
        assert_eq!(rtp_s.get::<&str>("media").unwrap(), "audio");
        assert_eq!(rtp_s.get::<&str>("encoding-name").unwrap(), "L24");
        assert_eq!(rtp_s.get::<i32>("clock-rate").unwrap(), 48_000);
        assert_eq!(rtp_s.get::<&str>("encoding-params").unwrap(), "2");
        assert_eq!(
            rtp_s.get::<&str>("a-ptime").unwrap(),
            "0.125",
            "ptime rides on rtp caps as `a-ptime` for the depayloader and SDP round-trip",
        );

        let raw_s = media.raw_caps.structure(0).expect("raw caps");
        assert_eq!(raw_s.name().as_str(), "audio/x-raw");
        assert_eq!(raw_s.get::<&str>("format").unwrap(), "S24BE");
        assert_eq!(raw_s.get::<i32>("rate").unwrap(), 48_000);
        assert_eq!(raw_s.get::<i32>("channels").unwrap(), 2);
        assert_eq!(raw_s.get::<&str>("layout").unwrap(), "interleaved");
        assert!(
            raw_s.get::<&str>("ptime").is_err() && raw_s.get::<&str>("a-ptime").is_err(),
            "ptime must not leak onto audio/x-raw caps",
        );
    }

    #[test]
    fn audio_l24_ptime_round_trips_via_build_sdp() {
        init_gst();
        let original = parse_sdp(AUDIO_L24_48K_STEREO_SDP).expect("parse original");
        let text = build_sdp(&original, test_session()).expect("build");
        assert!(
            text.contains("\r\na=ptime:0.125\r\n"),
            "built SDP must include a=ptime:0.125: {text}",
        );
        let round_tripped = parse_sdp(&text).expect("parse round-tripped");
        let rt_rtp = round_tripped.rtp_caps.structure(0).expect("rtp caps");
        assert_eq!(rt_rtp.get::<&str>("a-ptime").unwrap(), "0.125");
    }

    #[test]
    fn audio_l16_stereo_maps_to_s16be() {
        init_gst();
        let sdp = AUDIO_L24_48K_STEREO_SDP.replace("L24/48000/2", "L16/48000/2");
        let media = parse_sdp(&sdp).expect("parse");
        let raw_s = media.raw_caps.structure(0).expect("raw caps");
        assert_eq!(raw_s.get::<&str>("format").unwrap(), "S16BE");
        assert_eq!(raw_s.get::<i32>("channels").unwrap(), 2);
    }

    #[test]
    fn audio_l24_mono_default_channels_when_encoding_params_missing() {
        init_gst();
        let sdp = AUDIO_L24_48K_STEREO_SDP.replace("L24/48000/2", "L24/48000");
        let media = parse_sdp(&sdp).expect("parse");
        let raw_s = media.raw_caps.structure(0).expect("raw caps");
        assert_eq!(
            raw_s.get::<i32>("channels").unwrap(),
            1,
            "RFC 3551 default channel count is 1 (mono)",
        );
    }

    #[test]
    fn audio_l8_is_rejected_as_unsupported() {
        init_gst();
        let sdp = AUDIO_L24_48K_STEREO_SDP.replace("L24/48000/2", "L8/48000/2");
        let err = parse_sdp(&sdp).expect_err("L8 is out of scope for ST 2110-30");
        match err {
            SdpError::UnsupportedEssence(detail) => {
                assert!(
                    detail.contains("L8"),
                    "error message should mention L8: {detail}"
                );
            }
            other => panic!("expected UnsupportedEssence, got {other:?}"),
        }
    }

    #[test]
    fn video_interlaced_fmtp_flag_is_interleaved() {
        init_gst();
        // RFC 4175 §6.1's `interlace` is a value-less fmtp flag;
        // GStreamer turns value-less fmtp tokens into `<key>="1"`
        // on the caps (gstsdpmessage.c line ~3749) so the field
        // appears as `interlace=(string)"1"`.
        let sdp = VIDEO_YCBCR_422_10BIT_1080P50_SDP
            .replace("exactframerate=50;", "exactframerate=25; interlace;");
        let media = parse_sdp(&sdp).expect("parse");
        let raw_s = media.raw_caps.structure(0).expect("raw caps");
        assert_eq!(
            raw_s.get::<&str>("interlace-mode").unwrap(),
            "interleaved",
            "value-less RFC 4175 `interlace` flag must map to interlace-mode=interleaved",
        );
    }

    #[test]
    fn video_uyvy_happy_path() {
        init_gst();
        let sdp = VIDEO_YCBCR_422_10BIT_1080P50_SDP.replace("depth=10;", "depth=8;");
        let media = parse_sdp(&sdp).expect("parse");
        let raw_s = media.raw_caps.structure(0).expect("raw caps");
        assert_eq!(
            raw_s.get::<&str>("format").unwrap(),
            "UYVY",
            "YCbCr-4:2:2 depth=8 is the RFC 4175 UYVY wire format",
        );
        assert_eq!(raw_s.get::<i32>("width").unwrap(), 1920);
        assert_eq!(raw_s.get::<i32>("height").unwrap(), 1080);
    }

    #[test]
    fn unsupported_video_sampling_is_rejected_with_attributed_error() {
        init_gst();
        let sdp = VIDEO_YCBCR_422_10BIT_1080P50_SDP.replace("YCbCr-4:2:2", "YCbCr-4:2:0");
        let err = parse_sdp(&sdp).expect_err("YCbCr-4:2:0 not yet supported");
        match err {
            SdpError::UnsupportedEssence(detail) => {
                assert!(
                    detail.contains("sampling=YCbCr-4:2:0"),
                    "error message should mention sampling=YCbCr-4:2:0: {detail}"
                );
            }
            other => panic!("expected UnsupportedEssence, got {other:?}"),
        }
    }

    #[test]
    fn extract_source_ip_from_filter_include_mode_single_source() {
        assert_eq!(
            extract_source_ip_from_filter("incl IN IP4 239.1.1.1 192.0.2.20"),
            Some("192.0.2.20".to_owned()),
        );
    }

    #[test]
    fn extract_source_ip_from_filter_exclude_mode_returns_none() {
        assert_eq!(
            extract_source_ip_from_filter("excl IN IP4 239.1.1.1 192.0.2.99"),
            None,
        );
    }

    #[test]
    fn extract_source_ip_from_filter_multi_source_returns_none() {
        assert_eq!(
            extract_source_ip_from_filter(
                "incl IN IP4 239.1.1.1 192.0.2.20 192.0.2.21"
            ),
            None,
        );
    }

    #[test]
    fn extract_source_ip_from_filter_malformed_returns_none() {
        assert_eq!(extract_source_ip_from_filter("garbage"), None);
        assert_eq!(extract_source_ip_from_filter(""), None);
        assert_eq!(extract_source_ip_from_filter("incl IN IP4 239.1.1.1"), None);
    }

    /// Default session-level descriptors for round-trip tests —
    /// concrete strings so the produced SDP is deterministic.
    fn test_session() -> SdpSession<'static> {
        SdpSession {
            origin_address: "192.0.2.10",
            origin_session_id: "1234567890",
            session_name: "test session",
            description: None,
            name: None,
            advertise_caps: false,
        }
    }

    #[test]
    fn build_sdp_video_uyvp_round_trip() {
        init_gst();
        let original = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse original");
        let text = build_sdp(&original, test_session()).expect("build");
        let round_tripped = parse_sdp(&text).expect("parse round-tripped");

        assert_eq!(round_tripped.format, original.format);
        assert_eq!(
            round_tripped.primary.destination_ip,
            original.primary.destination_ip,
        );
        assert_eq!(
            round_tripped.primary.destination_port,
            original.primary.destination_port,
        );
        assert_eq!(round_tripped.primary.interface_ip, original.primary.interface_ip);
        assert_eq!(round_tripped.primary.source_ip, original.primary.source_ip);
        assert_eq!(round_tripped.primary.source_port, original.primary.source_port);

        let orig_raw = original.raw_caps.structure(0).unwrap();
        let rt_raw = round_tripped.raw_caps.structure(0).unwrap();
        assert_eq!(rt_raw.get::<&str>("format"), orig_raw.get::<&str>("format"));
        assert_eq!(rt_raw.get::<i32>("width"), orig_raw.get::<i32>("width"));
        assert_eq!(rt_raw.get::<i32>("height"), orig_raw.get::<i32>("height"));
        assert_eq!(
            rt_raw.get::<gst::Fraction>("framerate"),
            orig_raw.get::<gst::Fraction>("framerate"),
        );
        assert_eq!(
            rt_raw.get::<&str>("interlace-mode"),
            orig_raw.get::<&str>("interlace-mode"),
        );
    }

    #[test]
    fn build_sdp_audio_l24_round_trip() {
        init_gst();
        let original = parse_sdp(AUDIO_L24_48K_STEREO_SDP).expect("parse original");
        let text = build_sdp(&original, test_session()).expect("build");
        let round_tripped = parse_sdp(&text).expect("parse round-tripped");

        assert_eq!(round_tripped.format, original.format);
        assert_eq!(
            round_tripped.primary.destination_ip,
            original.primary.destination_ip,
        );
        assert_eq!(
            round_tripped.primary.destination_port,
            original.primary.destination_port,
        );
        assert_eq!(round_tripped.primary.interface_ip, original.primary.interface_ip);
        assert_eq!(round_tripped.primary.source_ip, original.primary.source_ip);
        assert_eq!(round_tripped.primary.source_port, original.primary.source_port);

        let orig_raw = original.raw_caps.structure(0).unwrap();
        let rt_raw = round_tripped.raw_caps.structure(0).unwrap();
        assert_eq!(rt_raw.get::<&str>("format"), orig_raw.get::<&str>("format"));
        assert_eq!(rt_raw.get::<i32>("rate"), orig_raw.get::<i32>("rate"));
        assert_eq!(rt_raw.get::<i32>("channels"), orig_raw.get::<i32>("channels"));
    }

    #[test]
    fn build_sdp_includes_session_level_descriptors() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let text = build_sdp(&media, test_session()).expect("build");
        assert!(
            text.contains("o=nvnmos 1234567890 0 IN IP4 192.0.2.10"),
            "origin line missing: {text}",
        );
        assert!(
            text.contains("s=test session"),
            "session-name line missing: {text}",
        );
        assert!(text.starts_with("v=0\r\n"), "version line missing: {text}");
    }

    #[test]
    fn build_sdp_omits_source_filter_when_source_ip_absent() {
        init_gst();
        let stripped = VIDEO_YCBCR_422_10BIT_1080P50_SDP.replace(
            "a=source-filter: incl IN IP4 239.1.1.1 192.0.2.20\r\n",
            "",
        );
        let media = parse_sdp(&stripped).expect("parse");
        assert!(media.primary.source_ip.is_none());
        let text = build_sdp(&media, test_session()).expect("build");
        assert!(
            !text.contains("source-filter"),
            "built SDP must not include source-filter when source_ip is None: {text}",
        );
    }

    #[test]
    fn build_sdp_emits_source_filter_when_source_ip_present() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        assert_eq!(media.primary.source_ip.as_deref(), Some("192.0.2.20"));
        let text = build_sdp(&media, test_session()).expect("build");
        assert!(
            text.contains("a=source-filter: incl IN IP4 239.1.1.1 192.0.2.20"),
            "source-filter line missing or malformed: {text}",
        );
    }

    #[test]
    fn build_sdp_emits_x_nvnmos_attributes_when_present() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let text = build_sdp(&media, test_session()).expect("build");
        assert!(
            text.contains("a=x-nvnmos-iface-ip:192.0.2.11"),
            "x-nvnmos-iface-ip line missing: {text}",
        );
        assert!(
            text.contains("a=x-nvnmos-src-port:5005"),
            "x-nvnmos-src-port line missing: {text}",
        );
    }

    #[test]
    #[cfg(unix)]
    fn build_sdp_emits_x_nvnmos_iface_when_interface_ip_resolves_locally() {
        init_gst();
        let Some(ip) = crate::iface::test_first_non_loopback_ipv4() else {
            return;
        };
        let ip_str = ip.to_string();
        let mut media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        media.primary.interface_ip = Some(ip_str.clone());
        let text = build_sdp(&media, test_session()).expect("build");
        let expected = crate::iface::x_nvnmos_iface_value_for_ip(&ip_str)
            .expect("host IP must yield x-nvnmos-iface");
        assert!(
            text.contains(&format!("a=x-nvnmos-iface-ip:{ip_str}")),
            "iface-ip: {text}",
        );
        assert!(
            text.contains(&format!("a=x-nvnmos-iface:{expected}")),
            "x-nvnmos-iface missing: {text}",
        );
    }

    #[test]
    fn build_sdp_omits_x_nvnmos_attributes_when_absent() {
        init_gst();
        let stripped = VIDEO_YCBCR_422_10BIT_1080P50_SDP
            .replace("a=x-nvnmos-iface-ip:192.0.2.11\r\n", "")
            .replace("a=x-nvnmos-src-port:5005\r\n", "");
        let media = parse_sdp(&stripped).expect("parse");
        assert!(media.primary.interface_ip.is_none());
        assert!(media.primary.source_port.is_none());
        let text = build_sdp(&media, test_session()).expect("build");
        assert!(
            !text.contains("x-nvnmos-iface-ip"),
            "built SDP must not include x-nvnmos-iface-ip when interface_ip is None: {text}",
        );
        assert!(
            !text.contains("x-nvnmos-src-port"),
            "built SDP must not include x-nvnmos-src-port when source_port is None: {text}",
        );
    }

    #[test]
    fn build_sdp_emits_i_line_when_description_set() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let mut session = test_session();
        session.description = Some("Test camera feed");
        let text = build_sdp(&media, session).expect("build");
        assert!(
            text.contains("\r\ni=Test camera feed\r\n"),
            "i= line missing when description is Some: {text}",
        );
    }

    #[test]
    fn build_sdp_omits_i_line_when_description_unset() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let text = build_sdp(&media, test_session()).expect("build");
        assert!(
            !text.contains("\r\ni="),
            "i= line must not appear when description is None: {text}",
        );
    }

    #[test]
    fn build_sdp_emits_x_nvnmos_name_when_set() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let mut session = test_session();
        session.name = Some("Camera 1");
        let text = build_sdp(&media, session).expect("build");
        assert!(
            text.contains("a=x-nvnmos-name:Camera 1"),
            "a=x-nvnmos-name line missing when session.name is Some: {text}",
        );
    }

    #[test]
    fn build_sdp_omits_x_nvnmos_name_when_unset() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let text = build_sdp(&media, test_session()).expect("build");
        assert!(
            !text.contains("x-nvnmos-name"),
            "a=x-nvnmos-name line must not appear when session.name is None: {text}",
        );
    }

    // Property-override passthrough tests. Each test pins one
    // override slot at a time, then a final "all-None preserves
    // input" + "rejects malformed" complete the matrix.

    #[test]
    fn reject_unsupported_multi_media_accepts_single_block() {
        init_gst();
        let msg = SDPMessage::parse_buffer(VIDEO_YCBCR_422_10BIT_1080P50_SDP.as_bytes())
            .expect("parse");
        reject_unsupported_multi_media(&msg).expect("single media ok");
    }

    #[test]
    fn reject_unsupported_multi_media_rejects_video_plus_audio() {
        init_gst();
        let sdp = format!(
            "{}{}",
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            "m=audio 5004 RTP/AVP 97\r\n\
             c=IN IP4 239.2.2.2/64\r\n\
             a=rtpmap:97 L24/48000/2\r\n",
        );
        let msg = SDPMessage::parse_buffer(sdp.as_bytes()).expect("parse");
        let err = reject_unsupported_multi_media(&msg).expect_err("mixed essence");
        assert!(matches!(err, SdpError::MultiMediaMixedEssence));
    }

    #[test]
    fn reject_unsupported_multi_media_rejects_three_blocks() {
        init_gst();
        let sdp = format!(
            "{}{}",
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            "m=video 5006 RTP/AVP 96\r\n\
             c=IN IP4 239.1.1.2/64\r\n\
             a=rtpmap:96 raw/90000\r\n\
             m=video 5008 RTP/AVP 96\r\n\
             c=IN IP4 239.1.1.3/64\r\n\
             a=rtpmap:96 raw/90000\r\n",
        );
        let msg = SDPMessage::parse_buffer(sdp.as_bytes()).expect("parse");
        let err = reject_unsupported_multi_media(&msg).expect_err("too many");
        assert!(matches!(err, SdpError::TooManyMediaBlocks(3)));
    }

    #[test]
    fn reject_unsupported_multi_media_rejects_two_same_type_blocks() {
        init_gst();
        let sdp = format!(
            "{}{}",
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            "m=video 5006 RTP/AVP 96\r\n\
             c=IN IP4 239.1.1.2/64\r\n\
             a=rtpmap:96 raw/90000\r\n",
        );
        let msg = SDPMessage::parse_buffer(sdp.as_bytes()).expect("parse");
        let err = reject_unsupported_multi_media(&msg).expect_err("dual leg");
        assert!(matches!(err, SdpError::MultipleMedia(2)));
    }

    #[test]
    fn passthrough_with_no_overrides_is_byte_identical_for_video() {
        init_gst();
        let out = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            &SdpOverrides::default(),
        )
        .expect("passthrough");
        assert_eq!(out, VIDEO_YCBCR_422_10BIT_1080P50_SDP);
        assert!(out.contains("a=ts-refclk:"));
        assert!(out.contains("a=mediaclk:"));
    }

    #[test]
    fn passthrough_preserves_unknown_fmtp_keys() {
        init_gst();
        let sdp = VIDEO_YCBCR_422_10BIT_1080P50_SDP.replace(
            "PM=2110GPM;",
            "PM=2110GPM; SomeFutureKey=Value;",
        );
        let out =
            passthrough_with_overrides(&sdp, &SdpOverrides::default()).expect("passthrough");
        assert!(
            out.contains("SomeFutureKey=Value"),
            "vendor/future fmtp keys must survive passthrough:\n{out}",
        );
    }

    #[test]
    fn passthrough_preserves_information_line() {
        init_gst();
        let sdp = VIDEO_YCBCR_422_10BIT_1080P50_SDP.replace("s=Example\r\n", "s=Example\r\ni=Studio A\r\n");
        let out =
            passthrough_with_overrides(&sdp, &SdpOverrides::default()).expect("passthrough");
        assert!(out.contains("i=Studio A"));
    }

    #[test]
    fn passthrough_label_replaces_session_name() {
        init_gst();
        let spliced = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            &SdpOverrides { label: Some("new label"), ..Default::default() },
        )
        .expect("splice");
        let msg = SDPMessage::parse_buffer(spliced.as_bytes()).expect("re-parse");
        assert_eq!(msg.session_name().unwrap(), "new label");
    }

    #[test]
    fn passthrough_description_replaces_i_line() {
        init_gst();
        // The fixture has no `i=` line; splice should add one.
        let spliced = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            &SdpOverrides {
                description: Some("Studio A camera"),
                ..Default::default()
            },
        )
        .expect("splice");
        let msg = SDPMessage::parse_buffer(spliced.as_bytes()).expect("re-parse");
        assert_eq!(msg.information().unwrap(), "Studio A camera");
    }

    #[test]
    fn passthrough_name_replaces_x_nvnmos_name() {
        init_gst();
        // The fixture has no `a=x-nvnmos-name`; splice should add one
        // *at the session level* (per `nvnmos.h` and what
        // libnvnmos's parser reads). Asserts both:
        //   1. `msg.attribute_val("x-nvnmos-name")` (session-level
        //      query) returns the spliced value.
        //   2. The line appears before the first `m=` in the
        //      serialised text — guards against the placement
        //      regression that earlier emitted it at media level.
        let spliced = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            &SdpOverrides { name: Some("Camera 1"), ..Default::default() },
        )
        .expect("splice");
        let msg = SDPMessage::parse_buffer(spliced.as_bytes()).expect("re-parse");
        assert_eq!(
            msg.attribute_val("x-nvnmos-name"),
            Some("Camera 1"),
            "x-nvnmos-name must be readable at session level: {spliced}",
        );
        let name_pos = spliced
            .find("a=x-nvnmos-name:")
            .expect("a=x-nvnmos-name: line must be present");
        let media_pos = spliced
            .find("\r\nm=")
            .expect("m= line must be present");
        assert!(
            name_pos < media_pos,
            "a=x-nvnmos-name (offset {name_pos}) must appear before first m= (offset {media_pos}); \
             session-level attributes precede the first media section per RFC 4566 §5: {spliced}",
        );
    }

    #[test]
    fn passthrough_leg_fields_replace_udp_media() {
        init_gst();
        let spliced = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            &SdpOverrides {
                destination_ip: Some("239.99.99.99"),
                destination_port: Some(5555),
                interface_ip: Some("192.0.2.99"),
                source_ip: Some("192.0.2.30"),
                source_port: Some(5556),
                ..Default::default()
            },
        )
        .expect("splice");
        // Re-parse via the canonical `parse_sdp` path rather than
        // raw `SDPMessage` to assert all five overrides land where
        // the rest of the crate reads them from.
        let media = parse_sdp(&spliced).expect("re-parse");
        assert_eq!(media.primary.destination_ip, "239.99.99.99");
        assert_eq!(media.primary.destination_port, 5555);
        assert_eq!(media.primary.interface_ip.as_deref(), Some("192.0.2.99"));
        assert_eq!(media.primary.source_ip.as_deref(), Some("192.0.2.30"));
        assert_eq!(media.primary.source_port, Some(5556));
    }

    #[test]
    fn passthrough_all_none_preserves_input_legs() {
        init_gst();
        let original = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let spliced =
            passthrough_with_overrides(VIDEO_YCBCR_422_10BIT_1080P50_SDP, &SdpOverrides::default())
                .expect("splice");
        let after = parse_sdp(&spliced).expect("re-parse");
        // Leg fields must round-trip unchanged.
        assert_eq!(after.primary.destination_ip, original.primary.destination_ip);
        assert_eq!(
            after.primary.destination_port,
            original.primary.destination_port
        );
        assert_eq!(after.primary.interface_ip, original.primary.interface_ip);
        assert_eq!(after.primary.source_ip, original.primary.source_ip);
        assert_eq!(after.primary.source_port, original.primary.source_port);
        // Session name preserved (input had `s=Example`).
        let msg = SDPMessage::parse_buffer(spliced.as_bytes()).expect("re-parse msg");
        assert_eq!(msg.session_name().unwrap(), "Example");
    }

    #[test]
    fn passthrough_preserves_origin_address_and_session_id() {
        init_gst();
        // The fixture sets `o=- 1234567890 0 IN IP4 192.0.2.10`.
        // With no `label` / `description` / `name` override and no
        // leg overrides, the splice must keep the same `o=`
        // address and session-id (the daemon may rely on these
        // being stable across the configuring-SDP lifecycle).
        let spliced =
            passthrough_with_overrides(VIDEO_YCBCR_422_10BIT_1080P50_SDP, &SdpOverrides::default())
                .expect("splice");
        let msg = SDPMessage::parse_buffer(spliced.as_bytes()).expect("re-parse");
        let origin = msg.origin().expect("origin");
        assert_eq!(origin.addr().unwrap(), "192.0.2.10");
        assert_eq!(origin.sess_id().unwrap(), "1234567890");
    }

    #[test]
    fn passthrough_invalid_input_propagates_parse_error() {
        init_gst();
        let err = passthrough_with_overrides("not an SDP at all", &SdpOverrides::default())
            .expect_err("must error");
        assert!(matches!(err, SdpError::Parse(_) | SdpError::NoMedia));
    }

    // -- transport-caps overrides --------------------------------

    /// Audio pt override: rewrite `m=audio … 97` → `… 99`,
    /// `a=rtpmap:97 …` → `a=rtpmap:99 …`. The single mutation
    /// to `rtp_caps.payload` flows through to all three slots
    /// via `set_media_from_caps`.
    #[test]
    fn passthrough_audio_pt_override_rewrites_m_line_and_rtpmap() {
        init_gst();
        let spliced = passthrough_with_overrides(
            AUDIO_L24_48K_STEREO_SDP,
            &SdpOverrides {
                payload_type: Some(99),
                ..Default::default()
            },
        )
        .expect("splice");
        assert!(spliced.contains("m=audio 5004 RTP/AVP 99"),
            "m= must carry new pt 99; got: {spliced}");
        assert!(spliced.contains("a=rtpmap:99 L24/48000/2"),
            "a=rtpmap must carry new pt 99; got: {spliced}");
        assert!(spliced.contains("a=fmtp:99"),
            "a=fmtp must be re-keyed to new pt 99; got: {spliced}");
        // Re-parse to confirm the model round-trips.
        let m = parse_sdp(&spliced).expect("re-parse");
        let pt = m.rtp_caps.structure(0).and_then(|s| s.get::<i32>("payload").ok());
        assert_eq!(pt, Some(99));
    }

    /// Audio clock-rate override: 48000 → 96000 rewrites the
    /// `a=rtpmap:97 L24/48000/2` clock-rate token.
    #[test]
    fn passthrough_audio_clock_rate_override_rewrites_rtpmap() {
        init_gst();
        let spliced = passthrough_with_overrides(
            AUDIO_L24_48K_STEREO_SDP,
            &SdpOverrides {
                audio_clock_rate: Some(96000),
                ..Default::default()
            },
        )
        .expect("splice");
        assert!(spliced.contains("a=rtpmap:97 L24/96000/2"),
            "a=rtpmap clock-rate must be 96000; got: {spliced}");
        assert!(!spliced.contains("L24/48000/"),
            "old 48000 must be gone; got: {spliced}");
    }

    /// Audio ptime + maxptime override: GStreamer convention
    /// uses string values on the `a-ptime` / `a-maxptime` caps
    /// fields (ms decimal), `set_media_from_caps` re-emits as
    /// `a=ptime:` / `a=maxptime:` lines.
    #[test]
    fn passthrough_audio_ptime_and_maxptime_override() {
        init_gst();
        let spliced = passthrough_with_overrides(
            AUDIO_L24_48K_STEREO_SDP,
            &SdpOverrides {
                a_ptime: Some("1"),
                a_maxptime: Some("2"),
                ..Default::default()
            },
        )
        .expect("splice");
        assert!(spliced.contains("a=ptime:1\r\n"),
            "a=ptime must be 1ms; got: {spliced}");
        assert!(spliced.contains("a=maxptime:2\r\n"),
            "a=maxptime must be 2ms; got: {spliced}");
        assert!(!spliced.contains("a=ptime:0.125"),
            "old 0.125 ms ptime must be gone; got: {spliced}");
    }

    /// Audio-only slots silently no-op for video raw essence
    /// (fixed 90 kHz clock-rate per RFC 4175, no packet-time
    /// metadata). The video SDP must round-trip with its
    /// 90000 / no-ptime shape intact even when the caller
    /// passes `audio_clock_rate` / `a_ptime` /
    /// `a_maxptime`.
    #[test]
    fn passthrough_audio_only_slots_silently_ignored_for_video() {
        init_gst();
        let spliced = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            &SdpOverrides {
                audio_clock_rate: Some(48000),
                a_ptime: Some("0.125"),
                a_maxptime: Some("0.125"),
                ..Default::default()
            },
        )
        .expect("splice");
        // `caps_from_media` upper-cases the rtpmap
        // encoding-name; match both forms.
        assert!(
            spliced.contains("a=rtpmap:96 RAW/90000")
                || spliced.contains("a=rtpmap:96 raw/90000"),
            "video clock-rate must stay 90000 (audio_clock_rate ignored); got: {spliced}",
        );
        assert!(!spliced.contains("a=ptime:"),
            "video must have no a=ptime (audio-only slot ignored); got: {spliced}");
    }

    /// Video pt override: `m=video … 96` → `… 100`. Pt
    /// override is essence-agnostic (RFC 3551 §6 dynamic
    /// range applies to all RTP transports).
    #[test]
    fn passthrough_video_pt_override_works() {
        init_gst();
        let spliced = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            &SdpOverrides {
                payload_type: Some(100),
                ..Default::default()
            },
        )
        .expect("splice");
        assert!(spliced.contains("m=video 5004 RTP/AVP 100"),
            "m= must carry new pt 100; got: {spliced}");
        // `caps_from_media` upper-cases the rtpmap
        // encoding-name via `g_ascii_strup`; the round-trip
        // through caps therefore emits `RAW`, not `raw`. Match
        // both for resilience to future GStreamer changes.
        assert!(
            spliced.contains("a=rtpmap:100 RAW/90000")
                || spliced.contains("a=rtpmap:100 raw/90000"),
            "a=rtpmap must carry new pt 100; got: {spliced}"
        );
    }

    // -- cross_check_essence -------------------------------------

    /// No caps + no transport-caps → pass-through. Pins the
    /// "user supplied neither hint" baseline: cross-check
    /// can't reject what it wasn't asked to compare against.
    #[test]
    fn cross_check_essence_with_no_caps_is_noop() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        cross_check_essence(&media, None, None).expect("no caps → pass");
    }

    /// Matching essence caps (`video/x-raw` + 1920x1080) +
    /// matching transport caps (`application/x-rtp` +
    /// `encoding-name=RAW`) against a raw video SDP must pass.
    #[test]
    fn cross_check_essence_with_matching_caps_passes() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let essence = gst::Caps::builder("video/x-raw")
            .field("width", 1920i32)
            .field("height", 1080i32)
            .build();
        let transport = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", "RAW")
            .field("clock-rate", 90_000i32)
            .build();
        cross_check_essence(&media, Some(&essence), Some(&transport))
            .expect("matching caps → pass");
    }

    /// Format-family mismatch (`audio/x-raw` declared on a
    /// video raw SDP) fires `SdpError::FormatMismatch` before
    /// the (necessarily empty) shape intersect runs, so the
    /// error message names the specific axis (`Audio` vs
    /// `Video`) rather than a generic shape blob.
    #[test]
    fn cross_check_essence_rejects_format_family_mismatch() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let audio_caps = gst::Caps::builder("audio/x-raw").build();
        let err = cross_check_essence(&media, Some(&audio_caps), None)
            .expect_err("format-family mismatch must error");
        match err {
            SdpError::FormatMismatch { caps, sdp } => {
                assert_eq!(caps, FlowFormat::Audio);
                assert_eq!(sdp, FlowFormat::Video);
            }
            other => panic!("expected FormatMismatch, got {other:?}"),
        }
    }

    /// Same family, conflicting essence-shape fields:
    /// `width=1280` vs SDP's `width=1920` → empty intersect →
    /// `SdpError::EssenceShapeMismatch`.
    #[test]
    fn cross_check_essence_rejects_shape_mismatch() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let caps = gst::Caps::builder("video/x-raw")
            .field("width", 1280i32)
            .field("height", 720i32)
            .build();
        let err = cross_check_essence(&media, Some(&caps), None)
            .expect_err("shape mismatch must error");
        assert!(matches!(err, SdpError::EssenceShapeMismatch { .. }),
            "got: {err:?}");
    }

    /// Transport-caps mismatch on a cross-check field
    /// (`encoding-name=L24` against a raw-video SDP).
    #[test]
    fn cross_check_essence_rejects_transport_caps_encoding_name_mismatch() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let transport = gst::Caps::builder("application/x-rtp")
            .field("encoding-name", "L24")
            .build();
        let err = cross_check_essence(&media, None, Some(&transport))
            .expect_err("encoding-name mismatch must error");
        assert!(matches!(err, SdpError::TransportCapsMismatch { .. }),
            "got: {err:?}");
    }

    /// Video clock-rate is cross-check (not override): a
    /// `transport-caps` claiming `clock-rate=48000` against a
    /// 90 kHz video SDP must error.
    #[test]
    fn cross_check_essence_rejects_video_clock_rate_mismatch() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let transport = gst::Caps::builder("application/x-rtp")
            .field("clock-rate", 48_000i32)
            .build();
        let err = cross_check_essence(&media, None, Some(&transport))
            .expect_err("video clock-rate mismatch must error");
        assert!(matches!(err, SdpError::TransportCapsMismatch { .. }),
            "got: {err:?}");
    }

    /// Always-override fields (`payload`, `a-ptime`,
    /// `a-maxptime`) are stripped from both sides before the
    /// intersect, so a disagreement on those fields alone
    /// does NOT trip the cross-check. The splice helper has
    /// already aligned them; the cross-check is for the
    /// genuinely-not-spliced fields.
    #[test]
    fn cross_check_essence_strips_override_fields_before_intersect() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let transport = gst::Caps::builder("application/x-rtp")
            // Different pt than the SDP's 96 — but pt is override
            // so the strip makes this disagreement invisible.
            .field("payload", 99i32)
            .field("a-ptime", "1")
            .field("a-maxptime", "1")
            // Matching cross-check fields:
            .field("encoding-name", "RAW")
            .field("clock-rate", 90_000i32)
            .build();
        cross_check_essence(&media, None, Some(&transport))
            .expect("override-only disagreement must pass");
    }

    /// Audio essence: the splice has already copied a user-
    /// supplied `clock-rate` override into `media.rtp_caps`,
    /// so the cross-check intersect on `clock-rate` matches
    /// implicitly. This pins the audio clock-rate override
    /// path end-to-end (splice → cross-check both pass).
    #[test]
    fn cross_check_essence_audio_clock_rate_override_passes_after_splice() {
        init_gst();
        let transport = gst::Caps::builder("application/x-rtp")
            .field("clock-rate", 96_000i32)
            .build();
        // 1. Splice the override into the audio SDP.
        let spliced = passthrough_with_overrides(
            AUDIO_L24_48K_STEREO_SDP,
            &SdpOverrides {
                audio_clock_rate: Some(96_000),
                ..Default::default()
            },
        )
        .expect("splice");
        let media = parse_sdp(&spliced).expect("re-parse");
        // 2. Cross-check matches because both now agree on
        //    clock-rate=96000.
        cross_check_essence(&media, None, Some(&transport))
            .expect("audio override + cross-check → pass");
    }

    /// Pt outside RFC 3551 §6 dynamic range (96..=127) is
    /// rejected loudly rather than silently dropped, because
    /// `transport-caps` is user-facing and a stale pt is a
    /// misconfiguration worth surfacing. Tests the dynamic-
    /// range lower (0, 95) and upper (128 via u8 max 255) edges.
    #[test]
    fn passthrough_invalid_pt_returns_error() {
        init_gst();
        for pt in [0u8, 33, 95, 128, 200, 255] {
            let err = passthrough_with_overrides(
                AUDIO_L24_48K_STEREO_SDP,
                &SdpOverrides {
                    payload_type: Some(pt),
                    ..Default::default()
                },
            )
            .expect_err(&format!("pt={pt} must be rejected"));
            match err {
                SdpError::InvalidPayloadType(p) => {
                    assert_eq!(p, u32::from(pt), "error must echo the offending pt");
                }
                other => panic!("expected InvalidPayloadType({pt}), got {other:?}"),
            }
        }
        // Sanity: 96 and 127 (the inclusive bounds) succeed.
        for pt in [96, 127] {
            passthrough_with_overrides(
                AUDIO_L24_48K_STEREO_SDP,
                &SdpOverrides {
                    payload_type: Some(pt),
                    ..Default::default()
                },
            )
            .unwrap_or_else(|e| panic!("pt={pt} must be valid: {e}"));
        }
    }

    /// Variant of `VIDEO_YCBCR_422_10BIT_1080P50_SDP` with an
    /// `a=x-nvnmos-caps:96` media-level attribute (canonical
    /// form per `nvnmos_impl.cpp:1727-1731`: pt-only, no
    /// constraints = fully flexible). Used by the `caps_mode`
    /// splice tests below to exercise the "input already
    /// carries the attribute" path. libnvnmos's parser is
    /// presence-only so the value doesn't matter for parsing,
    /// but this is the form [`build_sdp`] writes.
    const VIDEO_YCBCR_422_10BIT_1080P50_WIDE_SDP: &str = concat!(
        "v=0\r\n",
        "o=- 1234567890 0 IN IP4 192.0.2.10\r\n",
        "s=Example\r\n",
        "t=0 0\r\n",
        "m=video 5004 RTP/AVP 96\r\n",
        "c=IN IP4 239.1.1.1/64\r\n",
        "a=source-filter: incl IN IP4 239.1.1.1 192.0.2.20\r\n",
        "a=rtpmap:96 raw/90000\r\n",
        "a=fmtp:96 sampling=YCbCr-4:2:2; width=1920; height=1080; \
         exactframerate=50; depth=10; TCS=SDR; colorimetry=BT709; \
         PM=2110GPM; SSN=ST2110-20:2017; TP=2110TPN\r\n",
        "a=mediaclk:direct=0\r\n",
        "a=ts-refclk:ptp=IEEE1588-2008:traceable\r\n",
        "a=x-nvnmos-iface-ip:192.0.2.11\r\n",
        "a=x-nvnmos-src-port:5005\r\n",
        "a=x-nvnmos-caps:96\r\n",
    );

    /// Round-trip check: does the serialised SDP carry an
    /// `a=x-nvnmos-caps` attribute at media level? Uses
    /// `SDPMedia::attribute_val` so it works with both
    /// `a=x-nvnmos-caps` (flag form) and `a=x-nvnmos-caps:`
    /// (property form with empty value) — both parse to
    /// `Some("")` and both signal wide to libnvnmos.
    fn has_x_nvnmos_caps(sdp: &str) -> bool {
        let msg = SDPMessage::parse_buffer(sdp.as_bytes()).expect("parse");
        msg.media(0)
            .and_then(|m| m.attribute_val("x-nvnmos-caps").map(|_| ()))
            .is_some()
    }

    // `caps_mode` is the SDP-side override slot mirroring
    // `flow_def::FlowDefOverrides::caps_mode`. Each test pins
    // one (input-state, CapsMode) cell of the 2×3 matrix:
    //
    //               | Auto    | Narrow      | Wide
    //   ------------+---------+-------------+-------------
    //   absent      | absent  | absent      | present
    //   present     | present | absent      | present
    //
    // `build_sdp_*` tests pin the [`SdpSession::advertise_caps`]
    // boolean directly (no splice resolution involved).

    #[test]
    fn build_sdp_emits_x_nvnmos_caps_when_advertise_caps_true() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let mut session = test_session();
        session.advertise_caps = true;
        let text = build_sdp(&media, session).expect("build");
        assert!(
            has_x_nvnmos_caps(&text),
            "advertise_caps=true must emit media-level `a=x-nvnmos-caps`: {text}",
        );
        // Canonical wire form per `nvnmos_impl.cpp:1727-1731`:
        // pt-only (no constraints) means fully flexible. The
        // fixture's `m=video … 96` line drives pt=96, so the
        // serialised value must be exactly `96`.
        assert!(
            text.contains("\r\na=x-nvnmos-caps:96\r\n"),
            "advertise_caps must emit canonical `a=x-nvnmos-caps:<pt>` form: {text}",
        );
        let attr_pos = text
            .find("\r\na=x-nvnmos-caps")
            .expect("x-nvnmos-caps line present");
        let media_pos = text.find("\r\nm=").expect("m= line present");
        assert!(
            attr_pos > media_pos,
            "a=x-nvnmos-caps must be media-level (after first m=): attr_pos={attr_pos} \
             media_pos={media_pos}: {text}",
        );
    }

    #[test]
    fn build_sdp_omits_x_nvnmos_caps_when_advertise_caps_false() {
        init_gst();
        let media = parse_sdp(VIDEO_YCBCR_422_10BIT_1080P50_SDP).expect("parse");
        let text = build_sdp(&media, test_session()).expect("build");
        assert!(
            !has_x_nvnmos_caps(&text),
            "advertise_caps=false (test_session default) must omit `a=x-nvnmos-caps`: {text}",
        );
    }

    #[test]
    fn passthrough_caps_mode_auto_preserves_absent() {
        init_gst();
        let spliced = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            &SdpOverrides { caps_mode: CapsMode::Auto, ..Default::default() },
        )
        .expect("splice");
        assert!(
            !has_x_nvnmos_caps(&spliced),
            "input had no a=x-nvnmos-caps; Auto must leave it absent: {spliced}",
        );
    }

    #[test]
    fn passthrough_caps_mode_auto_preserves_present() {
        init_gst();
        let spliced = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_WIDE_SDP,
            &SdpOverrides { caps_mode: CapsMode::Auto, ..Default::default() },
        )
        .expect("splice");
        assert!(
            has_x_nvnmos_caps(&spliced),
            "input had a=x-nvnmos-caps; Auto must preserve it: {spliced}",
        );
    }

    #[test]
    fn passthrough_caps_mode_narrow_strips_attribute() {
        init_gst();
        let spliced = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_WIDE_SDP,
            &SdpOverrides { caps_mode: CapsMode::Narrow, ..Default::default() },
        )
        .expect("splice");
        assert!(
            !has_x_nvnmos_caps(&spliced),
            "Narrow must strip a=x-nvnmos-caps from a wide input: {spliced}",
        );
    }

    #[test]
    fn passthrough_caps_mode_narrow_no_op_when_absent() {
        init_gst();
        let spliced = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            &SdpOverrides { caps_mode: CapsMode::Narrow, ..Default::default() },
        )
        .expect("splice");
        assert!(
            !has_x_nvnmos_caps(&spliced),
            "Narrow on an already-narrow input must remain narrow: {spliced}",
        );
    }

    #[test]
    fn passthrough_caps_mode_wide_adds_attribute() {
        init_gst();
        let spliced = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_SDP,
            &SdpOverrides { caps_mode: CapsMode::Wide, ..Default::default() },
        )
        .expect("splice");
        assert!(
            has_x_nvnmos_caps(&spliced),
            "Wide must add a=x-nvnmos-caps to a narrow input: {spliced}",
        );
        // Canonical form: pt of the (single) media. Fixture pt
        // is 96.
        assert!(
            spliced.contains("\r\na=x-nvnmos-caps:96\r\n"),
            "Wide must emit canonical `a=x-nvnmos-caps:<pt>` form: {spliced}",
        );
    }

    #[test]
    fn passthrough_caps_mode_wide_idempotent_when_present() {
        init_gst();
        let spliced = passthrough_with_overrides(
            VIDEO_YCBCR_422_10BIT_1080P50_WIDE_SDP,
            &SdpOverrides { caps_mode: CapsMode::Wide, ..Default::default() },
        )
        .expect("splice");
        assert!(has_x_nvnmos_caps(&spliced));
        // Re-emission collapses to the canonical `<pt>` form
        // regardless of the input value (libnvnmos's parser is
        // presence-only; we don't try to preserve constraint
        // suffixes since `SdpSession.advertise_caps` is just a
        // bool).
        assert!(
            spliced.contains("\r\na=x-nvnmos-caps:96\r\n"),
            "Wide re-emit must normalise to canonical `<pt>` form: {spliced}",
        );
        // Exactly one `a=x-nvnmos-caps` line — guard against
        // double-emission if a future change accidentally
        // re-adds it on top of the parsed input.
        let occurrences = spliced.matches("a=x-nvnmos-caps").count();
        assert_eq!(
            occurrences, 1,
            "Wide on a present input must remain idempotent (single line): {spliced}",
        );
    }

    /// 1920×1080p60 SMPTE ST 2110-40 ANC SDP. The `m=video` line
    /// is correct per RFC 8331 §3 (ANC RTP rides on the video
    /// media type, not `data` / `application`); `encoding-name=
    /// SMPTE291` and `exactframerate` in the `a=fmtp:` line are
    /// the only essence-shape signals on the wire. `VPID_Code` is
    /// an optional ST 2110-40 fmtp parameter and is included here
    /// to cover the `caps_from_media` round-trip.
    const ANC_SMPTE291_1080P60_SDP: &str = concat!(
        "v=0\r\n",
        "o=- 1234567890 0 IN IP4 192.0.2.10\r\n",
        "s=Example ANC\r\n",
        "t=0 0\r\n",
        "m=video 5006 RTP/AVP 100\r\n",
        "c=IN IP4 239.1.1.10/64\r\n",
        "a=source-filter: incl IN IP4 239.1.1.10 192.0.2.20\r\n",
        "a=rtpmap:100 smpte291/90000\r\n",
        "a=fmtp:100 exactframerate=60; VPID_Code=132\r\n",
        "a=mediaclk:direct=0\r\n",
        "a=ts-refclk:ptp=IEEE1588-2008:traceable\r\n",
        "a=x-nvnmos-iface-ip:192.0.2.11\r\n",
        "a=x-nvnmos-src-port:5007\r\n",
    );

    #[test]
    fn anc_smpte291_happy_path() {
        init_gst();
        let media = parse_sdp(ANC_SMPTE291_1080P60_SDP).expect("parse");
        assert_eq!(
            media.format,
            FlowFormat::Data,
            "RFC 8331 SMPTE 291 ANC must map to FlowFormat::Data even though \
             the SDP `m=` line says `video` (RFC 8331 §3 explicitly carries \
             ANC under the video media type and only `encoding-name=SMPTE291` \
             distinguishes it from RFC 4175)",
        );
        assert_eq!(media.primary.destination_ip, "239.1.1.10");
        assert_eq!(media.primary.destination_port, 5006);
        assert_eq!(media.primary.interface_ip.as_deref(), Some("192.0.2.11"));
        assert_eq!(media.primary.source_ip.as_deref(), Some("192.0.2.20"));
        assert_eq!(media.primary.source_port, Some(5007));

        let rtp_s = media.rtp_caps.structure(0).expect("rtp caps");
        assert_eq!(rtp_s.name().as_str(), "application/x-rtp");
        assert_eq!(rtp_s.get::<&str>("media").unwrap(), "video");
        assert_eq!(rtp_s.get::<&str>("encoding-name").unwrap(), "SMPTE291");
        assert_eq!(rtp_s.get::<i32>("clock-rate").unwrap(), 90_000);
        assert_eq!(rtp_s.get::<i32>("payload").unwrap(), 100);

        let raw_s = media.raw_caps.structure(0).expect("raw caps");
        assert_eq!(
            raw_s.name().as_str(),
            "meta/x-st-2038",
            "ANC raw caps must match the `rtpsmpte291*` element's pad template \
             and the existing `flow_def::build_data_body` convention",
        );
        assert_eq!(
            raw_s.get::<&str>("alignment").unwrap(),
            "frame",
            "`alignment=frame` matches `rtpsmpte291pay`'s sink pad template",
        );
        assert_eq!(
            raw_s.get::<gst::Fraction>("framerate").unwrap(),
            gst::Fraction::new(60, 1),
            "`exactframerate=60` from a=fmtp: must surface as framerate=60/1",
        );
    }

    #[test]
    fn anc_smpte291_fractional_exactframerate() {
        init_gst();
        let sdp = ANC_SMPTE291_1080P60_SDP.replace("exactframerate=60;", "exactframerate=30000/1001;");
        let media = parse_sdp(&sdp).expect("parse");
        let raw_s = media.raw_caps.structure(0).expect("raw caps");
        assert_eq!(
            raw_s.get::<gst::Fraction>("framerate").unwrap(),
            gst::Fraction::new(30_000, 1_001),
            "fractional `exactframerate` (NTSC-like rates) must round-trip",
        );
    }

    #[test]
    fn anc_smpte291_without_exactframerate_omits_framerate() {
        init_gst();
        // Drop the fmtp line entirely — ANC SDPs in the wild often
        // don't carry it because the rate is implicit from the
        // paired video flow.
        let sdp = ANC_SMPTE291_1080P60_SDP
            .replace("a=fmtp:100 exactframerate=60; VPID_Code=132\r\n", "");
        let media = parse_sdp(&sdp).expect("parse");
        let raw_s = media.raw_caps.structure(0).expect("raw caps");
        assert!(
            raw_s.get::<gst::Fraction>("framerate").is_err(),
            "framerate must be absent on raw_caps when SDP carries no \
             exactframerate; downstream caps-merge (element property / \
             paired-flow context) fills it in",
        );
        assert_eq!(
            raw_s.get::<&str>("alignment").unwrap(),
            "frame",
            "alignment=frame must still be present without exactframerate",
        );
    }

    #[test]
    fn build_sdp_anc_round_trip() {
        init_gst();
        let original = parse_sdp(ANC_SMPTE291_1080P60_SDP).expect("parse original");
        let text = build_sdp(&original, test_session()).expect("build");
        // The rebuilt SDP must round-trip through parse_sdp back to
        // an equivalent UdpMedia. `set_media_from_caps` consumes the
        // RTP caps and produces `a=rtpmap:` + `a=fmtp:` lines, so
        // `encoding-name=SMPTE291` and `exactframerate` survive
        // intact even though build_sdp doesn't know anything about
        // ANC specifically.
        let round_tripped = parse_sdp(&text).expect("parse round-tripped");
        assert_eq!(round_tripped.format, FlowFormat::Data);
        assert_eq!(
            round_tripped.primary.destination_ip,
            original.primary.destination_ip,
        );
        let orig_raw = original.raw_caps.structure(0).unwrap();
        let rt_raw = round_tripped.raw_caps.structure(0).unwrap();
        assert_eq!(rt_raw.name(), orig_raw.name());
        assert_eq!(
            rt_raw.get::<&str>("alignment"),
            orig_raw.get::<&str>("alignment"),
        );
        assert_eq!(
            rt_raw.get::<gst::Fraction>("framerate"),
            orig_raw.get::<gst::Fraction>("framerate"),
        );
    }

    #[test]
    fn parse_exact_framerate_integer_and_fraction() {
        assert_eq!(parse_exact_framerate("50"), Some((50, 1)));
        assert_eq!(parse_exact_framerate("30000/1001"), Some((30_000, 1_001)));
        assert_eq!(parse_exact_framerate("  60000 / 1001 "), Some((60_000, 1_001)));
    }

    #[test]
    fn parse_exact_framerate_rejects_malformed() {
        assert_eq!(parse_exact_framerate(""), None);
        assert_eq!(parse_exact_framerate("oops"), None);
        assert_eq!(parse_exact_framerate("30000/0"), None);
        assert_eq!(parse_exact_framerate("30/abc"), None);
    }

    // -- synthesis-side helpers: format_exact_framerate +
    //    format_ptime_ns_as_ms + sdp_colorimetry_from_caps

    #[test]
    fn format_exact_framerate_emits_bare_integer_when_denominator_is_one() {
        assert_eq!(format_exact_framerate(50, 1), "50");
        assert_eq!(format_exact_framerate(25, 1), "25");
    }

    #[test]
    fn format_exact_framerate_emits_rational_for_non_unit_denominator() {
        assert_eq!(format_exact_framerate(30_000, 1_001), "30000/1001");
        assert_eq!(format_exact_framerate(60_000, 1_001), "60000/1001");
    }

    #[test]
    fn format_exact_framerate_round_trips_parse_exact_framerate() {
        for (num, den) in [(25_u32, 1_u32), (50, 1), (30_000, 1_001), (60_000, 1_001)] {
            let s = format_exact_framerate(num, den);
            assert_eq!(parse_exact_framerate(&s), Some((num, den)), "round-trip {s}");
        }
    }

    #[test]
    fn format_ptime_ns_as_ms_emits_bare_integer_for_whole_milliseconds() {
        assert_eq!(format_ptime_ns_as_ms(1_000_000), "1");
        assert_eq!(format_ptime_ns_as_ms(4_000_000), "4");
        assert_eq!(format_ptime_ns_as_ms(20_000_000), "20");
    }

    #[test]
    fn format_ptime_ns_as_ms_emits_decimal_for_sub_millisecond() {
        assert_eq!(format_ptime_ns_as_ms(125_000), "0.125");
        assert_eq!(format_ptime_ns_as_ms(250_000), "0.25");
        assert_eq!(format_ptime_ns_as_ms(500_000), "0.5");
    }

    #[test]
    fn format_ptime_ns_as_ms_pins_st2110_30_canonical_values() {
        assert_eq!(format_ptime_ns_as_ms(defaults::AUDIO_PTIME_NS), "1");
        assert_eq!(format_ptime_ns_as_ms(125_000), "0.125");
    }

    #[test]
    fn sdp_colorimetry_from_caps_pins_each_preset() {
        assert_eq!(
            sdp_colorimetry_from_caps("bt601"),
            Some(("BT601", None)),
        );
        assert_eq!(
            sdp_colorimetry_from_caps("bt709"),
            Some(("BT709", None)),
        );
        assert_eq!(
            sdp_colorimetry_from_caps("smpte240m"),
            Some(("SMPTE240M", None)),
        );
    }

    #[test]
    fn sdp_colorimetry_from_caps_collapses_bt2020_depth_variants_to_one_sdp_value() {
        assert_eq!(
            sdp_colorimetry_from_caps("bt2020"),
            Some(("BT2020", None)),
        );
        assert_eq!(
            sdp_colorimetry_from_caps("bt2020-10"),
            Some(("BT2020", None)),
        );
    }

    #[test]
    fn sdp_colorimetry_from_caps_splits_bt2100_via_tcs() {
        assert_eq!(
            sdp_colorimetry_from_caps("bt2100-pq"),
            Some(("BT2100", Some("PQ"))),
        );
        assert_eq!(
            sdp_colorimetry_from_caps("bt2100-hlg"),
            Some(("BT2100", Some("HLG"))),
        );
    }

    #[test]
    fn sdp_colorimetry_from_caps_unrecognised_inputs_yield_none() {
        assert_eq!(sdp_colorimetry_from_caps(""), None);
        assert_eq!(sdp_colorimetry_from_caps("1:3:5:1"), None);
        assert_eq!(sdp_colorimetry_from_caps("sRGB"), None);
    }

    // -- rtp_caps_from_raw_video

    /// Build a synthetic `video/x-raw` caps with the supplied
    /// parameters; helper for the synthesis tests below.
    fn raw_video_caps(
        format: &str,
        width: i32,
        height: i32,
        framerate: gst::Fraction,
        extras: Option<&str>,
    ) -> gst::Caps {
        let extras = extras.map(|s| format!(",{s}")).unwrap_or_default();
        gst::Caps::from_str(&format!(
            "video/x-raw,format={format},width={width},height={height},\
             framerate={n}/{d}{extras}",
            n = framerate.numer(),
            d = framerate.denom(),
        ))
        .expect("raw video caps")
    }

    #[test]
    fn rtp_caps_from_raw_video_uyvy_maps_to_ycbcr422_depth8() {
        init_gst();
        let raw = raw_video_caps("UYVY", 1920, 1080, gst::Fraction::new(50, 1), None);
        let rtp = rtp_caps_from_raw_video(&raw, 96).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(s.name().as_str(), "application/x-rtp");
        assert_eq!(s.get::<&str>("media").unwrap(), "video");
        assert_eq!(s.get::<i32>("clock-rate").unwrap(), defaults::VIDEO_CLOCK_RATE);
        // Canonical wire-form case: RFC 4175 lower-case
        // `raw`, ST 2110-20 upper-case `PM` / `SSN`. See
        // the comment on the caps-text builder in
        // `rtp_caps_from_raw_video` for the libnvnmos /
        // nmos-cpp compatibility rationale.
        assert_eq!(s.get::<&str>("encoding-name").unwrap(), "raw");
        assert_eq!(s.get::<i32>("payload").unwrap(), 96);
        assert_eq!(s.get::<&str>("sampling").unwrap(), "YCbCr-4:2:2");
        assert_eq!(s.get::<&str>("depth").unwrap(), "8");
        assert_eq!(s.get::<&str>("width").unwrap(), "1920");
        assert_eq!(s.get::<&str>("height").unwrap(), "1080");
        assert_eq!(s.get::<&str>("exactframerate").unwrap(), "50");
        assert_eq!(s.get::<&str>("PM").unwrap(), defaults::ST2110_20_PM);
        assert_eq!(s.get::<&str>("SSN").unwrap(), defaults::ST2110_20_SSN);
    }

    #[test]
    fn rtp_caps_from_raw_video_uyvp_maps_to_ycbcr422_depth10() {
        init_gst();
        let raw = raw_video_caps("UYVP", 1920, 1080, gst::Fraction::new(50, 1), None);
        let rtp = rtp_caps_from_raw_video(&raw, 96).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(s.get::<&str>("sampling").unwrap(), "YCbCr-4:2:2");
        assert_eq!(s.get::<&str>("depth").unwrap(), "10");
    }

    #[test]
    fn rtp_caps_from_raw_video_emits_fractional_exactframerate() {
        init_gst();
        let raw = raw_video_caps(
            "UYVP",
            1920,
            1080,
            gst::Fraction::new(30_000, 1_001),
            None,
        );
        let rtp = rtp_caps_from_raw_video(&raw, 96).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(s.get::<&str>("exactframerate").unwrap(), "30000/1001");
    }

    #[test]
    fn rtp_caps_from_raw_video_emits_interlace_only_when_interleaved() {
        init_gst();
        let progressive = raw_video_caps(
            "UYVP",
            1920,
            1080,
            gst::Fraction::new(50, 1),
            Some("interlace-mode=progressive"),
        );
        let rtp = rtp_caps_from_raw_video(&progressive, 96).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert!(
            s.get::<&str>("interlace").is_err(),
            "progressive must omit `interlace=`",
        );

        let interleaved = raw_video_caps(
            "UYVP",
            1920,
            1080,
            gst::Fraction::new(25, 1),
            Some("interlace-mode=interleaved"),
        );
        let rtp = rtp_caps_from_raw_video(&interleaved, 96).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(
            s.get::<&str>("interlace").unwrap(),
            "1",
            "interleaved must emit `interlace=1` per RFC 4175 §6.1",
        );
    }

    #[test]
    fn rtp_caps_from_raw_video_emits_colorimetry_for_recognised_preset() {
        init_gst();
        let raw = raw_video_caps(
            "UYVP",
            1920,
            1080,
            gst::Fraction::new(50, 1),
            Some("colorimetry=bt709"),
        );
        let rtp = rtp_caps_from_raw_video(&raw, 96).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(s.get::<&str>("colorimetry").unwrap(), "BT709");
        assert!(
            s.get::<&str>("tcs").is_err(),
            "non-BT2100 presets must not emit `tcs=`",
        );
    }

    #[test]
    fn rtp_caps_from_raw_video_emits_tcs_for_bt2100_presets() {
        init_gst();
        let pq = raw_video_caps(
            "UYVP",
            1920,
            1080,
            gst::Fraction::new(50, 1),
            Some("colorimetry=bt2100-pq"),
        );
        let rtp = rtp_caps_from_raw_video(&pq, 96).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(s.get::<&str>("colorimetry").unwrap(), "BT2100");
        assert_eq!(s.get::<&str>("tcs").unwrap(), "PQ");

        let hlg = raw_video_caps(
            "UYVP",
            1920,
            1080,
            gst::Fraction::new(50, 1),
            Some("colorimetry=bt2100-hlg"),
        );
        let rtp = rtp_caps_from_raw_video(&hlg, 96).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(s.get::<&str>("colorimetry").unwrap(), "BT2100");
        assert_eq!(s.get::<&str>("tcs").unwrap(), "HLG");
    }

    #[test]
    fn rtp_caps_from_raw_video_falls_back_to_default_on_unrecognised_colorimetry() {
        init_gst();
        let raw = raw_video_caps(
            "UYVP",
            1920,
            1080,
            gst::Fraction::new(50, 1),
            Some("colorimetry=1:3:5:1"),
        );
        let rtp = rtp_caps_from_raw_video(&raw, 96).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        // `colorimetry=` is REQUIRED by nmos-cpp's
        // `get_video_raw_parameters`; an unrecognised value on
        // the essence caps doesn't translate to a known SDP
        // colorimetry preset, but we still have to emit one or
        // libnvnmos silently rejects the sender. Fall back to
        // [`defaults::ST2110_20_COLORIMETRY`] (BT709) rather
        // than dropping the field entirely.
        assert_eq!(
            s.get::<&str>("colorimetry").unwrap(),
            defaults::ST2110_20_COLORIMETRY,
            "unrecognised input colorimetry must fall back to the BT709 default, \
             not drop the field (libnvnmos rejects SDPs without colorimetry)",
        );
    }

    #[test]
    fn rtp_caps_from_raw_video_rejects_unsupported_format() {
        init_gst();
        let raw = raw_video_caps("RGBA", 1920, 1080, gst::Fraction::new(50, 1), None);
        let err = rtp_caps_from_raw_video(&raw, 96).expect_err("must reject");
        assert!(matches!(err, SdpError::UnsupportedEssence(ref m) if m.contains("RGBA")));
    }

    #[test]
    fn rtp_caps_from_raw_video_round_trips_through_raw_caps_from_rtp_video() {
        init_gst();
        let original = raw_video_caps(
            "UYVP",
            1920,
            1080,
            gst::Fraction::new(50, 1),
            Some("interlace-mode=progressive,colorimetry=bt2020-10"),
        );
        let rtp = rtp_caps_from_raw_video(&original, 96).expect("synth");
        let round_tripped = raw_caps_from_rtp_video(&rtp).expect("parse back");
        let rt = round_tripped.structure(0).expect("rt raw");
        let orig = original.structure(0).expect("orig raw");
        assert_eq!(rt.name(), orig.name());
        assert_eq!(rt.get::<&str>("format"), orig.get::<&str>("format"));
        assert_eq!(rt.get::<i32>("width"), orig.get::<i32>("width"));
        assert_eq!(rt.get::<i32>("height"), orig.get::<i32>("height"));
        assert_eq!(
            rt.get::<gst::Fraction>("framerate"),
            orig.get::<gst::Fraction>("framerate"),
        );
        // bt2020-10 collapses to BT2020 on the synthesise side
        // (depth carries the bit-depth distinction) and then
        // expands back to bt2020-10 on the parse side because
        // depth=10 is in the RTP caps.
        assert_eq!(
            rt.get::<&str>("colorimetry").unwrap(),
            "bt2020-10",
        );
    }

    // -- rtp_caps_from_raw_audio

    fn raw_audio_caps(format: &str, rate: i32, channels: i32) -> gst::Caps {
        gst::Caps::from_str(&format!(
            "audio/x-raw,format={format},rate={rate},channels={channels},\
             layout=interleaved",
        ))
        .expect("raw audio caps")
    }

    #[test]
    fn rtp_caps_from_raw_audio_s24be_maps_to_l24() {
        init_gst();
        let raw = raw_audio_caps("S24BE", 48_000, 2);
        let rtp =
            rtp_caps_from_raw_audio(&raw, 97, defaults::AUDIO_PTIME_NS, None).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(s.get::<&str>("media").unwrap(), "audio");
        assert_eq!(s.get::<&str>("encoding-name").unwrap(), "L24");
        assert_eq!(s.get::<i32>("clock-rate").unwrap(), 48_000);
        assert_eq!(s.get::<i32>("payload").unwrap(), 97);
        assert_eq!(s.get::<&str>("encoding-params").unwrap(), "2");
        assert_eq!(s.get::<&str>("a-ptime").unwrap(), "1");
        assert!(
            s.get::<&str>("a-maxptime").is_err(),
            "no maxptime requested means no `a-maxptime` field",
        );
    }

    #[test]
    fn rtp_caps_from_raw_audio_s16be_maps_to_l16() {
        init_gst();
        let raw = raw_audio_caps("S16BE", 48_000, 2);
        let rtp =
            rtp_caps_from_raw_audio(&raw, 97, defaults::AUDIO_PTIME_NS, None).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(s.get::<&str>("encoding-name").unwrap(), "L16");
    }

    #[test]
    fn rtp_caps_from_raw_audio_emits_decimal_ptime_for_sub_millisecond() {
        init_gst();
        let raw = raw_audio_caps("S24BE", 48_000, 2);
        let rtp = rtp_caps_from_raw_audio(&raw, 97, 125_000, None).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(
            s.get::<&str>("a-ptime").unwrap(),
            "0.125",
            "ST 2110-30 low-latency ptime must round-trip as `0.125`",
        );
    }

    #[test]
    fn rtp_caps_from_raw_audio_emits_a_maxptime_when_supplied() {
        init_gst();
        let raw = raw_audio_caps("S24BE", 48_000, 2);
        let rtp =
            rtp_caps_from_raw_audio(&raw, 97, defaults::AUDIO_PTIME_NS, Some(4_000_000))
                .expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(s.get::<&str>("a-maxptime").unwrap(), "4");
    }

    #[test]
    fn rtp_caps_from_raw_audio_rejects_unsupported_format() {
        init_gst();
        let raw = raw_audio_caps("S32LE", 48_000, 2);
        let err =
            rtp_caps_from_raw_audio(&raw, 97, defaults::AUDIO_PTIME_NS, None).expect_err("reject");
        assert!(matches!(err, SdpError::UnsupportedEssence(ref m) if m.contains("S32LE")));
    }

    #[test]
    fn rtp_caps_from_raw_audio_round_trips_through_raw_caps_from_rtp_audio() {
        init_gst();
        let original = raw_audio_caps("S24BE", 48_000, 2);
        let rtp =
            rtp_caps_from_raw_audio(&original, 97, defaults::AUDIO_PTIME_NS, None).expect("synth");
        let round_tripped = raw_caps_from_rtp_audio(&rtp).expect("parse back");
        let rt = round_tripped.structure(0).expect("rt raw");
        let orig = original.structure(0).expect("orig");
        assert_eq!(rt.name(), orig.name());
        assert_eq!(rt.get::<&str>("format"), orig.get::<&str>("format"));
        assert_eq!(rt.get::<i32>("rate"), orig.get::<i32>("rate"));
        assert_eq!(rt.get::<i32>("channels"), orig.get::<i32>("channels"));
    }

    // -- rtp_caps_from_raw_data

    #[test]
    fn rtp_caps_from_raw_data_minimal_meta_x_st_2038() {
        init_gst();
        let raw = gst::Caps::from_str("meta/x-st-2038,alignment=frame").expect("data caps");
        let rtp = rtp_caps_from_raw_data(&raw, 100).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(s.get::<&str>("media").unwrap(), "video");
        assert_eq!(s.get::<&str>("encoding-name").unwrap(), "SMPTE291");
        assert_eq!(s.get::<i32>("clock-rate").unwrap(), defaults::ANC_CLOCK_RATE);
        assert_eq!(s.get::<i32>("payload").unwrap(), 100);
        assert!(
            s.get::<&str>("exactframerate").is_err(),
            "ANC without framerate must omit `exactframerate=`",
        );
    }

    #[test]
    fn rtp_caps_from_raw_data_propagates_framerate() {
        init_gst();
        let raw = gst::Caps::from_str("meta/x-st-2038,alignment=frame,framerate=25/1")
            .expect("data caps");
        let rtp = rtp_caps_from_raw_data(&raw, 100).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(s.get::<&str>("exactframerate").unwrap(), "25");

        let fractional = gst::Caps::from_str(
            "meta/x-st-2038,alignment=frame,framerate=30000/1001",
        )
        .expect("data caps");
        let rtp = rtp_caps_from_raw_data(&fractional, 100).expect("synth");
        let s = rtp.structure(0).expect("rtp");
        assert_eq!(s.get::<&str>("exactframerate").unwrap(), "30000/1001");
    }

    #[test]
    fn rtp_caps_from_raw_data_rejects_non_anc_essence() {
        init_gst();
        let raw = gst::Caps::from_str("video/x-raw,format=UYVY").expect("not ANC");
        let err = rtp_caps_from_raw_data(&raw, 100).expect_err("must reject");
        assert!(matches!(err, SdpError::UnsupportedEssence(ref m) if m.contains("video/x-raw")));
    }

    #[test]
    fn rtp_caps_from_raw_data_round_trips_through_raw_caps_from_rtp_data() {
        init_gst();
        let original = gst::Caps::from_str(
            "meta/x-st-2038,alignment=frame,framerate=25/1",
        )
        .expect("data caps");
        let rtp = rtp_caps_from_raw_data(&original, 100).expect("synth");
        let round_tripped = raw_caps_from_rtp_data(&rtp).expect("parse back");
        let rt = round_tripped.structure(0).expect("rt raw");
        let orig = original.structure(0).expect("orig");
        assert_eq!(rt.name(), orig.name());
        assert_eq!(
            rt.get::<gst::Fraction>("framerate"),
            orig.get::<gst::Fraction>("framerate"),
        );
    }

    // -- defaults: synthesis-specific additions

    #[test]
    fn defaults_video_and_anc_clock_rates_are_90khz() {
        // RFC 4175 §5.5 fixes RTP raw-video clock-rate at 90 kHz
        // regardless of frame rate / depth / sampling; RFC 8331 /
        // ST 2110-40 §4.4 likewise locks ANC to 90 kHz so its RTP
        // timestamps share the paired video flow's lattice.
        assert_eq!(defaults::VIDEO_CLOCK_RATE, 90_000);
        assert_eq!(defaults::ANC_CLOCK_RATE, 90_000);
    }

    #[test]
    fn defaults_st2110_20_pm_and_ssn_match_nmos_cpp() {
        assert_eq!(defaults::ST2110_20_PM, "2110GPM");
        assert_eq!(defaults::ST2110_20_SSN, "ST2110-20:2017");
    }

    /// `colorimetry=` is REQUIRED by nmos-cpp's
    /// `get_video_raw_parameters` and the SMPTE ST 2110-20
    /// reference SDPs all use `BT709` for SDR. Keep the default
    /// pinned so a synthesised SDP without an explicit
    /// colorimetry on the essence caps still parses against
    /// nmos-cpp / libnvnmos.
    #[test]
    fn defaults_st2110_20_colorimetry_is_bt709() {
        assert_eq!(defaults::ST2110_20_COLORIMETRY, "BT709");
    }

    // -- from_caps + helpers (resolve_payload_type,
    //    resolve_audio_ptime, parse_ptime_ms_as_ns,
    //    resolved_audio_caps, udp_leg_from_input)

    fn build_input<'a>(
        essence_caps: &'a gst::Caps,
        side: Side,
        transport_caps: Option<&'a gst::Caps>,
    ) -> SdpBuildInput<'a> {
        SdpBuildInput {
            essence_caps,
            transport_caps,
            side,
            label: "test-label",
            description: "test-description",
            name: "test-name",
            source_ip: "192.0.2.10",
            source_port: 5004,
            destination_ip: "239.0.0.1",
            destination_port: 5004,
            interface_ip: "192.0.2.11",
            advertise_caps: false,
            node_seed: "demo-node1",
        }
    }

    #[test]
    fn resolve_payload_type_falls_back_to_per_essence_defaults() {
        assert_eq!(
            resolve_payload_type(FlowFormat::Video, None).unwrap(),
            defaults::VIDEO_PAYLOAD_TYPE as u8
        );
        assert_eq!(
            resolve_payload_type(FlowFormat::Audio, None).unwrap(),
            defaults::AUDIO_PAYLOAD_TYPE as u8
        );
        assert_eq!(
            resolve_payload_type(FlowFormat::Data, None).unwrap(),
            defaults::ANC_PAYLOAD_TYPE as u8
        );
    }

    #[test]
    fn resolve_payload_type_honours_transport_caps_override() {
        init_gst();
        let tc = gst::Caps::from_str("application/x-rtp,payload=(int)110").unwrap();
        assert_eq!(resolve_payload_type(FlowFormat::Video, Some(&tc)).unwrap(), 110);
    }

    #[test]
    fn resolve_payload_type_rejects_out_of_range_override() {
        init_gst();
        for pt in [0_i32, 95, 128, 200] {
            let tc = gst::Caps::from_str(&format!("application/x-rtp,payload=(int){pt}")).unwrap();
            let err = resolve_payload_type(FlowFormat::Audio, Some(&tc)).expect_err("reject");
            assert!(
                matches!(err, SdpError::InvalidPayloadType(_)),
                "pt={pt} must be rejected, got {err:?}",
            );
        }
    }

    #[test]
    fn parse_ptime_ms_as_ns_round_trips_format_ptime_ns_as_ms() {
        for ns in [125_000_u64, 250_000, 1_000_000, 4_000_000, 20_000_000] {
            let s = format_ptime_ns_as_ms(ns);
            assert_eq!(parse_ptime_ms_as_ns(&s), Some(ns), "round-trip {ns}ns -> {s}");
        }
    }

    #[test]
    fn parse_ptime_ms_as_ns_rejects_malformed() {
        assert_eq!(parse_ptime_ms_as_ns(""), None);
        assert_eq!(parse_ptime_ms_as_ns("oops"), None);
        assert_eq!(parse_ptime_ms_as_ns("-1"), None);
    }

    #[test]
    fn resolve_audio_ptime_falls_back_to_defaults() {
        let (p, m) = resolve_audio_ptime(None);
        assert_eq!(p, defaults::AUDIO_PTIME_NS);
        assert_eq!(m, None);
    }

    #[test]
    fn resolve_audio_ptime_reads_override_strings() {
        init_gst();
        let tc = gst::Caps::from_str(
            "application/x-rtp,a-ptime=(string)0.125,a-maxptime=(string)4",
        )
        .unwrap();
        let (p, m) = resolve_audio_ptime(Some(&tc));
        assert_eq!(p, 125_000);
        assert_eq!(m, Some(4_000_000));
    }

    #[test]
    fn resolved_audio_caps_overrides_rate_from_transport_caps() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let tc = gst::Caps::from_str("application/x-rtp,clock-rate=(int)44100").unwrap();
        let resolved = resolved_audio_caps(&essence, Some(&tc)).unwrap();
        let s = resolved.structure(0).unwrap();
        assert_eq!(s.get::<i32>("rate").unwrap(), 44_100);
    }

    #[test]
    fn resolved_audio_caps_returns_essence_when_no_override() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let resolved = resolved_audio_caps(&essence, None).unwrap();
        let s = resolved.structure(0).unwrap();
        assert_eq!(s.get::<i32>("rate").unwrap(), 48_000);
    }

    #[test]
    fn udp_leg_from_input_sender_duplicates_source_ip_into_interface_ip() {
        init_gst();
        let essence = raw_video_caps("UYVP", 1920, 1080, gst::Fraction::new(50, 1), None);
        let mut input = build_input(&essence, Side::Sender, None);
        input.interface_ip = "";
        let leg = udp_leg_from_input(&input);
        assert_eq!(leg.destination_ip, "239.0.0.1");
        assert_eq!(leg.destination_port, 5004);
        assert_eq!(leg.source_ip.as_deref(), Some("192.0.2.10"));
        assert_eq!(
            leg.interface_ip.as_deref(),
            Some("192.0.2.10"),
            "Sender's egress NIC duplicates source_ip into interface_ip",
        );
        assert_eq!(leg.source_port, Some(5004));
    }

    #[test]
    fn udp_leg_from_input_receiver_keeps_source_ip_and_interface_ip_distinct() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let input = build_input(&essence, Side::Receiver, None);
        let leg = udp_leg_from_input(&input);
        assert_eq!(leg.source_ip.as_deref(), Some("192.0.2.10"));
        assert_eq!(leg.interface_ip.as_deref(), Some("192.0.2.11"));
        assert_eq!(leg.source_port, None, "Receiver carries no source_port");
    }

    #[test]
    fn udp_leg_from_input_zero_destination_port_falls_back_to_rtp_default() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let mut input = build_input(&essence, Side::Sender, None);
        input.destination_port = 0;
        let leg = udp_leg_from_input(&input);
        assert_eq!(leg.destination_port, defaults::RTP_PORT);
    }

    #[test]
    fn udp_leg_from_input_zero_source_port_emits_none() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let mut input = build_input(&essence, Side::Sender, None);
        input.source_port = 0;
        let leg = udp_leg_from_input(&input);
        assert_eq!(leg.source_port, None);
    }

    #[test]
    fn from_caps_video_synthesises_st2110_20_sdp() {
        init_gst();
        let essence = raw_video_caps(
            "UYVP",
            1920,
            1080,
            gst::Fraction::new(50, 1),
            Some("colorimetry=bt709,interlace-mode=progressive"),
        );
        let input = build_input(&essence, Side::Sender, None);
        let text = from_caps(&input).expect("synth");

        assert!(text.contains("s=test-label"), "s= carries label:\n{text}");
        assert!(text.contains("i=test-description"), "i= carries description");
        assert!(
            text.contains("a=x-nvnmos-name:test-name"),
            "session-level a=x-nvnmos-name carries the resource name",
        );
        assert!(text.contains("m=video 5004 RTP/AVP 96"), "m= line:\n{text}");
        assert!(
            text.contains("c=IN IP4 239.0.0.1/"),
            "multicast c= keeps the TTL suffix:\n{text}",
        );
        assert!(
            text.contains("a=rtpmap:96 raw/90000"),
            "rtpmap encoding-name + clock-rate (lower-case per \
             RFC 4175 §6.7 BNF; nmos-cpp matches it case-sensitively):\n{text}",
        );
        assert!(text.contains("sampling=YCbCr-4:2:2"), "fmtp sampling:\n{text}");
        assert!(text.contains("depth=10"), "fmtp depth=10 for UYVP:\n{text}");
        assert!(text.contains("width=1920"));
        assert!(text.contains("height=1080"));
        assert!(text.contains("exactframerate=50"));
        assert!(text.contains("colorimetry=BT709"));
        // gst-sdp's `set_media_from_caps` preserves caps-field
        // casing verbatim. ST 2110-20 §6.3 mandates upper-case
        // `PM` / `SSN` and nmos-cpp (`sdp::fields::packing_mode
        // = U("PM")`, `smpte_standard_number = U("SSN")`)
        // matches them case-sensitively, so the canonical wire
        // case has to be set in our caps-text builder. See the
        // comment on `rtp_caps_from_raw_video` for the full
        // compat story (lower-case `raw` + upper-case `PM` /
        // `SSN` is what libnvnmos accepts).
        assert!(text.contains("PM=2110GPM"), "ST 2110-20 PM:\n{text}");
        assert!(text.contains("SSN=ST2110-20:2017"), "ST 2110-20 SSN:\n{text}");
        assert!(
            text.contains("a=source-filter: incl IN IP4 239.0.0.1 192.0.2.10"),
            "Sender source-filter:\n{text}",
        );
        assert!(
            text.contains("a=x-nvnmos-iface-ip:192.0.2.10"),
            "Sender's iface-ip duplicates source_ip:\n{text}",
        );
        assert!(text.contains("a=x-nvnmos-src-port:5004"));
        assert!(
            !text.contains("a=x-nvnmos-caps"),
            "narrow advertise_caps=false omits caps attribute:\n{text}",
        );
        assert!(
            text.contains("a=mediaclk:direct=0"),
            "synthesis path emits ST 2110-10 direct media clock:\n{text}",
        );
        let sess_id = stable_origin_session_id("demo-node1", Side::Sender, "test-name");
        assert!(
            text.contains(&format!("o=nvnmos {sess_id} 0 IN IP4")),
            "o= sess-id is stable from node_seed+side+name:\n{text}",
        );
        assert_ne!(sess_id, "1", "sess-id must not be the old placeholder");
    }

    #[test]
    fn stable_origin_session_id_is_deterministic_and_scoped() {
        let a = stable_origin_session_id("demo-node1", Side::Sender, "video1");
        let b = stable_origin_session_id("demo-node1", Side::Sender, "video1");
        let other_name = stable_origin_session_id("demo-node1", Side::Sender, "audio1");
        let other_side = stable_origin_session_id("demo-node1", Side::Receiver, "video1");
        let other_seed = stable_origin_session_id("demo-node2", Side::Sender, "video1");
        assert_eq!(a, b);
        assert_ne!(a, other_name);
        assert_ne!(a, other_side);
        assert_ne!(a, other_seed);
        assert_ne!(a, "0");
        assert_ne!(a, "1");
    }

    #[test]
    fn from_caps_audio_synthesises_st2110_30_sdp_with_default_ptime() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let input = build_input(&essence, Side::Receiver, None);
        let text = from_caps(&input).expect("synth");
        assert!(text.contains("m=audio 5004 RTP/AVP 97"));
        assert!(text.contains("a=rtpmap:97 L24/48000/2"));
        assert!(text.contains("a=ptime:1"), "default 1ms ptime:\n{text}");
        assert!(
            !text.contains("a=maxptime"),
            "maxptime omitted when unset:\n{text}",
        );
        assert!(
            text.contains("a=x-nvnmos-iface-ip:192.0.2.11"),
            "Receiver iface-ip distinct from source_ip:\n{text}",
        );
        assert!(text.contains("a=mediaclk:direct=0"));
    }

    #[test]
    fn from_caps_audio_honours_transport_caps_overrides() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let tc = gst::Caps::from_str(
            "application/x-rtp,payload=(int)98,clock-rate=(int)96000,\
             a-ptime=(string)0.125,a-maxptime=(string)4",
        )
        .unwrap();
        let input = build_input(&essence, Side::Sender, Some(&tc));
        let text = from_caps(&input).expect("synth");
        assert!(text.contains("m=audio 5004 RTP/AVP 98"), "pt override:\n{text}");
        assert!(
            text.contains("a=rtpmap:98 L24/96000/2"),
            "audio clock-rate override:\n{text}",
        );
        assert!(text.contains("a=ptime:0.125"));
        assert!(text.contains("a=maxptime:4"));
    }

    #[test]
    fn from_caps_data_synthesises_st2110_40_sdp() {
        init_gst();
        let essence = gst::Caps::from_str(
            "meta/x-st-2038,alignment=frame,framerate=25/1",
        )
        .unwrap();
        let input = build_input(&essence, Side::Sender, None);
        let text = from_caps(&input).expect("synth");
        assert!(text.contains("m=video 5004 RTP/AVP 100"), "ANC pt=100:\n{text}");
        // RFC 8331 §5.1 / SMPTE ST 2110-40 §6 spell the encoding
        // name lower-case (`smpte291`); `nmos-cpp`'s
        // `media_types::video_smpte291` (`U("video/smpte291")`)
        // and `get_format` match it case-sensitively. The
        // canonicaliser at `build_sdp`'s tail lower-cases the
        // gst-uppercased `SMPTE291` that
        // [`rtp_caps_from_raw_data`] carries in the
        // `application/x-rtp` caps (gst convention) so the wire
        // form lands canonical.
        assert!(
            text.contains("a=rtpmap:100 smpte291/90000"),
            "ANC rtpmap must be lower-case on the wire per RFC 8331 §5.1:\n{text}",
        );
        assert!(text.contains("exactframerate=25"));
    }

    #[test]
    fn from_caps_advertise_caps_emits_x_nvnmos_caps_with_pt() {
        init_gst();
        let essence = raw_audio_caps("S16BE", 48_000, 2);
        let mut input = build_input(&essence, Side::Receiver, None);
        input.advertise_caps = true;
        let text = from_caps(&input).expect("synth");
        assert!(
            text.contains("a=x-nvnmos-caps:97"),
            "wide caps advertised with bare pt:\n{text}",
        );
    }

    #[test]
    fn from_caps_rejects_unsupported_essence() {
        init_gst();
        let essence = gst::Caps::from_str("video/x-h264").unwrap();
        let input = build_input(&essence, Side::Sender, None);
        let err = from_caps(&input).expect_err("must reject");
        assert!(matches!(err, SdpError::UnsupportedEssence(_)));
    }

    #[test]
    fn from_caps_invalid_pt_override_propagates_invalid_payload_type() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let tc = gst::Caps::from_str("application/x-rtp,payload=(int)95").unwrap();
        let input = build_input(&essence, Side::Sender, Some(&tc));
        let err = from_caps(&input).expect_err("must reject");
        assert!(matches!(err, SdpError::InvalidPayloadType(95)));
    }

    #[test]
    fn from_caps_round_trips_through_parse_sdp_for_video() {
        init_gst();
        let essence = raw_video_caps(
            "UYVP",
            1920,
            1080,
            gst::Fraction::new(50, 1),
            Some("colorimetry=bt709,interlace-mode=progressive"),
        );
        let input = build_input(&essence, Side::Sender, None);
        let text = from_caps(&input).expect("synth");
        let media = parse_sdp(&text).expect("round-trip parse");
        assert_eq!(media.format, FlowFormat::Video);
        assert_eq!(media.primary.destination_ip, "239.0.0.1");
        assert_eq!(media.primary.destination_port, 5004);
        assert_eq!(media.primary.source_ip.as_deref(), Some("192.0.2.10"));
        assert_eq!(media.primary.interface_ip.as_deref(), Some("192.0.2.10"));
        let rt_raw = media.raw_caps.structure(0).unwrap();
        let orig_raw = essence.structure(0).unwrap();
        assert_eq!(rt_raw.get::<&str>("format"), orig_raw.get::<&str>("format"));
        assert_eq!(rt_raw.get::<i32>("width"), orig_raw.get::<i32>("width"));
        assert_eq!(rt_raw.get::<i32>("height"), orig_raw.get::<i32>("height"));
        assert_eq!(
            rt_raw.get::<gst::Fraction>("framerate"),
            orig_raw.get::<gst::Fraction>("framerate"),
        );
        assert_eq!(rt_raw.get::<&str>("colorimetry").unwrap(), "bt709");
    }

    #[test]
    fn from_caps_round_trips_through_parse_sdp_for_audio() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let input = build_input(&essence, Side::Receiver, None);
        let text = from_caps(&input).expect("synth");
        let media = parse_sdp(&text).expect("round-trip parse");
        assert_eq!(media.format, FlowFormat::Audio);
        let rtp = media.rtp_caps.structure(0).unwrap();
        assert_eq!(rtp.get::<&str>("encoding-name").unwrap(), "L24");
        assert_eq!(rtp.get::<i32>("clock-rate").unwrap(), 48_000);
        assert_eq!(rtp.get::<&str>("encoding-params").unwrap(), "2");
        assert_eq!(rtp.get::<&str>("a-ptime").unwrap(), "1");
    }

    #[test]
    fn from_caps_round_trips_through_parse_sdp_for_data() {
        init_gst();
        let essence = gst::Caps::from_str(
            "meta/x-st-2038,alignment=frame,framerate=25/1",
        )
        .unwrap();
        let input = build_input(&essence, Side::Sender, None);
        let text = from_caps(&input).expect("synth");
        let media = parse_sdp(&text).expect("round-trip parse");
        assert_eq!(media.format, FlowFormat::Data);
        let rt_raw = media.raw_caps.structure(0).unwrap();
        assert_eq!(rt_raw.name().as_str(), "meta/x-st-2038");
        assert_eq!(
            rt_raw.get::<gst::Fraction>("framerate").unwrap(),
            gst::Fraction::new(25, 1),
        );
    }

    #[test]
    fn from_caps_unicast_destination_omits_ttl_suffix() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let mut input = build_input(&essence, Side::Sender, None);
        input.destination_ip = "192.0.2.99";
        let text = from_caps(&input).expect("synth");
        assert!(
            text.contains("c=IN IP4 192.0.2.99\r\n") || text.contains("c=IN IP4 192.0.2.99\n"),
            "unicast c= line omits /<ttl> suffix per RFC 4566 §5.7:\n{text}",
        );
    }

    #[test]
    fn from_caps_empty_label_falls_back_to_nvnmos() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let mut input = build_input(&essence, Side::Sender, None);
        input.label = "";
        let text = from_caps(&input).expect("synth");
        assert!(text.contains("s=nvnmos"), "default session name:\n{text}");
    }

    #[test]
    fn from_caps_empty_description_omits_information_line() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let mut input = build_input(&essence, Side::Sender, None);
        input.description = "";
        let text = from_caps(&input).expect("synth");
        assert!(!text.contains("\r\ni="), "no i= line emitted:\n{text}");
    }

    #[test]
    fn from_caps_empty_name_omits_x_nvnmos_name_attribute() {
        init_gst();
        let essence = raw_audio_caps("S24BE", 48_000, 2);
        let mut input = build_input(&essence, Side::Sender, None);
        input.name = "";
        let text = from_caps(&input).expect("synth");
        assert!(
            !text.contains("a=x-nvnmos-name"),
            "no a=x-nvnmos-name attribute emitted:\n{text}",
        );
    }

    /// Regression guard for the synthesised raw-video SDP wire
    /// case. nmos-cpp's `make_video_raw_sdp_parameters` emits
    /// the rtpmap encoding name lower-case (`raw`) and the
    /// ST 2110-20 fmtp keys upper-case (`PM=`, `SSN=`); both
    /// `get_format` and the fmtp parser are case-sensitive on
    /// these tokens. If we slip back to upper-case `RAW` /
    /// lower-case `pm` / `ssn`, libnvnmos's
    /// `add_nmos_sender_to_node_server` silently returns
    /// `false` (it catches all exceptions including the one
    /// `get_format` throws when the encoding name doesn't
    /// match), so this guard pins the on-wire form end-to-end.
    #[test]
    fn from_caps_video_raw_emits_canonical_wire_case() {
        init_gst();
        let essence = gst::Caps::from_str(
            "video/x-raw,format=UYVY,width=1280,height=720,framerate=25/1,interlace-mode=progressive",
        )
        .expect("video caps");
        let input = SdpBuildInput {
            essence_caps: &essence,
            transport_caps: None,
            side: Side::Sender,
            label: "video1",
            description: "",
            name: "video1",
            source_ip: "192.0.2.10",
            source_port: 0,
            destination_ip: "232.99.99.1",
            destination_port: 5004,
            interface_ip: "",
            advertise_caps: false,
            node_seed: "demo-node1",
        };
        let text = from_caps(&input).expect("synth");
        assert!(
            text.contains("a=rtpmap:96 raw/90000"),
            "RFC 4175 §6.7 spells the encoding name lower-case; \
             nmos-cpp's `get_format` matches `U(\"raw\")` \
             case-sensitively:\n{text}",
        );
        assert!(
            text.contains("PM=2110GPM"),
            "ST 2110-20 §6.3 packing-mode key is upper-case:\n{text}",
        );
        assert!(
            text.contains("SSN=ST2110-20:2017"),
            "ST 2110-20 §6.3 SMPTE-standard-number key is upper-case:\n{text}",
        );
        assert!(
            text.contains("colorimetry=BT709"),
            "colorimetry is REQUIRED by nmos-cpp's `get_video_raw_parameters`; \
             we default to ST 2110 SDR BT709 when essence caps don't supply one:\n{text}",
        );
        assert!(
            text.contains("\r\nt=0 0\r\n"),
            "gst-sdp emits the RFC 4566 default time line when no t= block was added:\n{text}",
        );
    }

    /// Regression: do not call [`SDPMessage::add_time`] in [`build_sdp`].
    /// Passing `repeat: &[]` from Rust can leave the internal time entry in a
    /// shape that makes [`SDPMessage::as_text`] segfault inside libgstsdp.
    /// Serialisation already injects `t=0 0` when the message has no time blocks.
    #[test]
    fn build_sdp_emits_implicit_time_line_without_add_time() {
        init_gst();
        let media = parse_sdp(AUDIO_L24_48K_STEREO_SDP).expect("parse");
        let text = build_sdp(&media, test_session()).expect("build must not crash");
        assert!(
            text.contains("\r\nt=0 0\r\n"),
            "built SDP must carry the default time line:\n{text}",
        );
    }

    /// Exhaustive table-driven pin on every ST 2110 fmtp key the
    /// canonicaliser is meant to preserve through the splice
    /// round-trip. Feeds a hand-crafted SDP with all of
    /// [`ST_2110_UPPERCASE_FMTP_KEYS`] (and the three lower-case
    /// rtpmap encoding names in
    /// [`ST_2110_LOWERCASE_RTPMAP_NAMES`]) on the wire, then
    /// asserts each one survives `parse_sdp` → `build_sdp` with
    /// its canonical case intact. If a future ST 2110 revision
    /// adds a new upper-case fmtp key, extending the table makes
    /// this test pass; nothing else should need editing.
    #[test]
    fn passthrough_preserves_all_st2110_uppercase_fmtp_keys() {
        init_gst();
        // The SDP below intentionally crams every upper-case
        // fmtp key into a single (semantically nonsensical)
        // `a=fmtp:` so we test the canonicaliser, not the spec
        // parser. Values are picked from `nmos-cpp`'s own
        // `sdp_utils_test.cpp` fixtures where possible.
        let raw_sdp = "v=0\r\n\
            o=- 1 0 IN IP4 192.0.2.10\r\n\
            s=video1\r\n\
            t=0 0\r\n\
            m=video 5004 RTP/AVP 96\r\n\
            c=IN IP4 232.99.99.1/64\r\n\
            a=rtpmap:96 raw/90000\r\n\
            a=fmtp:96 \
            sampling=YCbCr-4:2:2;\
            depth=10;\
            width=1920;\
            height=1080;\
            exactframerate=50;\
            colorimetry=BT709;\
            PM=2110BPM;\
            SSN=ST2110-20:2022;\
            TCS=ST2115LOGS3;\
            RANGE=FULLPROTECT;\
            PAR=12:11;\
            MAXUDP=1460;\
            TSMODE=SAMP;\
            TSDELAY=82;\
            TP=2110TPW;\
            TROFF=0;\
            CMAX=42;\
            DID_SDID={0x41,0x01};\
            VPID_Code=133;\
            TM=Async\r\n";
        let spliced =
            passthrough_with_overrides(raw_sdp, &SdpOverrides::default()).expect("splice no-op");
        // rtpmap encoding-name must round-trip lower-case.
        assert!(
            spliced.contains("a=rtpmap:96 raw/90000"),
            "rtpmap encoding-name must survive as lower-case `raw`:\n{spliced}",
        );
        // Pin every entry from the canonical table. Using
        // `key=` (with the `=`) ensures we don't accidentally
        // match a substring of a value
        // (e.g. `RANGE=FULLPROTECT` contains `PR=` if you squint).
        for entry in &*ST_2110_UPPERCASE_FMTP_KEYS {
            let canonical: &'static str = **entry;
            let needle = format!("{canonical}=");
            assert!(
                spliced.contains(&needle),
                "ST 2110 fmtp key `{needle}` must survive splice round-trip \
                 with canonical case:\n{spliced}",
            );
        }
        // And nothing should leak the lower-cased form. `tm=`
        // and `tp=` are too short for a `!spliced.contains`
        // check (they'd match `BPM=`, `2110TPW`, etc.), so pin
        // them with explicit leading-`;` / leading-` ` anchors.
        for &canonical in &["PM", "SSN", "TCS", "RANGE", "PAR", "MAXUDP", "TSMODE",
                             "TSDELAY", "TROFF", "CMAX", "DID_SDID", "VPID_Code"]
        {
            let lower = canonical.to_ascii_lowercase();
            let needle = format!(";{lower}=");
            assert!(
                !spliced.contains(&needle),
                "lower-cased `{lower}=` leaked into splice output:\n{spliced}",
            );
        }
    }

    /// Pin the rtpmap-encoding-name table for the JPEG-XS and
    /// ANC essence shapes. The splice path produces these once
    /// `nmossink`'s caps property accepts `video/x-jxsv` / the
    /// `meta/x-st-2038` shape, but the canonicaliser already has
    /// to do the right thing today because the table lives at
    /// the build-sdp layer.
    #[test]
    fn canonicalise_rtpmap_value_lowercases_jxsv_and_smpte291() {
        // Each input is what gst-sdp's `set_media_from_caps`
        // would produce after `caps_from_media` upper-cased the
        // encoding-name — i.e. the post-splice intermediate. We
        // assert the canonicaliser returns the lower-case form.
        assert_eq!(
            canonicalise_rtpmap_value("96 JXSV/90000").as_deref(),
            Some("96 jxsv/90000"),
        );
        assert_eq!(
            canonicalise_rtpmap_value("100 SMPTE291/90000").as_deref(),
            Some("100 smpte291/90000"),
        );
        // Already canonical → no rewrite (caller skips
        // `replace_attribute`).
        assert!(canonicalise_rtpmap_value("96 raw/90000").is_none());
        // Audio token never matches → no rewrite.
        assert!(canonicalise_rtpmap_value("97 L24/48000/2").is_none());
    }
}
