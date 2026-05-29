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
//! established. The network params (`destination_ip`,
//! `destination_port`, `interface_ip`, `source_ip`, `source_port`)
//! come from the `m=` and `c=` lines and the `a=source-filter:`,
//! `a=x-nvnmos-iface-ip:`, `a=x-nvnmos-src-port:` attributes.
//!
//! Today the module handles single-media SDPs only. ST 2022-7
//! dual-media SDPs are detected and rejected with a clearly-
//! attributed error; redundancy parsing lands when the property
//! surface for the secondary leg is designed.
//!
//! Essence coverage so far:
//!   * Video: RFC 4175 `encoding-name=RAW`,
//!     `sampling=YCbCr-4:2:2` at `depth=10` (→ `format=UYVP`) and
//!     `depth=8` (→ `format=UYVY`); other samplings and bit-depths
//!     (RGB/RGBA/BGR/BGRA, YCbCr-4:4:4, YCbCr-4:2:0, YCbCr-4:1:1)
//!     are not yet handled.
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
//! [`derive_raw_caps_audio`] for the reasoning.


use std::str::FromStr;

use gst_sdp::{SDPMedia, SDPMessage};
use gstreamer as gst;
use gstreamer_sdp as gst_sdp;
use thiserror::Error;

use crate::session::{UdpLeg, UdpMedia};
use crate::types::FlowFormat;

