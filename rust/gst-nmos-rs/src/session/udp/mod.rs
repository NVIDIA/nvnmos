// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! RTP/UDP transport session setup and activation.

pub(crate) mod types;

use anyhow::{Context, bail};
use gstreamer as gst;

use super::{CommonSettings, FakeKind, InnerConfig, TransportConfig};
use super::types::Side;
use crate::sdp::{self, DualLegPassthroughPolicy, SdpOverrides};
use crate::types::{CapsMode, Transport};

/// Read the NMOS resource name from an SDP transport file.
///
/// Used when the element already has a `transport-file` at NULL→READY.
/// Caps-only synthesis and deferred AddSender still require the name property.
pub(super) fn resource_name_from_transport_file(text: &str) -> Result<Option<String>, sdp::SdpError> {
    sdp::resource_name_from_transport(text)
}

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

/// Receiver SDP connection address for the `c=` line from IS-05
/// properties. Matches nmos-cpp `get_connection_address`:
/// `multicast_ip` when set, else `interface_ip` (unicast reception /
/// caps synthesis).
///
/// On the passthrough path this same dispatch feeds
/// [`SdpOverrides::destination_ip`], so `interface-ip` alone rewrites
/// `c=` even when the transport file already carries a multicast
/// address — it is not a "join NIC only, leave `c=` alone" override.
/// To pin IGMP join NIC on multicast without changing the group, put
/// `a=x-nvnmos-iface-ip` in the file or set `multicast-ip` to the
/// group's address alongside `interface-ip`.
pub(super) fn receiver_connection_address(settings: &CommonSettings) -> &str {
    if !settings.multicast_ip.is_empty() {
        settings.multicast_ip.as_str()
    } else {
        settings.interface_ip.as_str()
    }
}

/// Minimum IS-05 endpoint properties required to synthesise a configuring
/// SDP for AddSender / AddReceiver. Wire destinations may be omitted.
pub(super) fn validate_rtp_configuring_minimum(
    element: &str,
    settings: &CommonSettings,
) -> Result<(), anyhow::Error> {
    match settings.side {
        Side::Sender if settings.source_ip.is_empty() => {
            bail!(
                "{element}: `source-ip` is required to synthesise configuring SDP for AddSender"
            );
        }
        Side::Receiver if settings.interface_ip.is_empty() => {
            bail!(
                "{element}: `interface-ip` is required to synthesise configuring SDP for AddReceiver"
            );
        }
        _ => Ok(()),
    }
}

/// A `c=` / IS-05 `destination_ip` that is the RFC 4566 unspecified
/// address (or empty) is a configuring placeholder, not a real endpoint
/// to send to / receive from.
fn is_unspecified_destination(addr: &str) -> bool {
    addr.is_empty()
        || addr
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_unspecified())
            .unwrap_or(false)
}

/// Deferrable RTP/UDP parameter still unavailable for eager
/// `auto-activate`.
///
/// The one binding constraint is the same for both sides: the resolved
/// IS-05 `destination_ip` slot in `media.primary`. There is no sensible
/// default to invent for an unspecified address (`0.0.0.0`), so a
/// configuring chain without one must wait for an IS-05 PATCH. It is
/// read from the resolved `media`, so a property and a transport file
/// are honoured on the same footing. The destination *port* is not a
/// blocker: an unset / zero port resolves to the IS-05 auto default
/// ([`sdp::defaults::RTP_PORT`], 5004) here and in the daemon.
///
/// The check is side-neutral but the reported property is not, because
/// the same `destination_ip` slot is filled by different properties:
/// the Sender's egress target is `destination-ip`; the Receiver has no
/// `destination-ip` property and supplies the slot via `multicast-ip`
/// (or `interface-ip` for unicast). The other side-specific endpoints —
/// Sender `source-ip`, Receiver `interface-ip` — are required when the
/// Sender / Receiver is added (or fall back to an OS default), not
/// deferrable activation params, so they are never reported here.
///
/// Only a configured-but-dormant real chain can be eager-activated; any
/// fake chain returns `None` so the policy leaves deferred
/// (`NotConfigured`) / invalid (`Misconfigured`) resources alone and also
/// does not treat an `a=inactive` leg (`NotActive`) — fully specified
/// but dormant — as an auto-activate failure.
pub(super) fn udp_eager_blocked(side: Side, inner: &InnerConfig) -> Option<&'static str> {
    let media = match inner {
        InnerConfig::Real(TransportConfig::Udp { media, .. })
        | InnerConfig::Real(TransportConfig::NvDsUdp { media, .. }) => media,
        _ => return None,
    };
    if is_unspecified_destination(&media.primary.destination_ip) {
        Some(match side {
            Side::Sender => "destination-ip",
            Side::Receiver => "multicast-ip or interface-ip",
        })
    } else {
        None
    }
}

