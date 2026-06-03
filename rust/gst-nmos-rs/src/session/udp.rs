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

#[cfg(test)]
mod tests {
    use super::super::support::*;
    use super::super::*;
    use super::*;
    use crate::sdp;
    use std::str::FromStr;

    /// Representative [`UdpMedia`] for tests that exercise the
    /// `TransportConfig::Udp` dispatch arms without going through
    /// the SDP parsing layer. Single-leg; all optional fields
    /// populated so accessor-style assertions can see them.
    fn sample_udp_media() -> UdpMedia {
        use std::str::FromStr;
        cat(); // ensures gst::init() ran
        UdpMedia {
            format: FlowFormat::Video,
            primary: UdpLeg {
                destination_ip: "239.1.1.1".to_owned(),
                destination_port: 5004,
                interface_ip: Some("192.0.2.10".to_owned()),
                source_ip: Some("192.0.2.20".to_owned()),
                source_port: Some(5004),
            },
            secondary: None,
            rtp_caps: gst::Caps::from_str(
                "application/x-rtp,media=video,clock-rate=90000,encoding-name=RAW,payload=96",
            )
            .expect("static rtp caps parse"),
            raw_caps: gst::Caps::from_str(
                "video/x-raw,format=v210,width=1920,height=1080,framerate=50/1",
            )
            .expect("static raw caps parse"),
        }
    }

    mod transport_config {
        use super::*;

        #[test]
        fn udp_media_fixture_has_expected_fields() {
            let m = sample_udp_media();
            assert_eq!(m.format, FlowFormat::Video);
            assert_eq!(m.primary.destination_ip, "239.1.1.1");
            assert_eq!(m.primary.destination_port, 5004);
            assert_eq!(m.primary.interface_ip.as_deref(), Some("192.0.2.10"));
            assert_eq!(m.primary.source_ip.as_deref(), Some("192.0.2.20"));
            assert_eq!(m.primary.source_port, Some(5004));
            assert!(m.secondary.is_none());
            assert!(!m.rtp_caps.is_empty());
            assert!(!m.raw_caps.is_empty());
        }

        #[test]
        fn transport_file_udp_v1_present() {
            let tc = TransportConfig::Udp {
                variant: UdpVariant::V1,
                media: sample_udp_media(),
                transport_file: Some("payload".to_owned()),
            };
            assert_eq!(tc.transport_file(), Some("payload"));
        }

        #[test]
        fn transport_file_udp_v2_absent() {
            let tc = TransportConfig::Udp {
                variant: UdpVariant::V2,
                media: sample_udp_media(),
                transport_file: None,
            };
            assert_eq!(tc.transport_file(), None);
        }
    }

    mod udp_dispatch {
        use super::*;

        /// Minimal valid UDP-RTP SDP for the dispatch tests. The
        /// detailed coverage of `parse_sdp`'s essence-mapping lives
        /// in `crate::sdp::tests`; here we just need *something*
        /// the SDP module accepts so the dispatch returns
        /// `InnerConfig::Real(TransportConfig::Udp)`.
        const VIDEO_UDP_SDP: &str = concat!(
            "v=0\r\n",
            "o=- 1 0 IN IP4 192.0.2.10\r\n",
            "s=test\r\n",
            "t=0 0\r\n",
            "m=video 5004 RTP/AVP 96\r\n",
            "c=IN IP4 239.1.1.1/64\r\n",
            "a=rtpmap:96 raw/90000\r\n",
            "a=fmtp:96 sampling=YCbCr-4:2:2; width=1920; height=1080;",
            " exactframerate=50; depth=10\r\n",
        );

        fn udp_settings(side: Side, transport: Transport) -> CommonSettings {
            cat(); // ensures gst::init() ran for parse_sdp
            CommonSettings {
                transport,
                ..settings(side)
            }
        }

        #[test]
        fn decide_udp_v1_with_valid_sdp_is_real() {
            let s = udp_settings(Side::Receiver, Transport::Udp);
            let inner =
                decide_inner_config_udp(
                    "nmossrc",
                    &s,
                    UdpVariant::V1,
                    Some(VIDEO_UDP_SDP),
                    sdp::EssenceCrossCheckMode::Full,
                )
                .expect("valid SDP parses");
            match inner {
                InnerConfig::Real(TransportConfig::Udp {
                    variant,
                    media,
                    transport_file,
                }) => {
                    assert_eq!(variant, UdpVariant::V1);
                    assert_eq!(media.format, FlowFormat::Video);
                    assert_eq!(media.primary.destination_ip, "239.1.1.1");
                    assert_eq!(media.primary.destination_port, 5004);
                    assert_eq!(
                        transport_file.as_deref(),
                        Some(VIDEO_UDP_SDP),
                        "transport_file must be threaded into the resolved config",
                    );
                }
                other => panic!("expected Real(Udp), got {other:?}"),
            }
        }

