// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! RTP transport model for the UDP inner chain (one essence, one or two legs).
//!
//! SDP ([`crate::sdp`]) parses and builds the wire format. [`UdpMedia`] is the
//! denormalised logical stream the chain factories consume — not one SDP `m=`
//! block (ST 2022-7 may use several media lines for one shared essence).

use gstreamer as gst;

use crate::types::FlowFormat;

/// One logical RTP stream for the UDP inner chain: shared essence and
/// one or two network legs — everything the chain factories need to
/// instantiate the inner elements.
///
/// Populated from an SDP transport file ([`crate::sdp::parse_sdp`]),
/// from caps ([`crate::sdp::from_caps`]), or both after property splice.
/// That is separate from how many SDP `m=` lines describe the stream:
/// ST 2022-7 separate destination addresses mode uses two `m=` lines with
/// common media attributes; parsing folds these onto one [`UdpMedia`].
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
/// [`crate::session::CommonSettings::source_ip`] et seq. for the per-side wire
/// semantics. GObject properties map 1:1 to IS-05 **leg-0** transport
/// scalars (`destination-ip`, `interface-ip`, …); there is no `-2`
/// suffixed leg-2 property surface. ST 2022-7 uses a dual-`m=`
/// **transport file** on `transport=nvdsudp` (configuring passthrough
/// preserves both legs for AddSender / AddReceiver) and inner-element
/// redundancy properties on receive (comma-separated `st2022-7-streams`,
/// `local-iface-ip`, and `source-address` on `nvdsudpsrc`). Dual-leg
/// `transport-file*` on `udp` / `udp2` is rejected at element creation.
#[derive(Debug, Clone)]
pub(crate) struct UdpMedia {
    /// Essence family — selects the payloader / depayloader factory
    /// alongside [`crate::session::udp::UdpVariant`].
    pub(crate) format: FlowFormat,
    /// First **active** leg in SDP `m=` order (not necessarily `m=` line 0).
    /// Single-leg RTP has one active leg here; ST 2022-7 with only the
    /// second `m=` active also stores that leg alone on `primary`.
    /// When every leg is `a=inactive`, parsing still places the first `m=`
    /// block here so essence caps remain available; chain gating treats
    /// zero active legs as fake ([`crate::sdp::parse_sdp`]).
    pub(crate) primary: UdpLeg,
    /// Second **active** leg in SDP order, when two legs are active.
    /// `None` whenever fewer than two legs are active (`a=inactive`
    /// legs are omitted, not stored here).
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
    /// Local NIC from `a=x-nvnmos-iface-ip`. For chain wiring use
    /// [`Self::receiver_interface_ip`] or [`Self::sender_interface_ip`]
    /// — do not read this field directly except for SDP round-trip.
    pub(crate) interface_ip: Option<String>,
    /// SSM `a=source-filter:` include source-address.
    pub(crate) source_ip: Option<String>,
    /// Sender source port. Forwarded as `udpsink.bind-port`.
    /// Sender-only.
    pub(crate) source_port: Option<u16>,
}

impl UdpLeg {
    /// Receiver local NIC: `a=x-nvnmos-iface-ip` only.
    /// Used to populate `udpsrc` `multicast-iface` (via `iface_name_for_ip`).
    /// Used to populate `nvdsudpsrc` `local-iface-ip` and `ptp-src`.
    pub(crate) fn receiver_interface_ip(&self) -> Option<&str> {
        self.interface_ip.as_deref()
    }

    /// Sender local NIC: `a=x-nvnmos-iface-ip`, else SSM source-filter source.
    /// Used to populate `udpsink` `bind-address` and `multicast-iface` (via `iface_name_for_ip`).
    /// Used to populate `nvdsudpsink` `local-iface-ip` and `ptp-src`.
    pub(crate) fn sender_interface_ip(&self) -> Option<&str> {
        self.interface_ip.as_deref().or(self.source_ip.as_deref())
    }
}