/// Build [`sdp::SdpBuildInput`] from element settings and essence caps.
/// Shared by NULL→READY caps synthesis and READY→PAUSED deferred
/// sender AddSender.
pub(super) fn sdp_build_input<'a>(
    settings: &'a CommonSettings,
    essence_caps: &'a gst::Caps,
) -> sdp::SdpBuildInput<'a> {
    let destination_ip = match settings.side {
        Side::Sender => settings.destination_ip.as_str(),
        Side::Receiver => receiver_connection_address(settings),
    };
    let advertise_caps = match settings.caps_mode {
        CapsMode::Auto | CapsMode::Narrow => false,
        CapsMode::Wide => true,
    };
    sdp::SdpBuildInput {
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
        node_seed: &settings.node.node_seed,
        narrow_traffic_profile: settings.transport == Transport::NvDsUdp,
    }
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
            validate_rtp_configuring_minimum(element, settings)?;
            let text = sdp::from_caps(&sdp_build_input(settings, essence_caps))
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

/// `decide_inner_config_*` treats missing SDP as [`FakeKind::Misconfigured`].
/// For senders with neither `transport-file*` nor `caps`, that is deferred
/// mode — rewrite to [`FakeKind::NotConfigured`] so NULL→READY opens the
/// session without AddSender and waits for peer caps at READY→PAUSED.
/// Receivers without input stay misconfigured (no deferred path).
fn gate_deferred_sender_udp(inner: InnerConfig, settings: &CommonSettings) -> InnerConfig {
    if settings.side == Side::Sender
        && matches!(
            inner,
            InnerConfig::Fake {
                kind: FakeKind::Misconfigured,
                ..
            }
        )
    {
        InnerConfig::Fake {
            kind: FakeKind::NotConfigured,
            detail: String::new(),
        }
    } else {
        inner
    }
}

