// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! UDP/RTP transport session setup and activation.

use anyhow::Context;
use gstreamer as gst;

use super::{
    CommonSettings, InnerConfig, Side, TransportConfig,
};
use crate::sdp::{self, SdpOverrides};
use crate::types::{CapsMode, FlowFormat};

/// Which factory family to use for the UDP socket and RTP
/// (de)payloader elements.
///
/// `V1` is gst-plugins-good throughout: `udpsrc` / `udpsink` /
/// `rtpvrawpay` / `rtpL24pay` / etc. `V2` prefers gst-plugins-rs
/// (`udpsrc2`, `rtpL24pay2`, `rtpL24depay2`, …) on a per-element
/// basis and falls back to the V1 factory for any element that
/// doesn't yet have a V2 sibling. Per-element fallback (rather
/// than all-or-nothing) matters because the V2 family is rolled
/// out incrementally upstream — for example today
/// gst-plugins-rs ships `rtpL24pay2`/`depay2` but not yet
/// `rtpvrawpay2`/`depay2`, and no `udpsink2` exists at all (the
/// performance motivation for `udpsrc2` was kernel receive
/// efficiency, which doesn't translate to the send side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UdpVariant {
    V1,
    V2,
}

/// One RTP media line's worth of state — everything the UDP chain
/// factories need to instantiate the inner elements.
///
/// Essence-level state (`format`, `rtp_caps`, `raw_caps`) is shared
/// across legs because both legs of an ST 2022-7 pair carry the
/// same essence with the same PT / clock-rate / encoding-name; only
/// the network params differ. Per-leg state lives on [`UdpLeg`].
///
/// Field names use NMOS / IS-05 terminology (`destination_ip`,
/// `interface_ip`, `source_ip`, ...) for direction independence;
/// the public element properties on `nmossrc` / `nmossink` use the
/// IS-05 RTP transport_params vocabulary verbatim (`source-ip`,
/// `source-port`, `destination-ip`, `destination-port`,
/// `interface-ip`, `multicast-ip`), mapped onto these per-leg
/// fields at property-set / SDP-splice time — see
/// [`CommonSettings::source_ip`] et seq. for the per-side wire
/// semantics. The mapping is 1:1 to IS-05 wire JSON, so a
/// controller PATCHing `/single/senders/{id}/staged` reads
/// straight into the same GObject property names. How the
/// redundant secondary leg gets exposed on the property surface
/// is a separate design decision — `nvdsudpsrc` for example
/// overloads `local-iface-ip` into a comma-separated list and
/// adds a combined `st2022-7-streams` property rather than
/// `-2`-suffixed scalar twins — and is deferred until the
/// redundancy work lands.
#[derive(Debug, Clone)]
pub(crate) struct UdpMedia {
    /// Essence family — selects the payloader / depayloader factory
    /// alongside [`UdpVariant`].
    pub(crate) format: FlowFormat,
    /// First (and, for non-redundant RTP, only) leg.
    pub(crate) primary: UdpLeg,
    /// Redundant secondary leg for ST 2022-7. `None` for
    /// non-redundant RTP — which is everything today, until the
    /// 2022-7 work lands.
    pub(crate) secondary: Option<UdpLeg>,
    /// `application/x-rtp,...` caps the depayloader consumes (and
    /// the payloader produces). Carries PT, clock-rate,
    /// encoding-name, channels, sampling, depth and any other
    /// essence-specific RFC 4175 / RFC 3551 / RFC 3190 parameters
    /// that `a=rtpmap` / `a=fmtp` map to. `a=ptime:` / `a=maxptime:`
    /// are hoisted onto these caps as `a-ptime` / `a-maxptime`
    /// (the GStreamer convention `SDPMedia::set_media_from_caps`
    /// rebuilds into standalone `a=…:` SDP attributes). The
    /// payloader / depayloader and the chain factories
    /// ([`crate::inner::build_udpsink`] et al) read this field
    /// directly.
    pub(crate) rtp_caps: gst::Caps,
    /// Essence caps (`video/x-raw,…`, `audio/x-raw,…`,
    /// `meta/x-st-2038,…`). The receiver pins these on its ghost
    /// src pad so downstream caps queries see the concrete shape
    /// the flow will carry, mirroring the MXL path's
    /// `advertise_caps` derived from the flow_def.
    pub(crate) raw_caps: gst::Caps,
}