#[derive(Debug, Error)]
pub(crate) enum SdpError {
    #[error("SDP text could not be parsed: {0}")]
    Parse(String),
    #[error("SDP has no media lines")]
    NoMedia,
    #[error("SDP has {0} media lines; multi-leg SDPs (ST 2022-7) are not yet supported")]
    MultipleMedia(usize),
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
         `encoding-name=RAW, sampling=YCbCr-4:2:2` (`depth=10` → \
         `UYVP`, `depth=8` → `UYVY`), ST 2110-30 / RFC 3551 \
         L24 / L16 audio, and ST 2110-40 / RFC 8331 \
         `encoding-name=SMPTE291` ANC are recognised"
    )]
    UnsupportedEssence(String),
    #[error("`set_media_from_caps` rejected the supplied RTP caps: {0}")]
    BuildMediaFromCaps(String),
    #[error("`gst_sdp_message_as_text` failed to serialise the constructed SDP: {0}")]
    Serialise(String),
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
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct SdpSession<'a> {
    /// `o=` line `<unicast-address>` slot (RFC 4566 §5.2). For
    /// multicast Senders this is typically the local interface IP;
    /// `0.0.0.0` is a safe generic default when the interface is
    /// not known yet.
    pub origin_address: &'a str,
    /// `o=` line `<sess-id>` slot (RFC 4566 §5.2). RFC 4566 wants
    /// a non-zero numeric value; the daemon usually wants it
    /// stable per-Sender so successive configuring SDPs handed
    /// over for the same resource compare equal upstream of the
    /// IS-04 publication.
    pub origin_session_id: &'a str,
    /// `s=` line session name (RFC 4566 §5.3). Typically the
    /// Sender's NMOS `label`.
    pub name: &'a str,
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
    // `derive_raw_caps_*` then surfaces the precise reason.
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
        FlowFormat::Video => derive_raw_caps_video(&rtp_caps)?,
        FlowFormat::Audio => derive_raw_caps_audio(&rtp_caps)?,
        FlowFormat::Data => derive_raw_caps_data(&rtp_caps)?,
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
/// * Sender with `transport-file*` plus overriding scalar
///   properties — parse the file with [`parse_sdp`], apply the
///   overrides to the [`UdpMedia`] and the [`SdpSession`], then
///   rebuild with this function to splice them back in (the SDP
///   equivalent of [`crate::flow_def::splice_overrides`]).
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
/// s=<session.name>
/// t=0 0
/// m=<media> <destination_port> RTP/AVP <pt>
/// c=IN IP4 <destination_ip>/64
/// a=rtpmap:<pt> ...      ← from `set_media_from_caps`
/// a=fmtp:<pt> ...        ← from `set_media_from_caps`
/// a=source-filter: incl IN IP4 <destination_ip> <source_ip>   ← if source_ip
/// a=x-nvnmos-iface-ip:<interface_ip>                          ← if interface_ip
/// a=x-nvnmos-src-port:<source_port>                           ← if source_port
/// ```
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_sdp(media: &UdpMedia, session: SdpSession<'_>) -> Result<String, SdpError> {
    let mut msg = SDPMessage::new();
    msg.set_version("0");
    msg.set_session_name(session.name);
    msg.set_origin(
        "nvnmos",
        session.origin_session_id,
        "0",
        "IN",
        "IP4",
        session.origin_address,
    );

    let leg = &media.primary;
    let mut m = SDPMedia::new();
    m.set_proto("RTP/AVP");
    m.set_port_info(u32::from(leg.destination_port), 1);
    // ttl=64 matches `nvds_nmos_bin::sdp_from_caps`; correct for
    // typical multicast Senders. Unicast Senders strictly should
    // omit the ttl suffix per RFC 4566 but the cpp reference also
    // emits it unconditionally and downstream parsers tolerate it.
    m.add_connection("IN", "IP4", &leg.destination_ip, 64, 0);
    m.set_media_from_caps(&media.rtp_caps)
        .map_err(|e| SdpError::BuildMediaFromCaps(e.to_string()))?;

    if let Some(src) = leg.source_ip.as_deref() {
        let value = format!(" incl IN IP4 {dest} {src}", dest = leg.destination_ip);
        m.add_attribute("source-filter", Some(&value));
    }
    if let Some(iface) = leg.interface_ip.as_deref() {
        m.add_attribute("x-nvnmos-iface-ip", Some(iface));
    }
    if let Some(port) = leg.source_port {
        m.add_attribute("x-nvnmos-src-port", Some(&port.to_string()));
    }

    msg.add_media(m);

    msg.as_text().map_err(|e| SdpError::Serialise(e.to_string()))
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
/// `derive_raw_caps_video`'s docstring for the rationale (V2
/// doesn't read `RANGE` either, so plumbing it asymmetrically
/// via the V1 capssetter would diverge V1/V2 output).
fn colorimetry_for_caps(sdp: &str, depth: u32, tcs: Option<&str>) -> Option<&'static str> {
    match sdp.to_ascii_uppercase().as_str() {
        "BT601-5" | "BT601" => Some("bt601"),
        "BT709-2" | "BT709" => Some("bt709"),
        "SMPTE240M" => Some("smpte240m"),
        "BT2020" if depth >= 10 => Some("bt2020-10"),
        "BT2020" => Some("bt2020"),
        "BT2100" => match tcs.map(|t| t.to_ascii_uppercase()).as_deref() {
            Some("HLG") => Some("bt2100-hlg"),
            // `PQ`, missing, or any unknown TCS — assume PQ;
            // see the docstring's note on the V2 typo.
            _ => Some("bt2100-pq"),
        },
        _ => None,
    }
}

/// Derive `video/x-raw,...` caps from an `application/x-rtp,...`
/// caps that describes an RFC 4175 video media.
///
/// Currently handles `encoding-name=RAW` with the YCbCr-4:2:2
/// samplings the `rtpvrawpay` / `rtpvrawdepay` wire format exposes:
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
///     for `BT2100`) via [`colorimetry_for_caps`]. Required for
///     correctness on the V1 receiver path where the depayloader
///     ignores SDP colorimetry; see [`colorimetry_for_caps`]'s
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
fn derive_raw_caps_video(rtp_caps: &gst::Caps) -> Result<gst::Caps, SdpError> {
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
    let colorimetry_caps = colorimetry_for_caps(
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
fn derive_raw_caps_audio(rtp_caps: &gst::Caps) -> Result<gst::Caps, SdpError> {
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
fn derive_raw_caps_data(rtp_caps: &gst::Caps) -> Result<gst::Caps, SdpError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn init_gst() {
        let _ = gst::init();
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
    /// than calling `colorimetry_for_caps` directly; this keeps the
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
        // `YCbCr-4:2:2` samplings `derive_raw_caps_video` accepts
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
            name: "test session",
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
}