/// Synthesise configuring SDP and resolve the inner chain for deferred
/// RTP sender AddSender from fixated upstream peer caps.
pub(super) fn synthesise_deferred_sender_udp(
    element: &str,
    settings: &CommonSettings,
    essence_caps: &gst::Caps,
) -> Result<(String, InnerConfig), anyhow::Error> {
    validate_rtp_configuring_minimum(element, settings)?;
    let text = sdp::from_caps(&sdp_build_input(settings, essence_caps))
        .map_err(anyhow::Error::from)
        .with_context(|| format!("{element}: synthesising SDP from peer caps"))?;
    let inner = match settings.transport {
        Transport::Udp => decide_inner_config_udp(
            element,
            settings,
            UdpVariant::V1,
            Some(&text),
            sdp::EssenceCrossCheckMode::Full,
        )?,
        Transport::Udp2 => decide_inner_config_udp(
            element,
            settings,
            UdpVariant::V2,
            Some(&text),
            sdp::EssenceCrossCheckMode::Full,
        )?,
        Transport::NvDsUdp => decide_inner_config_nvdsudp(
            element,
            settings,
            Some(&text),
            sdp::EssenceCrossCheckMode::Full,
        )?,
        Transport::Mxl => bail!("{element}: internal: MXL is not an RTP/UDP transport"),
    };
    let blocked = udp_eager_blocked(settings.side, &inner);
    let inner = super::apply_auto_activate_policy(&crate::CAT, element, settings, inner, blocked);
    Ok((text, inner))
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
            // `x-nvnmos-iface-ip` (join NIC). When `multicast-ip` is
            // unset, [`receiver_connection_address`] also maps this into
            // `destination_ip` for the `c=` splice — see its doc.
            opt(&settings.interface_ip),
            opt(receiver_connection_address(settings)),
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
fn finish_udp_inner_config(
    element: &str,
    settings: &CommonSettings,
    transport: Transport,
    text: &str,
    cross_check_mode: sdp::EssenceCrossCheckMode,
    build_real: impl FnOnce(types::UdpMedia) -> InnerConfig,
) -> Result<InnerConfig, anyhow::Error> {
    if transport != Transport::NvDsUdp && sdp::sdp_media_block_count(text)? > 1 {
        return Err(anyhow::Error::new(sdp::SdpError::DualLegNotSupported));
    }
    let media = sdp::parse_sdp(text).with_context(|| {
        format!(
            "{element}: parsing SDP transport file for transport={transport:?}"
        )
    })?;
    sdp::cross_check_essence(
        &media,
        settings.caps.as_ref(),
        settings.transport_caps.as_ref(),
        cross_check_mode,
    )
    .with_context(|| {
        format!(
            "{element}: cross-checking SDP against `caps` / `transport-caps` \
             for transport={transport:?}"
        )
    })?;
    if sdp::count_active_sdp_legs(text)? == 0 {
        return Ok(InnerConfig::Fake {
            kind: FakeKind::NotActive,
            detail: "all legs inactive (rtp_enabled: false)".into(),
        });
    }
    Ok(build_real(media))
}

pub(crate) fn decide_inner_config_udp(
    element: &str,
    settings: &CommonSettings,
    variant: UdpVariant,
    transport_file: Option<&str>,
    cross_check_mode: sdp::EssenceCrossCheckMode,
) -> Result<InnerConfig, anyhow::Error> {
    let Some(text) = transport_file else {
        return Ok(InnerConfig::Fake {
            kind: FakeKind::Misconfigured,
            detail: "caps or transport-file required for AddSender / AddReceiver".into(),
        });
    };
    finish_udp_inner_config(
        element,
        settings,
        settings.transport,
        text,
        cross_check_mode,
        |media| {
            InnerConfig::Real(TransportConfig::Udp {
                variant,
                media,
                transport_file: Some(text.to_owned()),
            })
        },
    )
}

pub(crate) fn decide_inner_config_nvdsudp(
    element: &str,
    settings: &CommonSettings,
    transport_file: Option<&str>,
    cross_check_mode: sdp::EssenceCrossCheckMode,
) -> Result<InnerConfig, anyhow::Error> {
    let Some(text) = transport_file else {
        return Ok(InnerConfig::Fake {
            kind: FakeKind::Misconfigured,
            detail: "caps or transport-file required for AddSender / AddReceiver".into(),
        });
    };
    finish_udp_inner_config(
        element,
        settings,
        Transport::NvDsUdp,
        text,
        cross_check_mode,
        |media| {
            InnerConfig::Real(TransportConfig::NvDsUdp {
                media,
                transport_file: Some(text.to_owned()),
            })
        },
    )
}

pub(super) fn resolve_inner_config_nvdsudp(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    resolved_transport_file: Option<String>,
) -> Result<(InnerConfig, Option<String>), anyhow::Error> {
    let had_user_transport_file = resolved_transport_file.is_some();
    let resolved_transport_file =
        synthesise_or_passthrough_udp(cat, element, settings, resolved_transport_file)?;

    let resolved_transport_file = match resolved_transport_file {
        Some(text) if had_user_transport_file => Some(
            sdp::passthrough_with_overrides(
                &text,
                &property_overrides_udp(settings),
                DualLegPassthroughPolicy::AllowDualLeg,
            )
            .with_context(|| {
                format!("{element}: applying property overrides to transport-file SDP")
            })?,
        ),
        other => other,
    };
    let inner = gate_deferred_sender_udp(
        decide_inner_config_nvdsudp(
            element,
            settings,
            resolved_transport_file.as_deref(),
            sdp::EssenceCrossCheckMode::Full,
        )?,
        settings,
    );
    let blocked = udp_eager_blocked(settings.side, &inner);
    let inner = super::apply_auto_activate_policy(cat, element, settings, inner, blocked);
    Ok((inner, resolved_transport_file))
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
            sdp::passthrough_with_overrides(
                &text,
                &property_overrides_udp(settings),
                DualLegPassthroughPolicy::RejectDualLeg,
            )
            .with_context(|| {
                format!("{element}: applying property overrides to transport-file SDP")
            })?,
        ),
        other => other,
    };
    let inner = gate_deferred_sender_udp(
        decide_inner_config_udp(
            element,
            settings,
            variant,
            resolved_transport_file.as_deref(),
            sdp::EssenceCrossCheckMode::Full,
        )?,
        settings,
    );
    let blocked = udp_eager_blocked(settings.side, &inner);
    let inner = super::apply_auto_activate_policy(cat, element, settings, inner, blocked);
    Ok((inner, resolved_transport_file))
}

#[cfg(test)]
mod tests {
    use super::super::support::*;
    use super::super::*;
    use super::*;
    use super::types::{UdpLeg, UdpMedia};
    use crate::sdp;
    use crate::types::FlowFormat;
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

    mod eager_blocked {
        use super::*;

        fn real_udp(media: UdpMedia) -> InnerConfig {
            InnerConfig::Real(TransportConfig::Udp {
                variant: UdpVariant::V1,
                media,
                transport_file: Some("sdp".to_owned()),
            })
        }

        #[test]
        fn real_endpoints_are_unblocked() {
            assert!(udp_eager_blocked(Side::Sender, &real_udp(sample_udp_media())).is_none());
            assert!(udp_eager_blocked(Side::Receiver, &real_udp(sample_udp_media())).is_none());
        }