        #[test]
        fn decide_udp_v2_picks_udp2_variant() {
            let s = udp_settings(Side::Sender, Transport::Udp2);
            let inner =
                decide_inner_config_udp(
                    "nmossink",
                    &s,
                    UdpVariant::V2,
                    Some(VIDEO_UDP_SDP),
                    sdp::EssenceCrossCheckMode::Full,
                )
                .expect("valid SDP parses");
            match inner {
                InnerConfig::Real(TransportConfig::Udp { variant, .. }) => {
                    assert_eq!(variant, UdpVariant::V2);
                }
                other => panic!("expected Real(Udp, V2), got {other:?}"),
            }
        }

        #[test]
        fn decide_udp_without_transport_file_is_fake_deferred() {
            for side in [Side::Sender, Side::Receiver] {
                let s = udp_settings(side, Transport::Udp);
                let inner =
                    decide_inner_config_udp(
                        "nmossrc",
                        &s,
                        UdpVariant::V1,
                        None,
                        sdp::EssenceCrossCheckMode::Full,
                    )
                    .expect("None transport_file is not an error");
                match inner {
                    InnerConfig::Fake { reason } => {
                        assert!(
                            reason.contains("no SDP transport file"),
                            "expected no-SDP reason for {side:?}: {reason}",
                        );
                        assert!(
                            reason.contains("IS-05 PATCH"),
                            "expected IS-05 PATCH hint for {side:?}: {reason}",
                        );
                    }
                    other => panic!("expected Fake for {side:?}, got {other:?}"),
                }
            }
        }

        #[test]
        fn decide_udp_with_malformed_sdp_attributes_error() {
            let s = udp_settings(Side::Receiver, Transport::Udp);
            let err =
                decide_inner_config_udp(
                    "nmossrc",
                    &s,
                    UdpVariant::V1,
                    Some("garbage"),
                    sdp::EssenceCrossCheckMode::Full,
                )
                .expect_err("malformed SDP must error");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("nmossrc"),
                "error must attribute the element name: {msg}",
            );
            assert!(
                msg.contains("parsing SDP transport file"),
                "error must mention SDP parsing: {msg}",
            );
        }

        #[test]
        fn activation_udp_happy_path_is_real_success() {
            let s = udp_settings(Side::Receiver, Transport::Udp);
            let plan = make_activation_plan(
                &cat(),
                "nmossrc",
                &s,
                &req(Side::Receiver, Some(VIDEO_UDP_SDP)),
            );
            match plan.inner {
                InnerConfig::Real(TransportConfig::Udp { variant, media, .. }) => {
                    assert_eq!(variant, UdpVariant::V1);
                    assert_eq!(media.format, FlowFormat::Video);
                }
                other => panic!("expected Real(Udp), got {other:?}"),
            }
            assert!(matches!(plan.ack, ActivationAck::Success));
        }

        #[test]
        fn activation_udp2_happy_path_is_real_success() {
            let s = udp_settings(Side::Sender, Transport::Udp2);
            let plan = make_activation_plan(
                &cat(),
                "nmossink",
                &s,
                &req(Side::Sender, Some(VIDEO_UDP_SDP)),
            );
            match plan.inner {
                InnerConfig::Real(TransportConfig::Udp { variant, .. }) => {
                    assert_eq!(variant, UdpVariant::V2);
                }
                other => panic!("expected Real(Udp, V2), got {other:?}"),
            }
            assert!(matches!(plan.ack, ActivationAck::Success));
        }

        #[test]
        fn activation_udp_malformed_sdp_is_failure() {
            let s = udp_settings(Side::Receiver, Transport::Udp);
            let plan = make_activation_plan(
                &cat(),
                "nmossrc",
                &s,
                &req(Side::Receiver, Some("garbage")),
            );
            assert!(matches!(plan.inner, InnerConfig::Fake { .. }));
            match plan.ack {
                ActivationAck::Failure { reason } => assert!(
                    reason.contains("parsing activation SDP"),
                    "expected SDP-parse attribution: {reason}",
                ),
                ActivationAck::Success => panic!("expected Failure ack on malformed SDP"),
            }
        }

        // -- property_overrides_udp builder ------------------------

        /// Pure-function: a Sender's `source_ip` populates BOTH
        /// `SdpOverrides.source_ip` (SDP `a=source-filter:` SSM
        /// include-source) AND `SdpOverrides.interface_ip` (SDP
        /// `a=x-nvnmos-iface-ip:`). See `property_overrides_udp`'s
        /// doc for the per-side dispatch rationale.
        #[test]
        fn property_overrides_udp_sender_duplicates_source_ip_into_iface_ip() {
            let s = CommonSettings {
                side: Side::Sender,
                source_ip: "192.0.2.10".to_owned(),
                source_port: 5005,
                destination_ip: "239.1.1.1".to_owned(),
                destination_port: 5004,
                // Receiver-only slots are populated but must be
                // ignored on the Sender side.
                interface_ip: "should-not-leak.example".to_owned(),
                multicast_ip: "should-not-leak.example".to_owned(),
                ..udp_settings(Side::Sender, Transport::Udp)
            };
            let o = property_overrides_udp(&s);
            assert_eq!(o.source_ip, Some("192.0.2.10"));
            assert_eq!(o.interface_ip, Some("192.0.2.10"));
            assert_eq!(o.source_port, Some(5005));
            assert_eq!(o.destination_ip, Some("239.1.1.1"));
            assert_eq!(o.destination_port, Some(5004));
        }

