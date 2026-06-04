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
/// today parsing accepts a single `m=` line; redundancy will map multiple
/// lines onto one [`UdpMedia`].
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
    /// alongside [`crate::session::udp::UdpVariant`].
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