/// One network leg of a [`UdpMedia`]. Non-redundant RTP has a single
/// leg ([`UdpMedia::primary`]); ST 2022-7 adds a second
/// ([`UdpMedia::secondary`]) carrying the same essence over an
/// independent network path for hitless merging.
///
/// All fields are per-leg state that NMOS IS-05's
/// `transport_params` carries one-for-one (with `source_ip`
/// modelled as the NMOS-simplified single-entry equivalent of the
/// SDP `a=source-filter:` include list — see field doc).
#[derive(Debug, Clone)]
pub(crate) struct UdpLeg {
    /// Multicast group (or unicast destination). Sender's
    /// `udpsink.host` / receiver's `udpsrc.address`.
    pub(crate) destination_ip: String,
    /// Sender's `udpsink.port` / receiver's `udpsrc.port`.
    pub(crate) destination_port: u16,
    /// Local interface IP. Nvds elements take this directly as
    /// `local-iface-ip`; for `udpsrc` / `udpsink` we resolve to an
    /// interface name and forward as `multicast-iface`.
    pub(crate) interface_ip: Option<String>,
    /// SSM source-IP filter. Receiver-only. NMOS-RTP
    /// `transport_params[i].source_ip` is a single string by
    /// design — the SDP `a=source-filter:` line supports list /
    /// exclude semantics but NMOS constrains itself to one
    /// include-mode source per leg. We forward this directly to
    /// `nvdsudpsrc.source-address`; on the gst-plugins-good
    /// `udpsrc` path it's advertised in NMOS but not currently
    /// enforced at the socket (no native source-filter property).
    pub(crate) source_ip: Option<String>,
    /// Sender source port. Forwarded as `udpsink.bind-port`.
    /// Sender-only.
    pub(crate) source_port: Option<u16>,
}
pub(super) fn synthesise_or_passthrough_udp(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    resolved: Option<String>,
) -> Result<Option<String>, anyhow::Error> {
    match (resolved, settings.caps.as_ref()) {
        (Some(text), Some(_)) => {
            gst::debug!(
                cat,
                "{element}: SDP transport-file set; `caps` will be cross-checked against the file's essence shape",
            );
            Ok(Some(text))
        }
        (Some(text), None) => Ok(Some(text)),
        (None, Some(essence_caps)) => {
            // Per-side dispatch on the IS-05 destination slot:
            // Senders carry it as `destination_ip`, Receivers
            // as `multicast_ip`. The single wire slot on the
            // SDP `m=` / `c=` line is named `destination_ip`
            // on `sdp::SdpBuildInput`.
            let destination_ip = match settings.side {
                Side::Sender => settings.destination_ip.as_str(),
                Side::Receiver => settings.multicast_ip.as_str(),
            };
            let advertise_caps = match settings.caps_mode {
                // Auto resolves to narrow for synthesised
                // SDPs — the splice path can promote to wide
                // later if `receiver-caps-mode=Wide` is set.
                // (For synthesis we always know
                // `caps_mode` directly, so we can apply it
                // here without going through the splice's
                // text-rewrite.)
                CapsMode::Auto | CapsMode::Narrow => false,
                CapsMode::Wide => true,
            };
            let input = sdp::SdpBuildInput {
                essence_caps,
                transport_caps: settings.transport_caps.as_ref(),
                side: settings.side,
                label: &settings.label,
                description: &settings.description,
                name: &settings.name,
                source_ip: &settings.source_ip,
                source_port: settings.source_port,
                destination_ip,
                destination_port: settings.destination_port,
                interface_ip: &settings.interface_ip,
                advertise_caps,
                node_seed: &settings.node_seed,
            };
            let text = sdp::from_caps(&input)
                .with_context(|| format!("{element}: synthesising SDP from caps"))?;
            gst::info!(
                cat,
                "{element}: synthesised SDP from `caps` (side={:?})",
                settings.side,
            );
            Ok(Some(text))
        }
        (None, None) => Ok(None),
    }
}
pub(crate) fn property_overrides_udp(settings: &CommonSettings) -> SdpOverrides<'_> {
    fn opt(s: &str) -> Option<&str> {
        if s.is_empty() { None } else { Some(s) }
    }
    fn opt_port(p: u16) -> Option<u16> {
        if p == 0 { None } else { Some(p) }
    }
    let (source_ip, interface_ip, destination_ip, source_port) = match settings.side {
        Side::Sender => (
            opt(&settings.source_ip),
            // Sender duplicates source_ip into the iface-ip slot
            // — see the per-side dispatch note above.
            opt(&settings.source_ip),
            opt(&settings.destination_ip),
            opt_port(settings.source_port),
        ),
        Side::Receiver => (
            opt(&settings.source_ip),
            opt(&settings.interface_ip),
            opt(&settings.multicast_ip),
            None,
        ),
    };
    let tc = settings
        .transport_caps
        .as_ref()
        .and_then(|c| c.structure(0));
    // pt is i32 on `application/x-rtp` caps per GStreamer
    // convention; cast to u8 for the [`SdpOverrides`] slot.
    // 0..=255 keeps the cast lossless and lets the
    // RFC-3551-range check fires centrally in
    // `sdp::passthrough_with_overrides`.
    let payload_type = tc
        .and_then(|s| s.get::<i32>("payload").ok())
        .and_then(|pt| u8::try_from(pt).ok());
    let audio_clock_rate = tc
        .and_then(|s| s.get::<i32>("clock-rate").ok())
        .and_then(|rate| u32::try_from(rate).ok());
    let a_ptime = tc.and_then(|s| s.get::<&str>("a-ptime").ok());
    let a_maxptime = tc.and_then(|s| s.get::<&str>("a-maxptime").ok());

    SdpOverrides {
        label: opt(&settings.label),
        description: opt(&settings.description),
        name: opt(&settings.name),
        interface_ip,
        destination_ip,
        destination_port: opt_port(settings.destination_port),
        source_ip,
        source_port,
        payload_type,
        audio_clock_rate,
        a_ptime,
        a_maxptime,
        caps_mode: settings.caps_mode,
    }
}
pub(crate) fn decide_inner_config_udp(
    element: &str,
    settings: &CommonSettings,
    variant: UdpVariant,
    transport_file: Option<&str>,
    cross_check_mode: sdp::EssenceCrossCheckMode,
) -> Result<InnerConfig, anyhow::Error> {
    let Some(text) = transport_file else {
        let reason = match settings.side {
            Side::Sender => {
                "no SDP transport file; waiting for IS-05 PATCH to supply the destination address"
                    .to_owned()
            }
            Side::Receiver => {
                "no SDP transport file; waiting for IS-05 PATCH to supply the listen address"
                    .to_owned()
            }
        };
        return Ok(InnerConfig::Fake { reason });
    };
    let media = sdp::parse_sdp(text).with_context(|| {
        format!(
            "{element}: parsing SDP transport file for transport={:?}",
            settings.transport
        )
    })?;
    // Cross-check the parsed SDP against the user-supplied
    // `caps` (essence shape) and `transport-caps` (RTP-layer
    // hints). The check fires after property overrides have
    // applied the override-class fields, so an audio
    // clock-rate that the user asked us to write into the
    // SDP is implicit-OK while a video clock-rate disagreement
    // (where clock-rate is cross-check, not override) surfaces
    // as `SdpError::TransportCapsMismatch`. Mirrors
    // `decide_inner_config_mxl`'s `resolve_mxl_flow_meta`
    // cross-check pass.
    sdp::cross_check_essence(
        &media,
        settings.caps.as_ref(),
        settings.transport_caps.as_ref(),
        cross_check_mode,
    )
    .with_context(|| {
            format!(
                "{element}: cross-checking SDP against `caps` / `transport-caps` \
                 for transport={:?}",
                settings.transport
            )
        })?;
    Ok(InnerConfig::Real(TransportConfig::Udp {
        variant,
        media,
        transport_file: Some(text.to_owned()),
    }))
}