        /// Pure-function: a Receiver's `multicast_ip` populates
        /// `SdpOverrides.destination_ip` (the SDP `c=` line wire
        /// slot, which IS-05 splits between sender's
        /// `destination_ip` and receiver's `multicast_ip` by
        /// resource direction). `source_port` is forced to
        /// `None` because the IS-05 receiver schema doesn't
        /// define that slot.
        #[test]
        fn property_overrides_udp_receiver_maps_multicast_ip_to_destination_ip() {
            let s = CommonSettings {
                side: Side::Receiver,
                source_ip: "192.0.2.20".to_owned(),
                interface_ip: "192.0.2.30".to_owned(),
                multicast_ip: "239.1.1.1".to_owned(),
                destination_port: 5004,
                // Sender-only slot — must be ignored on the
                // Receiver side.
                destination_ip: "should-not-leak.example".to_owned(),
                source_port: 9999,
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let o = property_overrides_udp(&s);
            assert_eq!(o.source_ip, Some("192.0.2.20"));
            assert_eq!(o.interface_ip, Some("192.0.2.30"));
            assert_eq!(o.destination_ip, Some("239.1.1.1"));
            assert_eq!(o.destination_port, Some(5004));
            assert_eq!(
                o.source_port, None,
                "IS-05 receiver schema has no source-port slot",
            );
        }

        /// All slots `None` when no property is set. Pins that
        /// the empty-string / zero "unset" sentinel convention
        /// flows through to the splice helper as "leave the
        /// file's value alone". The shared `settings()` fixture
        /// pre-fills `name` for IS-04 registration coverage; we
        /// clear it here together with the other identity /
        /// network fields so the test asserts on the splice
        /// builder's behaviour, not the fixture's defaults.
        #[test]
        fn property_overrides_udp_default_settings_are_all_none() {
            for side in [Side::Sender, Side::Receiver] {
                let s = CommonSettings {
                    name: String::new(),
                    label: String::new(),
                    description: String::new(),
                    source_ip: String::new(),
                    source_port: 0,
                    destination_ip: String::new(),
                    destination_port: 0,
                    interface_ip: String::new(),
                    multicast_ip: String::new(),
                    ..udp_settings(side, Transport::Udp)
                };
                let o = property_overrides_udp(&s);
                assert_eq!(o.label, None, "{side:?}");
                assert_eq!(o.description, None, "{side:?}");
                assert_eq!(o.name, None, "{side:?}");
                assert_eq!(o.interface_ip, None, "{side:?}");
                assert_eq!(o.destination_ip, None, "{side:?}");
                assert_eq!(o.destination_port, None, "{side:?}");
                assert_eq!(o.source_ip, None, "{side:?}");
                assert_eq!(o.source_port, None, "{side:?}");
            }
        }

        // -- resolve_inner_config_udp end-to-end -------------------

        /// Round-trip: with a baseline SDP in `transport_file`
        /// and property overrides set, `resolve_inner_config_udp`
        /// must return the spliced text (the second tuple
        /// element) **and** the spliced `UdpMedia` inside
        /// `InnerConfig::Real(TransportConfig::Udp)`. Mirrors
        /// the MXL `resolve_inner_config_mxl` →
        /// `flow_def::splice_overrides` end-to-end story.
        #[test]
        fn resolve_inner_config_udp_applies_property_overrides_to_transport_file() {
            let s = CommonSettings {
                side: Side::Receiver,
                // Override the c= line address + m= port +
                // session `s=` (label) + session `i=`
                // (description) + session `a=x-nvnmos-name`.
                multicast_ip: "232.0.0.1".to_owned(),
                interface_ip: "192.0.2.30".to_owned(),
                source_ip: "192.0.2.20".to_owned(),
                destination_port: 5008,
                label: "Spliced label".to_owned(),
                description: "Spliced description".to_owned(),
                name: "spliced-name".to_owned(),
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let (inner, spliced_text) = resolve_inner_config_udp(
                &cat(),
                "nmossrc",
                &s,
                UdpVariant::V1,
                Some(VIDEO_UDP_SDP.to_owned()),
            )
            .expect("splice + decide must succeed");

            // The returned transport_file text carries the
            // overrides.
            let spliced = spliced_text.expect("transport_file must be Some after splice");
            assert!(spliced.contains("c=IN IP4 232.0.0.1"),
                "c= must be overridden to multicast_ip 232.0.0.1; got: {spliced}");
            assert!(spliced.contains("m=video 5008"),
                "m= port must be overridden to 5008; got: {spliced}");
            assert!(spliced.contains("s=Spliced label\r\n"),
                "s= must be overridden to label; got: {spliced}");
            assert!(spliced.contains("i=Spliced description\r\n"),
                "i= must be overridden to description; got: {spliced}");
            assert!(spliced.contains("a=x-nvnmos-name:spliced-name\r\n"),
                "session-level a=x-nvnmos-name must carry overridden name; got: {spliced}");
            assert!(spliced.contains("a=x-nvnmos-iface-ip:192.0.2.30"),
                "a=x-nvnmos-iface-ip must carry receiver's interface_ip; got: {spliced}");

            // The Real(Udp) inner config carries the spliced
            // UdpMedia (same source of truth).
            match inner {
                InnerConfig::Real(TransportConfig::Udp { media, transport_file, .. }) => {
                    assert_eq!(media.primary.destination_ip, "232.0.0.1");
                    assert_eq!(media.primary.destination_port, 5008);
                    assert_eq!(media.primary.source_ip.as_deref(), Some("192.0.2.20"));
                    assert_eq!(media.primary.interface_ip.as_deref(), Some("192.0.2.30"));
                    assert_eq!(
                        transport_file.as_deref(),
                        Some(spliced.as_str()),
                        "TransportConfig::Udp.transport_file must be the spliced text",
                    );
                }
                other => panic!("expected Real(Udp), got {other:?}"),
            }
        }

        /// No `transport_file` and no `caps` → neither synthesis
        /// nor splice fires; the deferred-fake path is preserved
        /// for the "wait for IS-05 PATCH to provide everything"
        /// case.
        #[test]
        fn resolve_inner_config_udp_no_transport_file_and_no_caps_remains_fake() {
            let s = CommonSettings {
                // IS-05 endpoint property set but no caps and no
                // transport file — nothing to synthesise from.
                multicast_ip: "232.0.0.1".to_owned(),
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let (inner, text) = resolve_inner_config_udp(
                &cat(),
                "nmossrc",
                &s,
                UdpVariant::V1,
                None,
            )
            .expect("no error");
            assert!(text.is_none(), "no input → no synth, no spliced output");
            assert!(matches!(inner, InnerConfig::Fake { .. }));
        }

        /// `caps` supplied but no transport_file →
        /// `synthesise_or_passthrough_udp` builds an SDP from
        /// caps + transport_caps + IS-05 endpoint properties.
        /// The resolved config is now `Real`, not `Fake`.
        #[test]
        fn resolve_inner_config_udp_synthesises_sdp_from_caps_only() {
            let essence = gst::Caps::from_str(
                "audio/x-raw,format=S24BE,rate=48000,channels=2,layout=interleaved",
            )
            .unwrap();
            let s = CommonSettings {
                caps: Some(essence),
                multicast_ip: "232.99.99.1".to_owned(),
                destination_port: 5004,
                interface_ip: "192.0.2.30".to_owned(),
                source_ip: "192.0.2.20".to_owned(),
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let (inner, text) = resolve_inner_config_udp(
                &cat(),
                "nmossrc",
                &s,
                UdpVariant::V1,
                None,
            )
            .expect("synth + splice + decide must succeed");
            let text = text.expect("synthesised SDP must be returned");
            assert!(text.contains("m=audio 5004 RTP/AVP 97"), "synthesised SDP:\n{text}");
            assert!(text.contains("a=rtpmap:97 L24/48000/2"), "rtpmap:\n{text}");
            assert!(text.contains("c=IN IP4 232.99.99.1/"), "multicast c=:\n{text}");
            assert!(
                text.contains("a=x-nvnmos-iface-ip:192.0.2.30"),
                "Receiver iface-ip:\n{text}",
            );
            match inner {
                InnerConfig::Real(TransportConfig::Udp { media, .. }) => {
                    assert_eq!(media.format, FlowFormat::Audio);
                    assert_eq!(media.primary.destination_ip, "232.99.99.1");
                    assert_eq!(media.primary.destination_port, 5004);
                }
                other => panic!("expected Real(Udp) from caps-only synthesis, got {other:?}"),
            }
        }

        /// Sender-side caps-only synthesis exercises the
        /// per-side dispatch: `destination_ip` flows from
        /// `settings.destination_ip` (not `multicast_ip`) and
        /// `source_ip` duplicates into the SDP's
        /// `a=x-nvnmos-iface-ip` slot via
        /// `udp_leg_from_input`.
        #[test]
        fn resolve_inner_config_udp_sender_caps_only_synthesis() {
            let essence = gst::Caps::from_str(
                "video/x-raw,format=UYVP,width=1920,height=1080,\
                 framerate=50/1,interlace-mode=progressive",
            )
            .unwrap();
            let s = CommonSettings {
                caps: Some(essence),
                destination_ip: "239.99.99.1".to_owned(),
                destination_port: 5008,
                source_ip: "192.0.2.10".to_owned(),
                source_port: 5008,
                ..udp_settings(Side::Sender, Transport::Udp)
            };
            let (inner, text) = resolve_inner_config_udp(
                &cat(),
                "nmossink",
                &s,
                UdpVariant::V1,
                None,
            )
            .expect("synth + splice + decide must succeed");
            let text = text.expect("synthesised SDP");
            assert!(text.contains("m=video 5008 RTP/AVP 96"), "Sender SDP:\n{text}");
            assert!(text.contains("c=IN IP4 239.99.99.1/"), "Sender c=:\n{text}");
            assert!(
                text.contains("a=source-filter: incl IN IP4 239.99.99.1 192.0.2.10"),
                "Sender source-filter:\n{text}",
            );
            assert!(
                text.contains("a=x-nvnmos-iface-ip:192.0.2.10"),
                "Sender iface-ip duplicates source_ip:\n{text}",
            );
            assert!(matches!(inner, InnerConfig::Real(_)));
        }

        /// `caps` and an explicit `transport-file*` both present
        /// → the explicit file's *essence shape* (encoding-name +
        /// clock-rate) survives even though caps could have
        /// synthesised an L16 SDP if the passthrough path
        /// hadn't taken over. Pins the precedence rule
        /// "transport-file > caps synthesis at startup". The
        /// destination address is then rewritten by the splice
        /// (per `multicast_ip` property), which is a separate
        /// layer that runs after passthrough.
        #[test]
        fn resolve_inner_config_udp_transport_file_beats_caps_synthesis() {
            const AUDIO_L24_SDP: &str = concat!(
                "v=0\r\n",
                "o=- 1 0 IN IP4 192.0.2.10\r\n",
                "s=Example\r\n",
                "t=0 0\r\n",
                "m=audio 5004 RTP/AVP 97\r\n",
                "c=IN IP4 239.2.2.2/64\r\n",
                "a=rtpmap:97 L24/48000/2\r\n",
            );
            // S24BE caps matches the passthrough SDP's L24
            // encoding; if synthesis had run, the resulting
            // SDP would have inherited the same shape — so
            // the encoding-name alone can't tell us which
            // path executed. The differentiator is the
            // `a=fmtp:` line: synthesised SDPs emit
            // `pm=2110GPM,ssn=ST2110-20:2017` on the fmtp
            // line (because `rtp_caps_from_raw_video` always
            // emits these defaults), but a passthrough SDP
            // missing those slots stays missing them.
            let essence = gst::Caps::builder("audio/x-raw")
                .field("format", "S24BE")
                .field("rate", 48_000_i32)
                .field("channels", 2_i32)
                .field("layout", "interleaved")
                .build();
            let s = CommonSettings {
                caps: Some(essence),
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let (_, text) = resolve_inner_config_udp(
                &cat(),
                "nmossrc",
                &s,
                UdpVariant::V1,
                Some(AUDIO_L24_SDP.to_owned()),
            )
            .expect("passthrough must succeed");
            let text = text.expect("transport_file passthrough");
            // The passthrough SDP carries no `a=ptime:` line;
            // synthesis would have emitted one (with
            // `defaults::AUDIO_PTIME_NS` = 1ms). Absence pins
            // that the synth path didn't execute.
            assert!(
                !text.contains("a=ptime"),
                "synthesis would have emitted a=ptime:1 — but passthrough wins:\n{text}",
            );
        }

        /// `transport-caps` carries the RTP payload-type
        /// override (RFC 3551 §6 dynamic range 96..=127, all
        /// essences); `property_overrides_udp` must read it
        /// from the caps' `payload` i32 field and cast to u8.
        #[test]
        fn property_overrides_udp_reads_pt_from_transport_caps() {
            let tc = gst::Caps::builder("application/x-rtp")
                .field("payload", 99i32)
                .build();
            let s = CommonSettings {
                transport_caps: Some(tc),
                ..udp_settings(Side::Sender, Transport::Udp)
            };
            let o = property_overrides_udp(&s);
            assert_eq!(o.payload_type, Some(99));
        }

        /// `transport-caps` carries the audio-only override
        /// slots (clock-rate, a-ptime, a-maxptime). The
        /// builder reads them blindly; the splice helper does
        /// the audio-essence gating downstream.
        #[test]
        fn property_overrides_udp_reads_audio_overrides_from_transport_caps() {
            let tc = gst::Caps::builder("application/x-rtp")
                .field("clock-rate", 96_000i32)
                .field("a-ptime", "1")
                .field("a-maxptime", "2")
                .build();
            let s = CommonSettings {
                transport_caps: Some(tc),
                ..udp_settings(Side::Sender, Transport::Udp)
            };
            let o = property_overrides_udp(&s);
            assert_eq!(o.audio_clock_rate, Some(96_000));
            assert_eq!(o.a_ptime, Some("1"));
            assert_eq!(o.a_maxptime, Some("2"));
        }

        /// No `transport-caps` → all four override slots are
        /// `None`, even when the property layer hands us a
        /// `CommonSettings` with the field defaulted.
        #[test]
        fn property_overrides_udp_no_transport_caps_leaves_override_slots_none() {
            let s = udp_settings(Side::Sender, Transport::Udp);
            assert!(s.transport_caps.is_none(), "fixture must default to None");
            let o = property_overrides_udp(&s);
            assert_eq!(o.payload_type, None);
            assert_eq!(o.audio_clock_rate, None);
            assert_eq!(o.a_ptime, None);
            assert_eq!(o.a_maxptime, None);
        }

        /// End-to-end: an audio `transport-file` with a base
        /// pt / clock-rate / ptime gets rewritten by
        /// `resolve_inner_config_udp` to match the user's
        /// `transport-caps`. Pins that the pt + clock-rate +
        /// ptime path all the way from `Settings.transport_caps`
        /// → `property_overrides_udp` → `sdp::passthrough_with_overrides`
        /// actually changes the wire SDP.
        #[test]
        fn resolve_inner_config_udp_applies_transport_caps_audio_overrides() {
            // 48 kHz L24 stereo, pt=97, ptime=0.125. The
            // simplest audio SDP that exercises all four
            // override slots in one pass.
            const AUDIO_L24_SDP: &str = concat!(
                "v=0\r\n",
                "o=- 1 0 IN IP4 192.0.2.10\r\n",
                "s=Example\r\n",
                "t=0 0\r\n",
                "m=audio 5004 RTP/AVP 97\r\n",
                "c=IN IP4 239.2.2.2/64\r\n",
                "a=rtpmap:97 L24/48000/2\r\n",
                "a=ptime:0.125\r\n",
                "a=mediaclk:direct=0\r\n",
                "a=ts-refclk:ptp=IEEE1588-2008:traceable\r\n",
            );
            let tc = gst::Caps::builder("application/x-rtp")
                .field("payload", 100i32)
                .field("clock-rate", 96_000i32)
                .field("a-ptime", "1")
                .field("a-maxptime", "1")
                .build();
            let s = CommonSettings {
                side: Side::Receiver,
                transport_caps: Some(tc),
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let (_, spliced) = resolve_inner_config_udp(
                &cat(),
                "nmossrc",
                &s,
                UdpVariant::V1,
                Some(AUDIO_L24_SDP.to_owned()),
            )
            .expect("splice + decide must succeed");
            let spliced = spliced.expect("transport_file must round-trip");

            assert!(spliced.contains("m=audio 5004 RTP/AVP 100"),
                "pt override must hit m= line; got: {spliced}");
            assert!(spliced.contains("a=rtpmap:100 L24/96000/2"),
                "pt + clock-rate must land on rtpmap together; got: {spliced}");
            assert!(spliced.contains("a=ptime:1\r\n"),
                "a=ptime override; got: {spliced}");
            assert!(spliced.contains("a=maxptime:1\r\n"),
                "a=maxptime override; got: {spliced}");
        }

        /// An invalid pt in `transport-caps` causes
        /// `resolve_inner_config_udp` to fail with the
        /// SdpError surfaced through the `with_context`
        /// chain. The element will then bail out of
        /// NULL→READY rather than silently producing a
        /// broken SDP.
        #[test]
        fn resolve_inner_config_udp_rejects_invalid_pt_in_transport_caps() {
            const AUDIO_L24_SDP: &str = concat!(
                "v=0\r\n",
                "o=- 1 0 IN IP4 192.0.2.10\r\n",
                "s=Example\r\n",
                "t=0 0\r\n",
                "m=audio 5004 RTP/AVP 97\r\n",
                "c=IN IP4 239.2.2.2/64\r\n",
                "a=rtpmap:97 L24/48000/2\r\n",
            );
            let tc = gst::Caps::builder("application/x-rtp")
                .field("payload", 33i32) // legacy MP2T, outside dynamic range
                .build();
            let s = CommonSettings {
                transport_caps: Some(tc),
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let err = resolve_inner_config_udp(
                &cat(),
                "nmossrc",
                &s,
                UdpVariant::V1,
                Some(AUDIO_L24_SDP.to_owned()),
            )
            .expect_err("must reject pt=33");
            let chain = format!("{err:#}");
            assert!(
                chain.contains("96..=127") || chain.contains("dynamic range"),
                "error must attribute the RFC 3551 range; got: {chain}",
            );
        }

        // -- cross-check -------------------------------------------

        /// Matching essence caps + matching transport caps
        /// against a raw video SDP must pass through
        /// `decide_inner_config_udp` cleanly. Pins the
        /// happy-path: cross-check is opt-in (driven by user
        /// supplying `caps` / `transport-caps`) and must not
        /// regress the existing SDP-only path.
        #[test]
        fn decide_inner_config_udp_accepts_matching_caps() {
            let s = CommonSettings {
                caps: Some(
                    gst::Caps::builder("video/x-raw")
                        .field("width", 1920i32)
                        .field("height", 1080i32)
                        .build(),
                ),
                transport_caps: Some(
                    gst::Caps::builder("application/x-rtp")
                        .field("media", "video")
                        .field("encoding-name", "RAW")
                        .field("clock-rate", 90_000i32)
                        .build(),
                ),
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            decide_inner_config_udp(
                "nmossrc",
                &s,
                UdpVariant::V1,
                Some(VIDEO_UDP_SDP),
                sdp::EssenceCrossCheckMode::Full,
            )
            .expect("matching caps + transport_caps → ok");
        }

        /// Format-family cross-check: `caps=audio/x-raw` on
        /// an `nmossrc` configured to receive a video SDP is
        /// a real misconfiguration → bail.
        #[test]
        fn decide_inner_config_udp_rejects_essence_caps_format_mismatch() {
            let s = CommonSettings {
                caps: Some(gst::Caps::builder("audio/x-raw").build()),
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let err = decide_inner_config_udp(
                "nmossrc",
                &s,
                UdpVariant::V1,
                Some(VIDEO_UDP_SDP),
                sdp::EssenceCrossCheckMode::Full,
            )
            .expect_err("audio caps + video SDP must error");
            let chain = format!("{err:#}");
            assert!(
                chain.contains("essence format mismatch")
                    && chain.contains("cross-checking SDP"),
                "error must attribute to cross-check; got: {chain}",
            );
        }

        /// Video clock-rate cross-check: 48 kHz declared in
        /// `transport-caps` against a 90 kHz video SDP must
        /// error. Pins the override-vs-cross-check rule: video
        /// clock-rate is cross-check, not override (audio is
        /// the override case, covered by a separate test).
        #[test]
        fn decide_inner_config_udp_rejects_video_clock_rate_mismatch() {
            let s = CommonSettings {
                transport_caps: Some(
                    gst::Caps::builder("application/x-rtp")
                        .field("clock-rate", 48_000i32)
                        .build(),
                ),
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let err = decide_inner_config_udp(
                "nmossrc",
                &s,
                UdpVariant::V1,
                Some(VIDEO_UDP_SDP),
                sdp::EssenceCrossCheckMode::Full,
            )
            .expect_err("video clock-rate mismatch must error");
            let chain = format!("{err:#}");
            assert!(
                chain.contains("transport-caps mismatch"),
                "error must attribute to cross-check; got: {chain}",
            );
        }

        /// Wide receiver activation: stereo `caps` must not
        /// block mono activation SDP when the SDP carries
        /// `a=x-nvnmos-caps:` (essence shape is not cross-checked;
        /// format family still matches).
        #[test]
        fn activation_udp_wide_receiver_skips_essence_shape_cross_check() {
            const AUDIO_MONO_ACTIVATION_SDP: &str = concat!(
                "v=0\r\n",
                "o=- 1 0 IN IP4 192.0.2.10\r\n",
                "s=Example\r\n",
                "t=0 0\r\n",
                "m=audio 5004 RTP/AVP 97\r\n",
                "c=IN IP4 239.2.2.2/64\r\n",
                "a=rtpmap:97 L24/48000\r\n",
                "a=x-nvnmos-caps:97\r\n",
            );
            let s = CommonSettings {
                caps: Some(
                    gst::Caps::builder("audio/x-raw")
                        .field("channels", 2i32)
                        .build(),
                ),
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let plan = make_activation_plan(
                &cat(),
                "nmossrc",
                &s,
                &req(Side::Receiver, Some(AUDIO_MONO_ACTIVATION_SDP)),
            );
            match plan.inner {
                InnerConfig::Real(TransportConfig::Udp { media, .. }) => {
                    assert_eq!(
                        media
                            .raw_caps
                            .structure(0)
                            .and_then(|s| s.get::<i32>("channels").ok()),
                        Some(1),
                        "activation SDP is authoritative for channel count",
                    );
                }
                other => panic!("expected Real(Udp) inner on wide activation; got {other:?}"),
            }
            assert!(matches!(plan.ack, ActivationAck::Success));
        }

        /// `receiver-caps-mode=wide` alone does not relax activation
        /// cross-check — the activation SDP must carry
        /// `a=x-nvnmos-caps:` (libnvnmos adds it for wide receivers).
        #[test]
        fn activation_udp_property_wide_without_sdp_marker_still_cross_checks() {
            const AUDIO_MONO_ACTIVATION_SDP: &str = concat!(
                "v=0\r\n",
                "o=- 1 0 IN IP4 192.0.2.10\r\n",
                "s=Example\r\n",
                "t=0 0\r\n",
                "m=audio 5004 RTP/AVP 97\r\n",
                "c=IN IP4 239.2.2.2/64\r\n",
                "a=rtpmap:97 L24/48000\r\n",
            );
            let s = CommonSettings {
                caps: Some(
                    gst::Caps::builder("audio/x-raw")
                        .field("channels", 2i32)
                        .build(),
                ),
                caps_mode: CapsMode::Wide,
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let plan = make_activation_plan(
                &cat(),
                "nmossrc",
                &s,
                &req(Side::Receiver, Some(AUDIO_MONO_ACTIVATION_SDP)),
            );
            assert!(matches!(plan.inner, InnerConfig::Fake { .. }));
            match plan.ack {
                ActivationAck::Failure { reason } => assert!(
                    reason.contains("cross-checking SDP"),
                    "expected essence cross-check failure; got: {reason}",
                ),
                other => panic!("expected Failure ack; got {other:?}"),
            }
        }

        /// Activation SDP cross-check fires too: a video
        /// `nmossink` element receiving an audio activation
        /// surfaces `SdpError::FormatMismatch` via
        /// `make_activation_plan`. The activation ack is
        /// `Failure` with attribution.
        #[test]
        fn activation_udp_cross_check_failure_acks_failure() {
            const AUDIO_ACTIVATION_SDP: &str = concat!(
                "v=0\r\n",
                "o=- 1 0 IN IP4 192.0.2.10\r\n",
                "s=Example\r\n",
                "t=0 0\r\n",
                "m=audio 5004 RTP/AVP 97\r\n",
                "c=IN IP4 239.2.2.2/64\r\n",
                "a=rtpmap:97 L24/48000/2\r\n",
            );
            let s = CommonSettings {
                caps: Some(gst::Caps::builder("video/x-raw").build()),
                ..udp_settings(Side::Sender, Transport::Udp)
            };
            let plan = make_activation_plan(
                &cat(),
                "nmossink",
                &s,
                &req(Side::Sender, Some(AUDIO_ACTIVATION_SDP)),
            );
            assert!(matches!(plan.inner, InnerConfig::Fake { .. }));
            match plan.ack {
                ActivationAck::Failure { reason } => assert!(
                    reason.contains("cross-checking SDP")
                        && reason.contains("essence format mismatch"),
                    "expected cross-check attribution; got: {reason}",
                ),
                ActivationAck::Success => panic!("expected Failure ack on cross-check fail"),
            }
        }

        /// Activation SDP is authoritative — property overrides
        /// must NOT splice into the activation transport file.
        /// Mirrors `resolve_activation_inner_mxl`'s
        /// `property_id=""` choice (see its doc comment at
        /// "Activation: the daemon's transport file is
        /// authoritative."). The transport file in the
        /// returned Real(Udp) config must equal the activation
        /// input byte-for-byte.
        #[test]
        fn activation_udp_does_not_apply_property_overrides() {
            let s = CommonSettings {
                side: Side::Receiver,
                // Properties that WOULD splice if applied.
                multicast_ip: "232.0.0.1".to_owned(),
                destination_port: 5008,
                label: "Spliced label".to_owned(),
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let plan = make_activation_plan(
                &cat(),
                "nmossrc",
                &s,
                &req(Side::Receiver, Some(VIDEO_UDP_SDP)),
            );
            match plan.inner {
                InnerConfig::Real(TransportConfig::Udp { media, transport_file, .. }) => {
                    // Activation address is preserved.
                    assert_eq!(media.primary.destination_ip, "239.1.1.1");
                    assert_eq!(media.primary.destination_port, 5004);
                    assert_eq!(
                        transport_file.as_deref(),
                        Some(VIDEO_UDP_SDP),
                        "activation SDP must pass through untouched",
                    );
                }
                other => panic!("expected Real(Udp), got {other:?}"),
            }
            assert!(matches!(plan.ack, ActivationAck::Success));
        }

        #[test]
        fn activation_nvdsudp_is_not_implemented_failure() {
            let s = udp_settings(Side::Sender, Transport::NvDsUdp);
            let plan = make_activation_plan(
                &cat(),
                "nmossink",
                &s,
                &req(Side::Sender, Some(VIDEO_UDP_SDP)),
            );
            assert!(matches!(plan.inner, InnerConfig::Fake { .. }));
            match plan.ack {
                ActivationAck::Failure { reason } => assert!(
                    reason.contains("nvdsudp") && reason.contains("not yet implemented"),
                    "expected nvdsudp not-implemented attribution: {reason}",
                ),
                ActivationAck::Success => panic!("expected Failure ack for nvdsudp"),
            }
        }
    }
}