        #[test]
        fn unspecified_destination_ip_blocks_with_side_specific_label() {
            let mut m = sample_udp_media();
            m.primary.destination_ip = "0.0.0.0".to_owned();
            let inner = real_udp(m);
            assert_eq!(
                udp_eager_blocked(Side::Sender, &inner),
                Some("destination-ip")
            );
            assert_eq!(
                udp_eager_blocked(Side::Receiver, &inner),
                Some("multicast-ip or interface-ip")
            );
        }

        #[test]
        fn empty_destination_ip_blocks() {
            let mut m = sample_udp_media();
            m.primary.destination_ip = String::new();
            assert_eq!(
                udp_eager_blocked(Side::Sender, &real_udp(m)),
                Some("destination-ip")
            );
        }

        #[test]
        fn zero_destination_port_does_not_block() {
            // 0 / omitted port resolves to the IS-05 auto default (5004)
            // here and in the daemon, so it is never a deferral reason —
            // only an unspecified destination address can be.
            let mut m = sample_udp_media();
            m.primary.destination_port = 0;
            assert!(udp_eager_blocked(Side::Sender, &real_udp(m)).is_none());
        }

        #[test]
        fn fake_chain_never_blocks() {
            let inner = InnerConfig::Fake {
                kind: FakeKind::NotConfigured,
                detail: String::new(),
            };
            assert!(udp_eager_blocked(Side::Sender, &inner).is_none());
            assert!(udp_eager_blocked(Side::Receiver, &inner).is_none());
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

        fn dual_leg_video_udp_sdp() -> String {
            let mut sdp = VIDEO_UDP_SDP.to_owned();
            sdp.push_str(
                "m=video 5006 RTP/AVP 96\r\n\
                 c=IN IP4 239.1.1.2/64\r\n\
                 a=rtpmap:96 raw/90000\r\n\
                 a=fmtp:96 sampling=YCbCr-4:2:2; width=1920; height=1080;\
                 exactframerate=50; depth=10\r\n",
            );
            sdp
        }

        #[test]
        fn decide_udp_rejects_dual_leg_transport_file() {
            let sdp = dual_leg_video_udp_sdp();
            for transport in [Transport::Udp, Transport::Udp2] {
                let s = udp_settings(Side::Receiver, transport);
                let err = decide_inner_config_udp(
                    "nmossrc",
                    &s,
                    if transport == Transport::Udp2 {
                        UdpVariant::V2
                    } else {
                        UdpVariant::V1
                    },
                    Some(&sdp),
                    sdp::EssenceCrossCheckMode::Full,
                )
                .expect_err("dual-leg SDP must be rejected for transport=udp/udp2");
                assert!(
                    format!("{err:#}").contains("dual-leg SDP requires transport=nvdsudp"),
                    "unexpected error: {err:#}",
                );
            }
        }

        #[test]
        fn decide_udp_single_leg_inactive_is_fake() {
            let sdp = VIDEO_UDP_SDP.replace(
                "m=video 5004 RTP/AVP 96\r\n",
                "m=video 5004 RTP/AVP 96\r\na=inactive\r\n",
            );
            let s = udp_settings(Side::Receiver, Transport::Udp);
            let inner = decide_inner_config_udp(
                "nmossrc",
                &s,
                UdpVariant::V1,
                Some(&sdp),
                sdp::EssenceCrossCheckMode::Full,
            )
            .expect("inactive single-leg SDP parses");
            match inner {
                InnerConfig::Fake { kind, .. } => {
                    assert_eq!(kind, FakeKind::NotActive);
                }
                other => panic!("expected Fake for inactive leg, got {other:?}"),
            }
        }

        #[test]
        fn activation_udp_inactive_leg_acks_success_with_fake_chain() {
            let sdp = VIDEO_UDP_SDP.replace(
                "m=video 5004 RTP/AVP 96\r\n",
                "m=video 5004 RTP/AVP 96\r\na=inactive\r\n",
            );
            let s = udp_settings(Side::Receiver, Transport::Udp);
            let plan = make_activation_plan(
                &cat(),
                "nmossrc",
                &s,
                &req(Side::Receiver, Some(&sdp)),
            );
            match plan.inner {
                InnerConfig::Fake { kind, .. } => {
                    assert_eq!(kind, FakeKind::NotActive);
                }
                other => panic!("expected Fake for inactive leg, got {other:?}"),
            }
            assert!(
                matches!(plan.ack, ActivationAck::Success),
                "master_enable with rtp_enabled=false is a valid dormant activation",
            );
        }

        #[test]
        fn decide_udp_without_transport_file_is_misconfigured() {
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
                    InnerConfig::Fake { kind, detail } => {
                        assert_eq!(kind, FakeKind::Misconfigured);
                        assert!(
                            detail.contains("caps or transport-file"),
                            "expected misconfiguration detail, got {detail:?}",
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

        /// Receiver `multicast_ip` wins over `interface_ip` for the
        /// `c=` override slot; `interface_ip` still drives
        /// `a=x-nvnmos-iface-ip` separately.
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

        /// Unicast receiver: empty `multicast_ip` maps `interface_ip`
        /// into `SdpOverrides.destination_ip` (the `c=` slot).
        #[test]
        fn property_overrides_udp_receiver_unicast_maps_interface_ip_to_destination_ip() {
            let s = CommonSettings {
                side: Side::Receiver,
                interface_ip: "192.0.2.30".to_owned(),
                ..udp_settings(Side::Receiver, Transport::Udp)
            };
            let o = property_overrides_udp(&s);
            assert_eq!(o.destination_ip, Some("192.0.2.30"));
            assert_eq!(o.interface_ip, Some("192.0.2.30"));
            assert_eq!(o.source_ip, None);
        }

        // -- receiver passthrough override matrix ----------------

        /// Passthrough property-override matrix (3 SDP baselines × 8
        /// property combos). Notable case: multicast configuring SDP with
        /// `interface-ip` only (no `multicast-ip`) rewrites `c=` to the
        /// unicast NIC — see
        /// `receiver_passthrough_multicast_file_interface_ip_only_rewrites_c`.
        mod receiver_passthrough_matrix {
            use super::*;

            const FILE_MCAST: &str = "239.1.1.1";
            const FILE_UNICAST: &str = "192.0.2.50";
            const FILE_SSM_SRC: &str = "192.0.2.100";
            const PROP_MCAST: &str = "232.99.99.1";
            const PROP_IFACE: &str = "192.0.2.30";
            const PROP_SRC: &str = "192.0.2.20";

            const MCAST_ASM_SDP: &str = concat!(
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

            const MCAST_SSM_SDP: &str = concat!(
                "v=0\r\n",
                "o=- 1 0 IN IP4 192.0.2.10\r\n",
                "s=test\r\n",
                "t=0 0\r\n",
                "m=video 5004 RTP/AVP 96\r\n",
                "c=IN IP4 239.1.1.1/64\r\n",
                "a=rtpmap:96 raw/90000\r\n",
                "a=fmtp:96 sampling=YCbCr-4:2:2; width=1920; height=1080;",
                " exactframerate=50; depth=10\r\n",
                "a=source-filter: incl IN IP4 239.1.1.1 192.0.2.100\r\n",
            );

            const UNICAST_SDP: &str = concat!(
                "v=0\r\n",
                "o=- 1 0 IN IP4 192.0.2.10\r\n",
                "s=test\r\n",
                "t=0 0\r\n",
                "m=video 5004 RTP/AVP 96\r\n",
                "c=IN IP4 192.0.2.50\r\n",
                "a=rtpmap:96 raw/90000\r\n",
                "a=fmtp:96 sampling=YCbCr-4:2:2; width=1920; height=1080;",
                " exactframerate=50; depth=10\r\n",
            );

            #[derive(Debug, Clone, Copy)]
            enum SdpKind {
                McastAsm,
                McastSsm,
                Unicast,
            }

            impl SdpKind {
                fn sdp(self) -> &'static str {
                    match self {
                        Self::McastAsm => MCAST_ASM_SDP,
                        Self::McastSsm => MCAST_SSM_SDP,
                        Self::Unicast => UNICAST_SDP,
                    }
                }

                fn baseline_c(self) -> &'static str {
                    match self {
                        Self::McastAsm | Self::McastSsm => FILE_MCAST,
                        Self::Unicast => FILE_UNICAST,
                    }
                }

                fn baseline_has_ssm(self) -> bool {
                    matches!(self, Self::McastSsm)
                }
            }

            fn receiver_settings(
                multicast: bool,
                interface: bool,
                source: bool,
            ) -> CommonSettings {
                let mut s = udp_settings(Side::Receiver, Transport::Udp);
                if multicast {
                    s.multicast_ip = PROP_MCAST.to_owned();
                }
                if interface {
                    s.interface_ip = PROP_IFACE.to_owned();
                }
                if source {
                    s.source_ip = PROP_SRC.to_owned();
                }
                s
            }

            fn splice(sdp: &str, settings: &CommonSettings) -> String {
                sdp::passthrough_with_overrides(
                    sdp,
                    &property_overrides_udp(settings),
                    DualLegPassthroughPolicy::RejectDualLeg,
                )
                .unwrap_or_else(|e| panic!("passthrough splice: {e}"))
            }

            fn expected_c(
                multicast: bool,
                interface: bool,
                baseline: &'static str,
            ) -> &'static str {
                if multicast {
                    PROP_MCAST
                } else if interface {
                    PROP_IFACE
                } else {
                    baseline
                }
            }

            fn assert_c_contains(spliced: &str, addr: &str) {
                let is_mcast = addr
                    .parse::<std::net::Ipv4Addr>()
                    .is_ok_and(|ip| ip.is_multicast());
                if is_mcast {
                    assert!(
                        spliced.contains(&format!("c=IN IP4 {addr}/")),
                        "expected multicast c= for {addr} in:\n{spliced}",
                    );
                } else {
                    assert!(
                        spliced.contains(&format!("c=IN IP4 {addr}\r\n"))
                            || spliced.contains(&format!("c=IN IP4 {addr}\n")),
                        "expected unicast c= for {addr} in:\n{spliced}",
                    );
                }
            }

            fn assert_source_filter(spliced: &str, expect: Option<(&str, &str)>) {
                match expect {
                    None => assert!(
                        !spliced.contains("a=source-filter:"),
                        "expected no source-filter in:\n{spliced}",
                    ),
                    Some((dest, src)) => {
                        let needle = format!("incl IN IP4 {dest} {src}");
                        assert!(
                            spliced.contains(&needle),
                            "expected source-filter containing `{needle}` in:\n{spliced}",
                        );
                    }
                }
            }

            fn assert_iface_attr(spliced: &str, expect: Option<&str>) {
                match expect {
                    None => assert!(
                        !spliced.contains("a=x-nvnmos-iface-ip:"),
                        "expected no x-nvnmos-iface-ip in:\n{spliced}",
                    ),
                    Some(ip) => assert!(
                        spliced.contains(&format!("a=x-nvnmos-iface-ip:{ip}")),
                        "expected x-nvnmos-iface-ip:{ip} in:\n{spliced}",
                    ),
                }
            }

            /// Multicast file + `interface-ip` only: `c=` becomes the
            /// property value (unicast), not the file's multicast
            /// group. This follows nmos-cpp `get_connection_address`
            /// but can surprise users who expected join-NIC-only.
            /// To keep the group, set `multicast-ip` or embed
            /// `a=x-nvnmos-iface-ip` in the file without setting the
            /// property.
            #[test]
            fn receiver_passthrough_multicast_file_interface_ip_only_rewrites_c() {
                let settings = receiver_settings(false, true, false);
                let spliced = splice(MCAST_ASM_SDP, &settings);
                assert_c_contains(&spliced, PROP_IFACE);
                assert!(
                    !spliced.contains(FILE_MCAST),
                    "multicast c= must be replaced when only interface-ip \
                     is set; got:\n{spliced}",
                );
                assert_iface_attr(&spliced, Some(PROP_IFACE));
            }

            #[test]
            fn receiver_passthrough_property_override_matrix() {
                for sdp_kind in [SdpKind::McastAsm, SdpKind::McastSsm, SdpKind::Unicast] {
                    for multicast in [false, true] {
                        for interface in [false, true] {
                            for source in [false, true] {
                                let settings = receiver_settings(multicast, interface, source);
                                let spliced = splice(sdp_kind.sdp(), &settings);
                                let c = expected_c(
                                    multicast,
                                    interface,
                                    sdp_kind.baseline_c(),
                                );

                                assert_c_contains(
                                    &spliced,
                                    c,
                                );

                                let sf = if source {
                                    Some((c, PROP_SRC))
                                } else if sdp_kind.baseline_has_ssm() {
                                    // Baseline SSM filter survives; `c=`
                                    // overrides rewrite its dest address.
                                    Some((c, FILE_SSM_SRC))
                                } else {
                                    None
                                };
                                assert_source_filter(&spliced, sf);

                                assert_iface_attr(
                                    &spliced,
                                    interface.then_some(PROP_IFACE),
                                );
                            }
                        }
                    }
                }
            }
        }

        /// All slots `None` when no property is set. Pins that
        /// the empty-string / zero "unset" sentinel convention
        /// flows through to the splice helper as "leave the
        /// file's value alone". The shared `settings()` fixture
        /// pre-fills `name` for IS-04 add-resource coverage; we
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
                auto_activate: true,
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

        /// Receiver with no `transport_file` and no `caps` stays
        /// misconfigured — receivers cannot use deferred mode.
        #[test]
        fn resolve_inner_config_udp_receiver_no_transport_file_and_no_caps_is_misconfigured() {
            let s = CommonSettings {
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
            match inner {
                InnerConfig::Fake { kind, .. } => {
                    assert_eq!(kind, FakeKind::Misconfigured);
                }
                other => panic!("expected misconfigured fake, got {other:?}"),
            }
        }

        /// Sender with no `transport_file` and no `caps` defers
        /// AddSender to READY→PAUSED (fake `NotConfigured`).
        #[test]
        fn resolve_inner_config_udp_sender_no_caps_no_file_is_not_configured() {
            let s = CommonSettings {
                destination_ip: "239.1.1.1".to_owned(),
                ..udp_settings(Side::Sender, Transport::Udp)
            };
            let (inner, text) = resolve_inner_config_udp(
                &cat(),
                "nmossink",
                &s,
                UdpVariant::V1,
                None,
            )
            .expect("no error");
            assert!(text.is_none());
            match inner {
                InnerConfig::Fake { kind, .. } => {
                    assert_eq!(kind, FakeKind::NotConfigured);
                }
                other => panic!("expected not-configured fake, got {other:?}"),
            }
        }

        #[test]
        fn resolve_inner_config_nvdsudp_sender_no_caps_no_file_is_not_configured() {
            let s = CommonSettings {
                destination_ip: "239.1.1.1".to_owned(),
                ..udp_settings(Side::Sender, Transport::NvDsUdp)
            };
            let (inner, text) = resolve_inner_config_nvdsudp(&cat(), "nmossink", &s, None)
                .expect("no error");
            assert!(text.is_none());
            assert!(matches!(
                inner,
                InnerConfig::Fake {
                    kind: FakeKind::NotConfigured,
                    ..
                }
            ));
        }

        /// `caps` supplied but no transport_file →
        /// `synthesise_or_passthrough_udp` builds an SDP from
        /// caps + transport_caps + IS-05 endpoint properties.
        /// The resolved config is now `Real`, not `Fake`.
        #[test]
        fn resolve_inner_config_udp_synthesises_unicast_receiver_from_caps_only() {
            let mut s = udp_settings(Side::Receiver, Transport::Udp);
            s.caps = Some(
                gst::Caps::from_str(
                    "audio/x-raw,format=S24BE,rate=48000,channels=2,layout=interleaved",
                )
                .unwrap(),
            );
            s.interface_ip = "192.0.2.30".to_owned();
            s.destination_port = 5004;
            s.auto_activate = true;
            let (inner, text) = resolve_inner_config_udp(
                &cat(),
                "nmossrc",
                &s,
                UdpVariant::V1,
                None,
            )
            .expect("unicast receiver caps synthesis");
            let text = text.expect("synthesised SDP");
            assert!(
                text.contains("c=IN IP4 192.0.2.30"),
                "unicast receiver c= comes from interface-ip, not multicast-ip:\n{text}",
            );
            assert!(
                !text.contains("source-filter:"),
                "unicast receiver synthesis omits source-filter when source-ip unset:\n{text}",
            );
            match inner {
                InnerConfig::Real(TransportConfig::Udp { media, .. }) => {
                    assert_eq!(media.primary.destination_ip, "192.0.2.30");
                    assert_eq!(media.primary.interface_ip.as_deref(), Some("192.0.2.30"));
                }
                other => panic!("expected Real(Udp) from unicast caps synthesis, got {other:?}"),
            }
        }

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
                auto_activate: true,
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
                auto_activate: true,
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
        fn activation_nvdsudp_parses_sdp_success() {
            let s = udp_settings(Side::Sender, Transport::NvDsUdp);
            let plan = make_activation_plan(
                &cat(),
                "nmossink",
                &s,
                &req(Side::Sender, Some(VIDEO_UDP_SDP)),
            );
            match plan.inner {
                InnerConfig::Real(TransportConfig::NvDsUdp { media, .. }) => {
                    assert_eq!(media.primary.destination_ip, "239.1.1.1");
                    assert_eq!(media.primary.destination_port, 5004);
                }
                other => panic!("expected Real(NvDsUdp), got {other:?}"),
            }
            assert!(matches!(plan.ack, ActivationAck::Success));
        }

        const ANC_SMPTE291_SDP: &str = concat!(
            "v=0\r\n",
            "o=- 0 0 IN IP4 192.0.2.10\r\n",
            "s=ANC\r\n",
            "t=0 0\r\n",
            "m=video 5006 RTP/AVP 100\r\n",
            "c=IN IP4 239.1.1.10/64\r\n",
            "a=rtpmap:100 smpte291/90000\r\n",
            "a=fmtp:100 exactframerate=60\r\n",
        );

        #[test]
        fn decide_udp_anc_smpte291_is_real() {
            for transport in [Transport::Udp, Transport::Udp2, Transport::NvDsUdp] {
                let s = udp_settings(Side::Receiver, transport);
                let inner = match transport {
                    Transport::NvDsUdp => decide_inner_config_nvdsudp(
                        "nmossrc",
                        &s,
                        Some(ANC_SMPTE291_SDP),
                        sdp::EssenceCrossCheckMode::Full,
                    )
                    .expect("ANC SDP parses"),
                    Transport::Udp => decide_inner_config_udp(
                        "nmossrc",
                        &s,
                        UdpVariant::V1,
                        Some(ANC_SMPTE291_SDP),
                        sdp::EssenceCrossCheckMode::Full,
                    )
                    .expect("ANC SDP parses"),
                    Transport::Udp2 => decide_inner_config_udp(
                        "nmossrc",
                        &s,
                        UdpVariant::V2,
                        Some(ANC_SMPTE291_SDP),
                        sdp::EssenceCrossCheckMode::Full,
                    )
                    .expect("ANC SDP parses"),
                    _ => unreachable!(),
                };
                match inner {
                    InnerConfig::Real(TransportConfig::NvDsUdp { media, .. })
                    | InnerConfig::Real(TransportConfig::Udp { media, .. }) => {
                        assert_eq!(media.format, FlowFormat::Data);
                        assert_eq!(
                            media.raw_caps.structure(0).unwrap().name(),
                            "meta/x-st-2038",
                        );
                    }
                    other => panic!("expected Real for {transport:?} ANC, got {other:?}"),
                }
            }
        }
    }

    mod add_deferred_sender {
        use super::super::super::add_deferred_sender;
        use super::*;
        use std::str::FromStr;

        fn sender_udp_settings(transport: Transport) -> CommonSettings {
            cat();
            CommonSettings {
                transport,
                destination_ip: "239.99.99.1".to_owned(),
                destination_port: 5004,
                source_ip: "192.0.2.10".to_owned(),
                auto_activate: true,
                ..settings(Side::Sender)
            }
        }

        fn video_peer_caps() -> gst::Caps {
            cat();
            gst::Caps::from_str(
                "video/x-raw,format=UYVP,width=1920,height=1080,framerate=50/1,\
                 interlace-mode=progressive",
            )
            .expect("video caps")
        }

        fn anc_peer_caps() -> gst::Caps {
            cat();
            gst::Caps::from_str("meta/x-st-2038,alignment=frame,framerate=30/1")
                .expect("anc caps")
        }

        #[test]
        fn synthesise_deferred_sender_udp_video_udp_builds_real_inner() {
            let s = sender_udp_settings(Transport::Udp);
            let (text, inner) = synthesise_deferred_sender_udp(
                "nmossink",
                &s,
                &video_peer_caps(),
            )
            .expect("synth");
            assert!(text.contains("m=video 5004 RTP/AVP 96"), "SDP:\n{text}");
            assert!(text.contains("c=IN IP4 239.99.99.1/"));
            assert!(matches!(inner, InnerConfig::Real(TransportConfig::Udp { .. })));
        }

        #[test]
        fn synthesise_deferred_sender_udp_anc_nvdsudp_builds_real_inner() {
            let s = sender_udp_settings(Transport::NvDsUdp);
            let (text, inner) = synthesise_deferred_sender_udp(
                "nmossink",
                &s,
                &anc_peer_caps(),
            )
            .expect("synth");
            assert!(text.contains("smpte291/90000"), "ANC SDP:\n{text}");
            assert!(matches!(inner, InnerConfig::Real(TransportConfig::NvDsUdp { .. })));
        }

        #[test]
        fn synthesise_deferred_sender_udp_unset_destination_ip_uses_zero_address() {
            let s = CommonSettings {
                destination_ip: String::new(),
                ..sender_udp_settings(Transport::Udp)
            };
            let (text, inner) = synthesise_deferred_sender_udp("nmossink", &s, &video_peer_caps())
                .expect("unset destination-ip synthesises");
            assert!(
                text.contains("c=IN IP4 0.0.0.0"),
                "expected unspecified c= line, got:\n{text}",
            );
            match inner {
                InnerConfig::Fake { kind, .. } => assert_eq!(kind, FakeKind::NotActive),
                InnerConfig::Real(_) => {
                    panic!("configuring-only sender keeps fake inner until activation")
                }
            }
        }

        #[test]
        fn add_deferred_sender_udp_unsupported_video_format_is_error() {
            let caps = gst::Caps::from_str("video/x-raw,format=I420,width=1920,height=1080")
                .expect("caps");
            let err = add_deferred_sender(
                &cat(),
                "nmossink",
                &sender_udp_settings(Transport::Udp),
                &Mutex::new(None),
                caps,
            )
            .expect_err("I420 unsupported for ST 2110 synthesis");
            assert!(
                format!("{err:#}").contains("synthesising SDP from peer caps"),
                "expected synthesis context: {err:#}",
            );
        }

        #[test]
        fn add_deferred_sender_udp_no_open_session_is_error() {
            let err = add_deferred_sender(
                &cat(),
                "nmossink",
                &sender_udp_settings(Transport::Udp2),
                &Mutex::new(None),
                video_peer_caps(),
            )
            .expect_err("missing session");
            assert!(
                format!("{err:#}").contains("no open session"),
                "expected no-open-session reason: {err:#}",
            );
        }
    }
}