/// Cross-check strictness for [`decide_inner_config_udp`]: wide
/// receivers (activation SDP carries `a=x-nvnmos-caps:`) relax
/// essence shape on activation only.
pub(crate) fn udp_essence_cross_check_mode(
    settings: &CommonSettings,
    activation: bool,
    transport_file: Option<&str>,
) -> sdp::EssenceCrossCheckMode {
    if activation
        && settings.side == Side::Receiver
        && transport_file.is_some_and(sdp::indicates_wide_receiver_caps)
    {
        sdp::EssenceCrossCheckMode::FormatFamilyOnly
    } else {
        sdp::EssenceCrossCheckMode::Full
    }
}
pub(super) fn resolve_inner_config_udp(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    variant: UdpVariant,
    resolved_transport_file: Option<String>,
) -> Result<(InnerConfig, Option<String>), anyhow::Error> {
    // Synthesise an SDP from caps when no transport-file*
    // is supplied; pass through the transport-file* otherwise.
    // Mirrors `resolve_inner_config_mxl`'s
    // `synthesise_or_passthrough_mxl` call.
    let had_user_transport_file = resolved_transport_file.is_some();
    let resolved_transport_file =
        synthesise_or_passthrough_udp(cat, element, settings, resolved_transport_file)?;

    // Property-overrides passthrough: rewrite any user-set
    // identity / cosmetic / network properties (label,
    // description, name, IS-05 endpoints, caps_mode) into
    // user-supplied transport files before the daemon sees them.
    // Mirrors `resolve_inner_config_mxl`'s
    // `flow_def::splice_overrides` call. Activation-time SDP stays
    // authoritative (see `make_activation_plan`) — the passthrough
    // runs at startup only. Synthesised SDPs already bake every
    // property in via [`sdp::from_caps`]; skip the second pass.
    let resolved_transport_file = match resolved_transport_file {
        Some(text) if had_user_transport_file => Some(
            sdp::passthrough_with_overrides(&text, &property_overrides_udp(settings))
                .with_context(|| {
                    format!("{element}: applying property overrides to transport-file SDP")
                })?,
        ),
        other => other,
    };
    let inner = decide_inner_config_udp(
        element,
        settings,
        variant,
        resolved_transport_file.as_deref(),
        sdp::EssenceCrossCheckMode::Full,
    )?;
    Ok((inner, resolved_transport_file))
}
