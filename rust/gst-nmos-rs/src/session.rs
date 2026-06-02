// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Property validation and session lifecycle shared between `nmossrc`
//! and `nmossink`.
//!
//! Each element snapshots its `Settings` into [`CommonSettings`] under
//! its own settings lock, then calls [`validate_and_open`] /
//! [`close`] with that snapshot. The element holds the resulting
//! [`Session`](crate::daemon::Session) under a separate lock to keep
//! the settings critical section short.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, bail};
use gstreamer as gst;
use nvnmos_rpc::v1::Transport as ProtoTransport;

use crate::daemon::{ActivationHandler, ActivationRequest, Session};
use crate::domain::{self, DomainIdOrigin};
use crate::flow_def::{self, FlowDefBuildInput, FlowDefOverrides, ValueOrigin};
use crate::runtime::SHARED_RUNTIME;
use crate::sdp::{self, SdpOverrides};
use crate::types::{CapsMode, FlowFormat, Transport};

/// Open-session timeout. Aligned with the daemon's activation ack
/// timeout — same order of magnitude, no special meaning.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the snapshot came from `nmossink` or `nmossrc`. Surfaces in
/// error/log messages so validation failures point the user at the
/// right property name, and selects which gRPC AddSender/AddReceiver
/// call the session opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Side {
    Sender,
    Receiver,
}

impl Side {
    /// Property name the user sets to supply the NMOS name for this
    /// side of element — `sender-name` on `nmossink`, `receiver-name`
    /// on `nmossrc`. Used in validation error messages.
    fn name_property(self) -> &'static str {
        match self {
            Self::Sender => "sender-name",
            Self::Receiver => "receiver-name",
        }
    }

    /// Decode the proto-level `Side` enum value carried on
    /// `ActivationEvent.side`. Returns `None` for `SIDE_UNSPECIFIED`
    /// or any value not in the proto enum — the daemon never sends
    /// those, so the activation handler treats them as a bug and
    /// acks failure.
    pub(crate) fn try_from_proto(value: i32) -> Option<Self> {
        match nvnmos_rpc::v1::Side::try_from(value).ok()? {
            nvnmos_rpc::v1::Side::Sender => Some(Self::Sender),
            nvnmos_rpc::v1::Side::Receiver => Some(Self::Receiver),
            nvnmos_rpc::v1::Side::Unspecified => None,
        }
    }
}

/// Translate the GObject `Transport` enum to the wire enum.
///
/// `Mxl` carries data via the MXL Domain. `Udp` and `Udp2` reach
/// this helper once `validate_and_open` resolves an SDP into a
/// [`TransportConfig::Udp`] and the inner chain factories
/// instantiate `udpsrc` / `udpsink`. `NvDsUdp` is rejected
/// up-front because the DeepStream `nvdsudp*` elements aren't
/// wired in yet. The mapping is provided eagerly so the helper
/// stays exhaustive across all `Transport` variants.
pub(crate) fn transport_to_proto(t: Transport) -> ProtoTransport {
    match t {
        Transport::Mxl => ProtoTransport::Mxl,
        Transport::Udp | Transport::Udp2 | Transport::NvDsUdp => ProtoTransport::Rtp,
    }
}

// Shared `ParamSpec` blurbs for properties that exist on both
// `nmossink` and `nmossrc` with byte-identical wording. Hoisted to
// `session.rs` (next to `CommonSettings`) so the two elements can't
// drift, and so `gst-inspect-1.0 nmossink` and `gst-inspect-1.0
// nmossrc` print the same text for properties that aren't side-
// specific. The narrative form lives in the README's property
// table; properties whose blurb genuinely *does* differ between
// sender and receiver (e.g. `mxl-domain-path`, `transport-file`,
// `label`, `description`, `mxl-flow-id`, `caps`) keep their text
// inline in the respective `imp.rs`.

pub(crate) const DAEMON_URI_BLURB: &str =
    "gRPC endpoint for nvnmosd. Only `unix:/path/to/sock` URIs are \
     currently supported.";

pub(crate) const NODE_SEED_BLURB: &str =
    "NvNmos Node seed (node_config.seed). Required. Sessions sharing \
     this seed contribute to the same NMOS Node.";

pub(crate) const HTTP_PORT_BLURB: &str =
    "TCP port libnvnmos serves the NMOS HTTP APIs on \
     (node_config.http_port). 0 (the default) leaves libnvnmos on \
     the nmos-cpp per-API defaults (Node API on 3212, Connection \
     API on 3215). Non-zero collapses every HTTP API onto this \
     single port. Honoured only by the OpenSession that actually \
     creates the Node — when attaching to a pre-existing Node \
     (e.g. another nmossink / nmossrc opened first with the same \
     node-seed) this property is ignored, just like the rest of \
     node_config.";

pub(crate) const TRANSPORT_BLURB: &str =
    "Inner data path family. \
     `mxl`: MXL shared-memory transport (`mxlsrc` / `mxlsink`). \
     `udp`: ST 2110 over RTP/UDP via gst-plugins-good (`udpsrc` / \
     `udpsink` + the `rtpvrawpay` / `rtpL24pay` / `rtpsmpte291pay` \
     family). \
     `udp2`: ST 2110 over RTP/UDP via gst-plugins-rs (`udpsrc2` + \
     the `*pay2` / `*depay2` family where available, falling back \
     to gst-plugins-good per-element). \
     `nvdsudp`: reserved for DeepStream's `nvdsudp*` (kernel-bypass \
     plus PTP-aligned timing for strict ST 2110); not yet \
     implemented and rejected today.";

pub(crate) const MXL_DOMAIN_ID_BLURB: &str =
    "MXL Domain identifier (UUID) advertised in NMOS as \
     `urn:x-nvnmos:tag:mxl-domain-id` in the transport file. \
     Required when transport=mxl, but may be omitted if \
     `mxl-domain-path` points at a directory containing a \
     `domain_def.json` (AMWA BCP-007-03 WIP): the file's `id` is \
     then used. Overrides the transport file's tag when both are \
     supplied. Cross-checked against `domain_def.json` when both \
     are supplied (mismatch is an error — this is host-level \
     identity, not just labelling).";

pub(crate) const TRANSPORT_FILE_PATH_BLURB: &str =
    "Filesystem path read at NULL\u{2192}READY into `transport-file`. \
     Convenience for gst-launch; mutually exclusive with \
     `transport-file`.";

pub(crate) const TRANSPORT_CAPS_BLURB: &str =
    "Per-transport overrides (SDP fmtp-style). Typically empty for MXL.";

pub(crate) const TRANSPORT_PROPERTIES_BLURB: &str =
    "Overrides applied to the inner source or sink (`udpsrc`, `udpsink`, \
     `mxlsrc`, or `mxlsink`) every time the data-path chain is built. \
     Pass a `GstStructure` whose fields are GObject property names on that \
     inner source or sink — for example `properties,buffer-size=26214400`. \
     The structure name is not interpreted. Takes effect on the next chain \
     build, not immediately on the one currently in the chain.";

pub(crate) const PAY_PROPERTIES_BLURB: &str =
    "Overrides applied to the inner RTP payloader every time the UDP sender \
     chain is built. Same `GstStructure` syntax as `transport-properties`; \
     ignored on non-UDP transports (a warning is logged if non-empty). Takes \
     effect on the next chain build.";

pub(crate) const DEPAY_PROPERTIES_BLURB: &str =
    "Overrides applied to the inner RTP depayloader every time the UDP \
     receiver chain is built. Same `GstStructure` syntax as \
     `transport-properties`; ignored on non-UDP transports (a warning is \
     logged if non-empty). Takes effect on the next chain build.";

pub(crate) const AUTO_ACTIVATE_BLURB: &str =
    "Bring the inner `mxlsink` / `mxlsrc` up immediately at \
     NULL\u{2192}READY (or, for deferred-mode senders, READY\u{2192}PAUSED) \
     once the configuring flow_def has been resolved, and call \
     `SyncResourceState` on the daemon to advertise the resource as \
     active on IS-04/IS-05 without waiting for an IS-05 PATCH. \
     Default `false` gives canonical NMOS behaviour: the resource is \
     registered (so it appears on IS-04) but the data path stays on \
     the fake chain until an external IS-05 controller activates it.";

/// Snapshot of the properties needed to open a session, taken under
/// the per-element settings lock so the lock isn't held over the
/// blocking RPC.
#[derive(Debug, Clone)]
pub(crate) struct CommonSettings {
    pub(crate) daemon_uri: String,
    pub(crate) node_seed: String,
    /// See [`HTTP_PORT_BLURB`].
    pub(crate) http_port: u16,
    pub(crate) transport: Transport,
    /// Whether this snapshot came from `nmossink` (Sender) or `nmossrc`
    /// (Receiver). Pinned by the element that built the snapshot.
    pub(crate) side: Side,
    /// NMOS resource name within the Node, unique per side on the
    /// Node. A Sender on `nmossink` and a Receiver on `nmossrc` are
    /// permitted to share the same name; the daemon scopes its
    /// `by_name` index by `(node_seed, side, name)` and the activation
    /// callback surfaces the side alongside the name.
    pub(crate) name: String,
    /// MXL Domain identifier (UUID) advertised in NMOS via
    /// `urn:x-nvnmos:tag:mxl-domain-id` in the flow_def. If
    /// `mxl_domain_path` is also set and contains a `domain_def.json`
    /// (AMWA BCP-007-03 WIP), the file's `id` is cross-checked
    /// against this property — see [`crate::domain`].
    pub(crate) mxl_domain_id: String,
    /// Local filesystem path identifying the MXL Domain on this host.
    /// If the directory contains a `domain_def.json` its `id` is used
    /// to populate `mxl_domain_id` when the property is unset, or
    /// cross-checked against it when both are supplied. Fed into the
    /// inner `mxlsink` / `mxlsrc` `domain=` property.
    pub(crate) mxl_domain_path: String,
    /// MXL flow id (UUID) to bind the inner `mxlsink.flow-id=` or the
    /// matching `mxlsrc.{video,audio,data}-flow-id=`. Cross-checked
    /// against the transport file's top-level `id` when both are
    /// supplied; either source alone is enough.
    pub(crate) mxl_flow_id: String,
    /// Literal transport file contents (MXL `flow_def` JSON today).
    /// Convenient for programmatic callers (e.g. Rust/C apps that
    /// compute the flow_def in memory) but awkward to pass from
    /// `gst-launch-1.0` because the JSON contains newlines and
    /// quotes — those callers use `transport_file_path` instead.
    pub(crate) transport_file: String,
    /// Filesystem path that's read into `transport_file` at
    /// NULL→READY. Mutually exclusive with `transport_file`.
    pub(crate) transport_file_path: String,
    /// NMOS `label` for the synthesised flow_def. Optional: the
    /// builder falls back to the flow id when this is empty.
    pub(crate) label: String,
    /// NMOS `description` for the synthesised flow_def. Optional;
    /// omitted from the JSON when empty.
    pub(crate) description: String,
    /// Essence caps. On `nmossink`, when no `transport_file*` is
    /// supplied, the element synthesises a flow_def JSON from these
    /// caps plus the resolved property state
    /// (see [`crate::flow_def::from_caps`]). On `nmossrc`,
    /// the media-type structure name decides which `mxlsrc` flow-id
    /// slot receives `mxl-flow-id` and the caps are pinned on the
    /// ghost source pad so downstream caps queries see the concrete
    /// shape the flow will carry. When `transport_file*` is supplied
    /// the file is authoritative; for `nmossink` the caps are
    /// ignored; for `nmossrc` the caps-derived format is
    /// cross-checked against the file's `format` field.
    pub(crate) caps: Option<gst::Caps>,
    /// Per-transport overrides (`application/x-rtp,…` shape for the
    /// RTP transports; typically empty / unused for `mxl`). Carries
    /// the parameters that the user wants to override on the wire —
    /// principally RTP `payload`, audio `clock-rate`, and
    /// `a-ptime` (in milliseconds) — per the
    /// override-vs-cross-check rule agreed for startup-time SDP
    /// resolution:
    ///
    /// * **Override** when `transport-file*` is also supplied:
    ///   audio `clock-rate`, `a-ptime` / `a-maxptime`, and any
    ///   payload-type in the RTP dynamic range (96..=127, all
    ///   essence families). `transport-caps` wins; the file is
    ///   rewritten by the splice helper.
    /// * **Cross-check** when `transport-file*` is also supplied:
    ///   `encoding-name`, video / ANC `clock-rate` (always
    ///   90000), and any essence-shape parameter that also
    ///   appears on `caps`. Mismatch is a hard error.
    ///
    /// On the no-transport-file path the same fields seed the
    /// synthesised SDP (alongside [`caps`](Self::caps) and
    /// [`crate::sdp::defaults`]).
    ///
    /// Consumed at startup by [`property_overrides_udp`] (which
    /// feeds the override-class slots into the splice helper),
    /// by [`sdp::cross_check_essence`] in
    /// [`decide_inner_config_udp`], and by
    /// [`synthesise_or_passthrough_udp`] on the caps-only
    /// synthesis path (which threads it into
    /// [`sdp::from_caps`]'s `transport_caps` slot).
    pub(crate) transport_caps: Option<gst::Caps>,
    /// Controls whether the resource advertises narrow or wide caps
    /// in IS-04. See [`CapsMode`] for the full semantics. Honoured
    /// only when `side` is `Receiver` (driven by the
    /// `receiver-caps-mode` property on `nmossrc`); `nmossink` leaves
    /// it at [`CapsMode::Auto`].
    pub(crate) caps_mode: CapsMode,
    /// IS-05 RTP transport_params `source_ip` — string form. The
    /// IS-05 spec assigns this slot different semantics per
    /// resource:
    ///
    /// * Sender (`side == Sender`): local egress NIC IP. Emitted in
    ///   the configuring SDP as both the `a=source-filter:`
    ///   include-source (RFC 4607 SSM convention) and the
    ///   `a=x-nvnmos-iface-ip:` attribute, so a single property
    ///   value drives both wire slots.
    /// * Receiver (`side == Receiver`): SSM include-source — the
    ///   remote sender's IP. Emitted in the configuring SDP as the
    ///   `a=source-filter:` include-source.
    ///
    /// Empty string = unset (let the daemon resolve from
    /// `a=source-filter:` if present in `transport_file*`, else
    /// leave as the IS-05 `auto` sentinel for the daemon to fill
    /// at activation). Honoured only when `transport == Udp` /
    /// `Udp2`; ignored on the MXL path.
    ///
    pub(crate) source_ip: String,
    /// IS-05 RTP sender transport_params `source_port` — Sender-
    /// only. Local egress port for `udpsink` (drives both
    /// `udpsink.bind-port` and the SDP `a=x-nvnmos-src-port:`
    /// attribute). 0 = unset. Ignored on the Receiver side
    /// (IS-05 receiver schema doesn't define this slot).
    pub(crate) source_port: u16,
    /// IS-05 RTP sender transport_params `destination_ip` —
    /// Sender-only. Remote destination IP (unicast peer or
    /// multicast group). Becomes the `c=` line address in the
    /// configuring SDP and the `udpsink.host` property. Empty
    /// string = unset. Ignored on the Receiver side (receivers
    /// use `multicast_ip` + `interface_ip` instead).
    pub(crate) destination_ip: String,
    /// IS-05 RTP transport_params `destination_port`. Same name on
    /// both sides but with different semantics:
    ///
    /// * Sender: remote destination port (becomes `udpsink.port`
    ///   and the SDP `m=` port slot).
    /// * Receiver: local listen port (becomes `udpsrc.port`).
    ///
    /// 0 = unset (falls back to the SDP `m=` port if a transport
    /// file is supplied, else to [`crate::sdp::defaults::RTP_PORT`]).
    pub(crate) destination_port: u16,
    /// IS-05 RTP receiver transport_params `interface_ip` —
    /// Receiver-only. Local NIC IP used for the IGMP join
    /// (resolved to an interface name via
    /// [`crate::iface::iface_name_for_ip`] and threaded into
    /// `udpsrc.multicast-iface`). Also emitted in the configuring
    /// SDP as the `a=x-nvnmos-iface-ip:` attribute. Empty string =
    /// unset. Ignored on the Sender side (senders use `source_ip`
    /// for the same wire concept).
    pub(crate) interface_ip: String,
    /// IS-05 RTP receiver transport_params `multicast_ip` —
    /// Receiver-only. Multicast group to join (or empty for
    /// unicast reception). Becomes the `c=` line address in the
    /// configuring SDP and the `udpsrc.address` property. Empty
    /// string = unset (unicast / let the SDP / daemon resolve).
    /// Ignored on the Sender side (senders use `destination_ip`
    /// for the same wire concept).
    pub(crate) multicast_ip: String,
    /// Whether the element brings its inner `mxlsink` / `mxlsrc` up
    /// immediately at NULL→READY (or, for a deferred-mode sender,
    /// READY→PAUSED) once the configuring transport file has been
    /// resolved, and synchronises the daemon's IS-04/IS-05 state to
    /// match via `SyncResourceState`.
    ///
    /// `false` (default) gives canonical NMOS behaviour: the
    /// element registers the resource (so it appears on IS-04) but
    /// leaves the data path on the fake chain until an IS-05 PATCH
    /// against `/single/{senders,receivers}/{id}/staged` activates
    /// it. `true` is the "no-controller" shortcut for development
    /// and for pipelines whose flow identity is entirely property /
    /// transport-file driven.
    ///
    /// The toggle is orthogonal to how the configuring flow_def
    /// itself was obtained (property override of `mxl-flow-id`,
    /// supplied `transport-file*`, or caps→flow_def synthesis): as
    /// long as one of those routes produces a usable flow_def at
    /// NULL→READY (or READY→PAUSED for a deferred sender),
    /// `auto-activate=true` brings the inner up and informs the
    /// daemon; `auto-activate=false` leaves it for the controller.
    pub(crate) auto_activate: bool,
}

/// Outcome of resolving `transport_file` / `transport_file_path`.
/// `Some(text)` means a non-empty literal was supplied (directly or
/// loaded from the path); `None` means neither was set and no
/// resource will be registered.
fn resolve_transport_file(
    element: &str,
    settings: &CommonSettings,
) -> Result<Option<String>, anyhow::Error> {
    let inline = !settings.transport_file.is_empty();
    let path = !settings.transport_file_path.is_empty();
    if inline && path {
        bail!(
            "{element}: `transport-file` and `transport-file-path` are mutually exclusive; set at most one"
        );
    }
    if inline {
        Ok(Some(settings.transport_file.clone()))
    } else if path {
        let text = std::fs::read_to_string(&settings.transport_file_path).with_context(|| {
            format!(
                "{element}: reading `transport-file-path` = `{}`",
                settings.transport_file_path
            )
        })?;
        if text.is_empty() {
            bail!(
                "{element}: `transport-file-path` = `{}` is empty",
                settings.transport_file_path
            );
        }
        Ok(Some(text))
    } else {
        Ok(None)
    }
}

/// What the element should build on its data path after a successful
/// `validate_and_open`.
///
/// [`InnerConfig::Real`] carries everything the element needs to
/// instantiate a real transport chain (today only MXL, captured in
/// [`TransportConfig::Mxl`]). [`InnerConfig::Fake`] means the
/// resolved configuration didn't pin enough state to build a real
/// chain (e.g. no Flow id, no Domain path) and the element keeps
/// its fake data path in place (`fakesink` on the sink side, an
/// `appsrc` configured with the resolved essence caps on the
/// source side — see [`crate::inner`]). A later step
/// (caps→flow_def, IS-05 activation) will supply the missing pieces
/// and the bin will swap from fake to real.
#[derive(Debug, Clone)]
pub(crate) enum InnerConfig {
    Real(TransportConfig),
    Fake {
        /// One-line summary of which piece of state was missing.
        /// Logged at INFO so it's clear why the fake chain is in
        /// use.
        reason: String,
    },
}

/// Per-transport state needed to build a real chain. New transports
/// (NVDS-UDP, ...) get their own variants alongside the two
/// implemented today.
#[derive(Debug, Clone)]
pub(crate) enum TransportConfig {
    Mxl {
        domain_path: String,
        flow_id: String,
        /// Unspecified on `nmossink` — `mxlsink` has only one
        /// flow-id slot — and one of Video/Audio/Data on `nmossrc`.
        format: FlowFormat,
        /// Resolved `flow_def` JSON (when one is in play, whether
        /// supplied via `transport-file*` or synthesised from `caps`).
        /// Receivers reverse-map this into essence Caps and pin them
        /// on the ghost source pad so downstream caps queries see
        /// the concrete shape the flow will carry (rather than the
        /// broad `mxlsrc` pad template). Senders ignore it. `None`
        /// only when neither a transport file nor a synthesise-able
        /// caps + `mxl-flow-id` pairing was supplied at NULL→READY
        /// (e.g. deferred-mode Senders awaiting peer caps, or
        /// Receivers whose `mxl-flow-id` will arrive via IS-05 PATCH).
        transport_file: Option<String>,
    },
    /// OSS UDP/RTP transport. The inner chain is
    /// `rtp*pay ! udpsink` for senders and
    /// `udpsrc ! capsfilter(rtp_caps) ! rtp*depay ! capsfilter(raw_caps)?`
    /// for receivers. The exact element factory names dispatch on
    /// [`UdpVariant`].
    ///
    /// Constructed at runtime by `resolve_inner_config_udp` (in
    /// `validate_and_open`) and `decide_inner_config_udp` (in
    /// `make_activation_plan`) once the SDP transport file has
    /// been parsed by [`crate::sdp::parse_sdp`] (or synthesised
    /// from `caps` + properties via [`crate::sdp::from_caps`]).
    /// The chain factories ([`crate::inner::build_udpsink`] /
    /// [`crate::inner::build_udpsrc`]) instantiate real
    /// `udpsrc` / `udpsink` chains with the matching
    /// [`UdpVariant`]-selected RTP (de)payloader.
    Udp {
        variant: UdpVariant,
        media: UdpMedia,
        /// SDP transport file the daemon advertises on IS-04 (either
        /// the user-supplied one or the synthesised one). Retained
        /// verbatim for logs / diagnostics; the data the inner chain
        /// needs is denormalised into [`UdpMedia`].
        transport_file: Option<String>,
    },
}

impl TransportConfig {
    /// The transport-file text the daemon advertises on IS-04 for
    /// this configuration, in the format appropriate to the
    /// transport family (flow_def JSON for MXL, SDP for UDP).
    /// `None` when the configuration was synthesised purely from
    /// properties + caps and no resource is being advertised
    /// (deferred-mode awaiting peer caps).
    pub(crate) fn transport_file(&self) -> Option<&str> {
        match self {
            Self::Mxl { transport_file, .. } | Self::Udp { transport_file, .. } => {
                transport_file.as_deref()
            }
        }
    }
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

/// Validate the settings snapshot and open a session via the shared
/// tokio runtime. On success the session is stored under `session`
/// and the returned [`InnerConfig`] tells the element how to wire its
/// data path. `activation_handler` is forwarded to
/// [`Session::open`] to receive `ActivationEvent`s.
pub(crate) fn validate_and_open(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    session: &Mutex<Option<Session>>,
    activation_handler: ActivationHandler,
) -> Result<InnerConfig, anyhow::Error> {
    if settings.node_seed.is_empty() {
        bail!("{element}: `node-seed` is required");
    }
    if settings.name.is_empty() {
        bail!(
            "{element}: `{}` is required",
            settings.side.name_property()
        );
    }

    let resolved_transport_file = resolve_transport_file(element, settings)?;

    let (mut inner, transport_file) = match settings.transport {
        Transport::Mxl => {
            resolve_inner_config_mxl(cat, element, settings, resolved_transport_file)?
        }
        Transport::Udp => resolve_inner_config_udp(
            cat,
            element,
            settings,
            UdpVariant::V1,
            resolved_transport_file,
        )?,
        Transport::Udp2 => resolve_inner_config_udp(
            cat,
            element,
            settings,
            UdpVariant::V2,
            resolved_transport_file,
        )?,
        Transport::NvDsUdp => bail!(
            "{element}: transport=nvdsudp is not yet implemented — strict \
             ST 2110 receive/send via DeepStream's `nvdsudp` elements is \
             gated on ConnectX / Rivermax hardware; only `mxl`, `udp` and \
             `udp2` are implemented today"
        ),
    };

    inner = apply_auto_activate_gate(inner, settings.auto_activate);

    let transport = transport_to_proto(settings.transport);
    let side = settings.side;
    let name = settings.name.clone();

    let new_session = SHARED_RUNTIME
        .block_on(async {
            tokio::time::timeout(
                OPEN_TIMEOUT,
                Session::open(
                    &settings.daemon_uri,
                    &settings.node_seed,
                    settings.http_port,
                    side,
                    &name,
                    transport,
                    transport_file.as_deref(),
                    activation_handler,
                ),
            )
            .await
        })
        .with_context(|| format!("{element}: OpenSession against {} timed out", settings.daemon_uri))?
        .with_context(|| format!("{element}: OpenSession against {}", settings.daemon_uri))?;

    let resource_summary = match new_session.resource_id() {
        Some((handle, id)) => format!("resource registered: resource_handle={handle} resource_id={id}"),
        None => "no resource registered (transport-file unset)".to_owned(),
    };
    // For MXL, `mxl-domain-id` is already logged at resolution time
    // by `resolve_inner_config_mxl`; for UDP there's no equivalent
    // session-level identifier — the network params live on
    // `UdpMedia` and are summarised below.
    let inner_summary = match &inner {
        InnerConfig::Real(TransportConfig::Mxl { domain_path, flow_id, format, .. }) => {
            format!("inner data path: mxl (domain_path={domain_path:?}, flow_id={flow_id}, format={format:?})")
        }
        InnerConfig::Real(TransportConfig::Udp { variant, media, .. }) => {
            let leg_summary = |leg: &UdpLeg| {
                format!(
                    "{}:{} iface={:?} source_ip={:?} source_port={:?}",
                    leg.destination_ip,
                    leg.destination_port,
                    leg.interface_ip,
                    leg.source_ip,
                    leg.source_port,
                )
            };
            let mut s = format!(
                "inner data path: udp ({variant:?}, format={:?}, primary=[{}]",
                media.format,
                leg_summary(&media.primary),
            );
            if let Some(secondary) = &media.secondary {
                s.push_str(&format!(", secondary=[{}]", leg_summary(secondary)));
            }
            s.push(')');
            s
        }
        InnerConfig::Fake { reason } => {
            format!("inner data path: fake ({reason})")
        }
    };
    gst::info!(
        cat,
        "session opened: handle={} node_id={} created_node={} \
         (node_seed={}, side={:?}, name={}, transport={:?}); {}; {}",
        new_session.session_handle,
        new_session.node_id,
        new_session.created_node,
        settings.node_seed,
        side,
        settings.name,
        settings.transport,
        resource_summary,
        inner_summary,
    );

    *session.lock().unwrap() = Some(new_session);
    Ok(inner)
}

/// If the user supplied a `transport-file` (literal or path), pass
/// it through; otherwise, when `caps` is set and `mxl-flow-id` is
/// non-empty, synthesise a flow_def JSON document via
/// [`flow_def::from_caps`]. When *both* a transport file and
/// `caps` are set, the file is passed through and the caps are
/// cross-checked against the file's `format` further down the
/// validate path (a mismatch is a hard error, not silently dropped).
///
/// Senders and Receivers both go through synthesis: a Sender's
/// `flow_def` describes the Flow it produces; a Receiver's *configuring*
/// `flow_def` describes the essence shape this Receiver is configured
/// to accept (BCP-004-01 narrow Receiver Caps), with the `urn:x-nvnmos:tag:caps`
/// tag spliced in by `receiver-caps-mode` to indicate narrow vs wide.
/// The live transport file that arrives via IS-05 PATCH replaces the
/// subscription-relevant fields (mxl-flow-id, etc.) at activation time
/// but the configuring file is what the daemon uses for IS-04
/// advertisement at registration time.
///
/// Returns `Ok(None)` when nothing can be synthesised — neither a
/// transport file nor enough property state to build one. The element
/// then opens the session without a transport file and runs on the
/// fake chain until an IS-05 activation arrives.
fn synthesise_or_passthrough_mxl(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    resolved_mxl_domain_id: &str,
    resolved: Option<String>,
) -> Result<Option<String>, anyhow::Error> {
    match (resolved, settings.caps.as_ref()) {
        (Some(text), Some(_)) => {
            gst::debug!(
                cat,
                "{element}: transport-file set; `caps` will be cross-checked against the file's `format`"
            );
            Ok(Some(text))
        }
        (Some(text), None) => Ok(Some(text)),
        (None, Some(caps)) => {
            if settings.mxl_flow_id.is_empty() {
                gst::debug!(
                    cat,
                    "{element}: `caps` set but `mxl-flow-id` empty; deferring flow_def \
                     synthesis (the fake chain will be in use until an IS-05 \
                     activation supplies the flow id)"
                );
                return Ok(None);
            }
            let json = flow_def::from_caps(&FlowDefBuildInput {
                flow_id: &settings.mxl_flow_id,
                name: &settings.name,
                mxl_domain_id: resolved_mxl_domain_id,
                label: &settings.label,
                description: &settings.description,
                caps,
            })
            .with_context(|| format!("{element}: synthesising flow_def from caps"))?;
            gst::info!(
                cat,
                "{element}: synthesised flow_def from `caps` (side={:?})",
                settings.side,
            );
            Ok(Some(json))
        }
        (None, None) => Ok(None),
    }
}

/// UDP counterpart of [`synthesise_or_passthrough_mxl`]:
/// produce a configuring SDP at startup time, either by
/// passing through a user-supplied `transport-file*` or by
/// synthesising one from `caps` + `transport-caps` +
/// IS-05 endpoint properties + [`sdp::defaults`].
///
/// Returns `Ok(Some(text))` when either path produces an SDP,
/// `Ok(None)` when neither is possible (no transport file
/// **and** no `caps`) — the element then opens the session
/// without one and runs on the fake chain until an IS-05
/// activation arrives.
///
/// Precedence is identical to the MXL path: an explicit
/// transport file beats `caps`-driven synthesis, mirroring
/// the user spec "transport-file* takes priority over caps
/// at startup; activation-SDP wins over both at PATCH time
/// (handled in [`make_activation_plan`])".
///
/// Caps-only synthesis composes [`sdp::from_caps`] with the
/// CommonSettings snapshot, mapping the per-side IS-05
/// property vocabulary (Sender's `destination_ip` vs
/// Receiver's `multicast_ip`; Sender's `source_ip`-as-NIC vs
/// Receiver's distinct `interface_ip`) into the unified
/// [`sdp::SdpBuildInput`] vocabulary that
/// [`sdp::from_caps`] consumes.
fn synthesise_or_passthrough_udp(
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

/// Best-effort [`FlowFormat`] derived from the `caps` property.
/// Returns [`FlowFormat::Unspecified`] when `caps` is unset or the
/// first structure's media type isn't one of the recognised essence
/// shapes — the caller then falls through to the transport file's
/// `format` (if present) or to the fake chain.
fn caps_format(settings: &CommonSettings) -> FlowFormat {
    settings
        .caps
        .as_ref()
        .map(FlowFormat::from_caps)
        .unwrap_or(FlowFormat::Unspecified)
}

/// Build an [`SdpOverrides`] from the element's property snapshot
/// for the RTP transports. Mirrors [`property_overrides_mxl`] on the
/// MXL path:
///
/// * Empty-string properties map to `None` (i.e. "user did not set
///   this; leave the file's value alone").
/// * Zero ports map to `None` (`http-port`'s "unset" sentinel
///   convention, extended to all u16 port slots).
/// * `caps_mode` is passed through by value because
///   [`CapsMode::Auto`] is already the "no override" sentinel.
///
/// Per-side dispatch reflects the IS-05 schema's asymmetry:
///
/// * **Sender**: `settings.source_ip` is the local egress NIC IP
///   and feeds **both** [`SdpOverrides::source_ip`] (used by the
///   splice as the SDP `a=source-filter:` include-source — RFC
///   4607 SSM convention) and [`SdpOverrides::interface_ip`]
///   (used as `a=x-nvnmos-iface-ip:`). The two SDP slots carry
///   the same IP on a Sender because they're both saying "this
///   is where the sender egresses from", just for different
///   downstream consumers (SSM-aware receivers vs libnvnmos).
///   Receiver-only slots (`interface_ip`, `multicast_ip`) are
///   not read.
/// * **Receiver**: [`SdpOverrides::source_ip`] is the SSM
///   include-source (remote sender's IP) from
///   `settings.source_ip`; [`SdpOverrides::interface_ip`] is the
///   local NIC from `settings.interface_ip`;
///   [`SdpOverrides::destination_ip`] is the multicast group
///   from `settings.multicast_ip` (the SDP `c=` line wire slot
///   that IS-05 splits between sender's `destination_ip` and
///   receiver's `multicast_ip` per resource direction).
///   `source_port` is left unset (the IS-05 receiver schema
///   doesn't define a local source-port slot).
///
/// `transport_caps` populates the override-class slots
/// (`payload_type`, `audio_clock_rate`, `a_ptime`,
/// `a_maxptime`) by reading the corresponding fields from
/// `application/x-rtp,...` caps. Out-of-range / missing values
/// drop silently here — pt's RFC 3551 range check fires
/// downstream in [`sdp::passthrough_with_overrides`], where the error
/// can be attributed cleanly to the SDP transform rather than
/// to a property-setter side-effect. Cross-check fields
/// (`encoding-name`, video/ANC `clock-rate`, essence shape)
/// don't pass through this builder — [`sdp::cross_check_essence`]
/// reads them from `settings.transport_caps` directly.
fn property_overrides_udp(settings: &CommonSettings) -> SdpOverrides<'_> {
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

/// Build a [`FlowDefOverrides`] from the element's property snapshot.
/// Empty-string properties map to `None` (i.e. "user did not set
/// this; leave the file's value alone"). `mxl_domain_id` is taken
/// from the domain resolution result, not the raw property, so the
/// `domain_def.json`-derived value also flows into the splice when
/// the user didn't set the property directly.
fn property_overrides_mxl<'a>(
    settings: &'a CommonSettings,
    resolved_mxl_domain_id: &'a str,
) -> FlowDefOverrides<'a> {
    fn opt(s: &str) -> Option<&str> {
        if s.is_empty() { None } else { Some(s) }
    }
    FlowDefOverrides {
        flow_id: opt(&settings.mxl_flow_id),
        label: opt(&settings.label),
        description: opt(&settings.description),
        name: opt(&settings.name),
        mxl_domain_id: opt(resolved_mxl_domain_id),
        caps_mode: settings.caps_mode,
    }
}

fn log_flow_origin(cat: &gst::DebugCategory, field: &str, origin: ValueOrigin) {
    match origin {
        ValueOrigin::Property => gst::debug!(cat, "{field} from property; no transport file constraint"),
        ValueOrigin::File => gst::info!(cat, "{field} taken from transport file"),
        ValueOrigin::Both => gst::debug!(cat, "{field} cross-checked against transport file"),
        ValueOrigin::None => gst::debug!(cat, "{field} not supplied by either source"),
    }
}

/// Decide whether the element can build a real `mxlsink` / `mxlsrc`
/// chain or has to fall back to its fake chain. Both sides need a
/// non-empty Domain path and a non-empty flow id; the receiver
/// additionally needs a specific [`FlowFormat`] (because `mxlsrc`
/// has separate `video-flow-id` / `audio-flow-id` / `data-flow-id`
/// properties).
fn decide_inner_config_mxl(
    settings: &CommonSettings,
    flow: &flow_def::FlowResolution,
    transport_file: Option<&str>,
) -> InnerConfig {
    if settings.mxl_domain_path.is_empty() {
        return InnerConfig::Fake {
            reason: "`mxl-domain-path` unset".to_owned(),
        };
    }
    if flow.id.is_empty() {
        return InnerConfig::Fake {
            reason: "`mxl-flow-id` unset (neither property nor transport file supplied it)".to_owned(),
        };
    }
    if settings.side == Side::Receiver && flow.format == FlowFormat::Unspecified {
        return InnerConfig::Fake {
            reason:
                "`caps` media-type unrecognised or unset on nmossrc \
                 (neither caps nor transport file pinned a flow format)"
                    .to_owned(),
        };
    }
    InnerConfig::Real(TransportConfig::Mxl {
        domain_path: settings.mxl_domain_path.clone(),
        flow_id: flow.id.clone(),
        format: flow.format,
        transport_file: transport_file.map(str::to_owned),
    })
}

/// UDP sibling of [`decide_inner_config_mxl`]. Returns
/// [`InnerConfig::Fake`] when there's no SDP to parse (deferred
/// mode awaiting IS-05 PATCH); otherwise parses the SDP via
/// [`sdp::parse_sdp`] and packages the resulting [`UdpMedia`] into
/// a [`TransportConfig::Udp`]. SDP parse errors propagate as
/// `Err` so the caller can ack-fail with attribution rather than
/// silently downgrading to the fake chain (a malformed SDP is a
/// real misconfiguration, not a "wait for more state to arrive"
/// case).
fn decide_inner_config_udp(
    element: &str,
    settings: &CommonSettings,
    variant: UdpVariant,
    transport_file: Option<&str>,
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
    sdp::cross_check_essence(&media, settings.caps.as_ref(), settings.transport_caps.as_ref())
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

/// Transport-specific setup-time work for [`Transport::Mxl`]:
/// domain-id resolution, transport-file synthesis from `caps`,
/// property-overrides splice, and flow_def cross-checking. Extracted
/// from [`validate_and_open`] so the top-level function reads as a
/// clean transport dispatch.
///
/// Returns the resolved [`InnerConfig`] plus the transport-file
/// text the daemon will be handed at `OpenSession` time (the
/// post-synthesis, post-splice version of the input). The
/// caller applies [`apply_auto_activate_gate`] separately.
fn resolve_inner_config_mxl(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    resolved_transport_file: Option<String>,
) -> Result<(InnerConfig, Option<String>), anyhow::Error> {
    let domain_resolution =
        domain::resolve_mxl_domain_id(&settings.mxl_domain_id, &settings.mxl_domain_path)
            .with_context(|| format!("{element}: resolving MXL Domain identity"))?;
    if domain_resolution.id.is_empty() {
        bail!(
            "{element}: `mxl-domain-id` is required when transport=mxl \
             (set the property directly or supply an `mxl-domain-path` whose `domain_def.json` provides the id)"
        );
    }
    match domain_resolution.origin {
        DomainIdOrigin::Property => gst::debug!(
            cat,
            "mxl-domain-id from property; no `domain_def.json` consulted",
        ),
        DomainIdOrigin::DomainDef => gst::info!(
            cat,
            "mxl-domain-id taken from `domain_def.json` at `{}`",
            settings.mxl_domain_path,
        ),
        DomainIdOrigin::Both => gst::debug!(
            cat,
            "mxl-domain-id cross-checked against `domain_def.json` at `{}`",
            settings.mxl_domain_path,
        ),
        DomainIdOrigin::None => unreachable!("empty id rejected above"),
    }

    let transport_file = synthesise_or_passthrough_mxl(
        cat,
        element,
        settings,
        &domain_resolution.id,
        resolved_transport_file,
    )?;

    // Property-overrides-file: splice any user-set identity/cosmetic
    // properties (name, flow_id, mxl-domain-id, label, description,
    // receiver-caps-mode) into the transport file before the daemon
    // sees it. `caps` and `transport-caps` remain cross-checked by
    // `resolve_mxl_flow_meta` below — they describe the essence
    // shape and a mismatch is a real error.
    let transport_file = match transport_file {
        Some(text) => Some(
            flow_def::splice_overrides(&text, &property_overrides_mxl(settings, &domain_resolution.id))
                .with_context(|| format!("{element}: splicing property overrides into transport file"))?,
        ),
        None => None,
    };

    let caps_format = caps_format(settings);
    let flow = flow_def::resolve_mxl_flow_meta(
        &settings.mxl_flow_id,
        caps_format,
        transport_file.as_deref(),
    )
    .with_context(|| format!("{element}: resolving MXL flow id / format"))?;
    log_flow_origin(cat, "mxl-flow-id", flow.id_origin);
    log_flow_origin(cat, "caps format", flow.format_origin);

    let mut inner = decide_inner_config_mxl(settings, &flow, transport_file.as_deref());
    // Deferred-mode case (sender only): no resource is going to be
    // registered at NULL→READY because neither `transport-file*` nor
    // `caps` was supplied. Keep the fake chain so we don't bring
    // `mxlsink` up against an unregistered Flow (which would fail to
    // preroll); the inner is swapped to `mxlsink` only after
    // `register_deferred` registers the Sender at READY→PAUSED.
    if transport_file.is_none()
        && settings.side == Side::Sender
        && matches!(inner, InnerConfig::Real(_))
    {
        inner = InnerConfig::Fake {
            reason: "deferred — peer caps will drive registration at READY\u{2192}PAUSED"
                .to_owned(),
        };
    }

    Ok((inner, transport_file))
}

/// Transport-specific setup-time work for [`Transport::Udp`] /
/// [`Transport::Udp2`]: synthesise (or pass through) an SDP via
/// [`synthesise_or_passthrough_udp`], splice property overrides
/// via [`sdp::passthrough_with_overrides`] for user-supplied transport
/// files, parse the result via
/// [`sdp::parse_sdp`] inside [`decide_inner_config_udp`], and
/// package the resulting [`UdpMedia`] into a
/// [`TransportConfig::Udp`].
///
/// Precedence mirrors the MXL path:
///
/// * Explicit `transport-file*` → passthrough (caps cross-check
///   against the file's essence shape via
///   [`sdp::cross_check_essence`] in [`decide_inner_config_udp`]).
/// * `caps` only (no `transport-file*`) → synthesise an SDP from
///   `caps` + `transport_caps` + IS-05 endpoint properties +
///   [`sdp::defaults`] via [`sdp::from_caps`].
/// * Neither → no SDP; [`decide_inner_config_udp`] returns
///   [`InnerConfig::Fake`] so the element waits for an IS-05
///   PATCH to supply everything.
///
/// Activation-time SDP supersedes both at PATCH time
/// (see [`make_activation_plan`]).
fn resolve_inner_config_udp(
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
    let inner =
        decide_inner_config_udp(element, settings, variant, resolved_transport_file.as_deref())?;
    Ok((inner, resolved_transport_file))
}

/// Honour `auto-activate` at setup time. When `auto_activate` is
/// `false` and `decide_inner_config_mxl` / `decide_inner_config_udp`
/// would have produced a real transport chain, downgrade to
/// [`InnerConfig::Fake`] so the data path stays inactive until an
/// IS-05 PATCH activates the resource (the canonical NMOS path).
///
/// The resource registration itself isn't affected — that's driven
/// by whether a configuring transport file is in play, not by the
/// gate — so a resource opened with `auto-activate=false` still
/// appears on IS-04 immediately, ready to be PATCHed.
fn apply_auto_activate_gate(inner: InnerConfig, auto_activate: bool) -> InnerConfig {
    if !auto_activate && matches!(inner, InnerConfig::Real(_)) {
        return InnerConfig::Fake {
            reason: "auto-activate=false; waiting for IS-05 PATCH to activate".to_owned(),
        };
    }
    inner
}

/// Register a Sender via the deferred-mode path: synthesise a
/// flow_def from upstream peer caps and call `AddSender` against a
/// session that was opened without one. Used by `nmossink` from
/// inside `change_state(ReadyToPaused)` when neither `transport-file*`
/// nor `caps` was set at NULL→READY.
///
/// `peer_caps` is what `gst_pad_peer_query_caps()` returned, before
/// fixation. The helper fixates internally and rejects ANY / EMPTY
/// caps with a clear, user-facing error message telling them to
/// declare `caps=…` or insert a `capsfilter` upstream — that's the
/// same recipe the plan doc spells out for pipelines where the peer
/// query can't fix caps (h264parse pre-data, etc.).
///
/// Returns the [`InnerConfig`] the element should install on the
/// data path; today always [`InnerConfig::Real`] on success because
/// deferred-mode registration is only attempted when `mxl-domain-path`
/// is set (the fake chain is the alternative the caller picks when
/// this helper isn't called).
pub(crate) fn register_deferred(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    session: &Mutex<Option<Session>>,
    peer_caps: gst::Caps,
) -> Result<InnerConfig, anyhow::Error> {
    if settings.transport != Transport::Mxl {
        bail!(
            "{element}: deferred registration unsupported for transport `{:?}`",
            settings.transport
        );
    }
    if settings.side != Side::Sender {
        // Receiver deferred mode is explicitly out of scope (plan doc:
        // “`nmossrc` cannot use deferred mode — there is no peer to
        // query.”). Reject so we don't accidentally try.
        bail!("{element}: deferred registration is sender-only");
    }

    if peer_caps.is_empty() {
        bail!(
            "{element}: deferred registration: upstream peer offered no caps. \
             Declare `caps=\"…\"` on the element or insert a `capsfilter` \
             upstream so the element knows what flow_def to register."
        );
    }
    if peer_caps.is_any() {
        bail!(
            "{element}: deferred registration: upstream peer offered ANY caps \
             (likely no negotiated caps yet — e.g. `fakesrc` with no upstream \
             capsfilter). Declare `caps=\"…\"` on the element or insert a \
             `capsfilter` upstream so the element knows what flow_def to register."
        );
    }

    // Fixate the (possibly under-constrained) peer caps into a single,
    // concrete shape — the same operation any sink performs to decide
    // its negotiated caps. The fixated caps drive the flow_def
    // builder.
    let mut fixated = peer_caps;
    fixated.fixate();
    gst::info!(
        cat,
        "{element}: deferred mode: peer caps fixated to `{fixated}`",
    );

    let domain_resolution =
        domain::resolve_mxl_domain_id(&settings.mxl_domain_id, &settings.mxl_domain_path)
            .with_context(|| {
                format!("{element}: resolving MXL Domain identity for deferred registration")
            })?;
    if domain_resolution.id.is_empty() {
        bail!(
            "{element}: deferred registration: `mxl-domain-id` is required \
             (set the property directly or supply an `mxl-domain-path` whose \
             `domain_def.json` provides the id)"
        );
    }

    let json = flow_def::from_caps(&FlowDefBuildInput {
        flow_id: &settings.mxl_flow_id,
        name: &settings.name,
        mxl_domain_id: &domain_resolution.id,
        label: &settings.label,
        description: &settings.description,
        caps: &fixated,
    })
    .with_context(|| format!("{element}: synthesising flow_def from peer caps"))?;
    gst::info!(cat, "{element}: deferred mode: synthesised flow_def");

    let flow = flow_def::resolve_mxl_flow_meta(
        &settings.mxl_flow_id,
        FlowFormat::from_caps(&fixated),
        Some(&json),
    )
    .with_context(|| {
        format!("{element}: resolving MXL flow id / format for deferred registration")
    })?;
    let inner = apply_auto_activate_gate(
        decide_inner_config_mxl(settings, &flow, Some(&json)),
        settings.auto_activate,
    );

    let transport = transport_to_proto(settings.transport);
    let side = settings.side;
    let name = settings.name.clone();
    // Take the Session out of the std::Mutex before doing async work
    // (clippy's `await_holding_lock` lint, same pattern `close()` uses
    // for the symmetrical CloseSession call). The session is put back
    // afterwards whether AddSender succeeded or failed so READY→NULL
    // still has something to close.
    let mut taken = session.lock().unwrap().take().ok_or_else(|| {
        anyhow::anyhow!(
            "{element}: deferred registration but no open session — was NULL→READY skipped?"
        )
    })?;
    let rpc_result = SHARED_RUNTIME.block_on(async {
        tokio::time::timeout(
            OPEN_TIMEOUT,
            taken.add_resource(side, &name, transport, &json),
        )
        .await
        .with_context(|| format!("{element}: AddSender for deferred registration timed out"))?
        .with_context(|| format!("{element}: AddSender for deferred registration"))
    });
    let summary = taken
        .resource_id()
        .map(|(h, id)| format!("resource_handle={h} resource_id={id}"))
        .unwrap_or_else(|| "<no resource id>".to_owned());
    *session.lock().unwrap() = Some(taken);
    rpc_result?;

    gst::info!(
        cat,
        "{element}: deferred registration complete: {summary}; inner data path: {:?}",
        inner,
    );
    Ok(inner)
}

/// Tell the daemon to sync its IS-04/IS-05 view of the registered
/// resource to "active" (`master_enable: true`) with `transport_file`
/// as the live configuration. Used by the `auto-activate=true` path
/// after the element has already swapped its inner `mxlsink` /
/// `mxlsrc` directly from the resolved configuring flow_def.
///
/// Pass `None` for `transport_file` to sync to "inactive" (the
/// reverse direction, for symmetry — currently unused; the element
/// closes the session at READY→NULL which the daemon treats as a
/// full resource teardown, so explicit deactivation hasn't been
/// needed yet).
///
/// Logs and returns without an error on `DaemonError::NoResource`
/// (caller bug guard — the inner was somehow swapped to `mxlsink` /
/// `mxlsrc` without `AddSender` / `AddReceiver` having succeeded
/// first; in practice unreachable since `decide_inner_config` plus
/// the `auto-activate` gate only let that happen after a successful
/// registration). Other RPC failures are returned so the caller can
/// log them without forcing the inner swap itself to roll back.
pub(crate) fn sync_active(
    cat: &gst::DebugCategory,
    element: &str,
    session: &Mutex<Option<Session>>,
    transport_file: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut taken = session.lock().unwrap().take().ok_or_else(|| {
        anyhow::anyhow!("{element}: SyncResourceState but no open session")
    })?;
    let rpc_result = SHARED_RUNTIME.block_on(async {
        tokio::time::timeout(OPEN_TIMEOUT, taken.sync_resource_state(transport_file)).await
    });
    let resource_summary = taken
        .resource_id()
        .map(|(h, id)| format!("resource_handle={h} resource_id={id}"))
        .unwrap_or_else(|| "<no resource id>".to_owned());
    *session.lock().unwrap() = Some(taken);
    match rpc_result {
        Ok(Ok(())) => {
            gst::info!(
                cat,
                "{element}: auto-activate sync complete (active={}, {resource_summary})",
                transport_file.is_some(),
            );
            Ok(())
        }
        Ok(Err(crate::daemon::DaemonError::NoResource)) => {
            gst::warning!(
                cat,
                "{element}: auto-activate sync skipped — session has no resource yet ({resource_summary})",
            );
            Ok(())
        }
        Ok(Err(e)) => Err(anyhow::Error::new(e).context(format!(
            "{element}: SyncResourceState against the daemon ({resource_summary})"
        ))),
        Err(_elapsed) => Err(anyhow::anyhow!(
            "{element}: SyncResourceState against the daemon timed out ({resource_summary})"
        )),
    }
}

/// Drop the session and tell the daemon to close it. Logged-only on
/// error so state-change cleanup always succeeds.
pub(crate) fn close(cat: &gst::DebugCategory, element: &str, session: &Mutex<Option<Session>>) {
    let to_close = session.lock().unwrap().take();
    if let Some(s) = to_close {
        let handle = s.session_handle.clone();
        let result = SHARED_RUNTIME.block_on(s.close());
        match result {
            Ok(()) => gst::info!(cat, "session closed: handle={handle}"),
            Err(e) => gst::warning!(cat, "{element}: CloseSession (handle={handle}): {e}"),
        }
    }
}

/// What an [`ActivationRequest`] resolves to once the element re-runs
/// the same identity / flow cross-checks `validate_and_open` did at
/// NULL→READY, but with the event's `transport_file` substituted in.
///
/// `inner` is what the element should install on the data path;
/// `ack` is what the element should report to the daemon via
/// `AckActivation` once the swap completes. Deactivations always
/// ack success; failed activations swap to the fake chain but ack
/// failure so the IS-05 controller knows the resource is not live.
#[derive(Debug)]
pub(crate) struct ActivationPlan {
    pub(crate) inner: InnerConfig,
    pub(crate) ack: ActivationAck,
}

/// Two variants matching the proto `AckActivationRequest` shape
/// (`bool success`, `string failure_reason`). The element produces
/// one of these, the activation task forwards it.
#[derive(Debug, Clone)]
pub(crate) enum ActivationAck {
    Success,
    Failure { reason: String },
}

/// Resolve an [`ActivationRequest`] into an [`ActivationPlan`].
///
/// Logic:
///
/// * `req.side` must match the element's own [`Side`]. Mismatches
///   indicate a daemon-routing bug; we swap to the fake chain and
///   ack failure.
///
/// * `req.transport_file.is_none()` is a deactivation: swap to the
///   fake chain and ack **success**.
///
/// * Otherwise dispatch on [`settings.transport`]:
///
///   - `Mxl`: re-resolve `mxl-domain-id` (re-runs the
///     `domain_def.json` cross-check) and the flow id/format
///     (`flow_def::resolve_mxl_flow_meta` against the new
///     `transport_file`), then run [`decide_inner_config_mxl`].
///   - `Udp` / `Udp2`: parse the SDP via [`sdp::parse_sdp`] and
///     run [`decide_inner_config_udp`]. SDP parse errors → fake
///     chain + failure ack with attribution.
///   - `NvDsUdp`: not implemented; fake chain + failure ack.
///
/// * If the chosen `decide_inner_config_*` returns
///   [`InnerConfig::Real`], ack success; if it returns
///   [`InnerConfig::Fake`] we have a live transport file but
///   can't bring up the real chain (typically `mxl-domain-path` is
///   unset on this host, or — for UDP — IS-05 PATCH delivered an
///   empty SDP) — swap to the fake chain but ack **failure** so
///   the controller surfaces the misconfiguration.
pub(crate) fn make_activation_plan(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    req: &ActivationRequest,
) -> ActivationPlan {
    if req.side != settings.side {
        return ActivationPlan {
            inner: InnerConfig::Fake {
                reason: "activation side mismatch".to_owned(),
            },
            ack: ActivationAck::Failure {
                reason: format!(
                    "{element}: ActivationEvent side={:?} does not match element side={:?}",
                    req.side, settings.side,
                ),
            },
        };
    }

    let Some(transport_file) = req.transport_file.as_deref() else {
        gst::info!(
            cat,
            "{element}: activation is a deactivation (resource_handle={}); \
             swapping to fake chain",
            req.resource_handle,
        );
        return ActivationPlan {
            inner: InnerConfig::Fake {
                reason: "deactivation".to_owned(),
            },
            ack: ActivationAck::Success,
        };
    };

    let inner = match settings.transport {
        Transport::Mxl => match resolve_activation_inner_mxl(cat, element, settings, transport_file) {
            Ok(inner) => inner,
            Err(plan) => return *plan,
        },
        Transport::Udp => match decide_inner_config_udp(
            element,
            settings,
            UdpVariant::V1,
            Some(transport_file),
        ) {
            Ok(inner) => inner,
            Err(e) => {
                return ActivationPlan {
                    inner: InnerConfig::Fake {
                        reason: "SDP transport file rejected".to_owned(),
                    },
                    ack: ActivationAck::Failure {
                        reason: format!("{element}: parsing activation SDP: {e:#}"),
                    },
                };
            }
        },
        Transport::Udp2 => match decide_inner_config_udp(
            element,
            settings,
            UdpVariant::V2,
            Some(transport_file),
        ) {
            Ok(inner) => inner,
            Err(e) => {
                return ActivationPlan {
                    inner: InnerConfig::Fake {
                        reason: "SDP transport file rejected".to_owned(),
                    },
                    ack: ActivationAck::Failure {
                        reason: format!("{element}: parsing activation SDP: {e:#}"),
                    },
                };
            }
        },
        Transport::NvDsUdp => {
            return ActivationPlan {
                inner: InnerConfig::Fake {
                    reason: "transport=nvdsudp not implemented".to_owned(),
                },
                ack: ActivationAck::Failure {
                    reason: format!(
                        "{element}: activation rejected — transport=nvdsudp is not yet \
                         implemented; strict ST 2110 send/receive via DeepStream's \
                         `nvdsudp` elements is gated on ConnectX / Rivermax hardware",
                    ),
                },
            };
        }
    };

    let ack = match &inner {
        InnerConfig::Real(_) => ActivationAck::Success,
        // Per design: if the activation supplies a live transport file
        // but the element can't bring up the real chain (typically
        // `mxl-domain-path` is unset), ack failure so the controller
        // sees the resource as misconfigured rather than silently
        // deactivated.
        InnerConfig::Fake { reason } => ActivationAck::Failure {
            reason: format!(
                "{element}: activation cannot bring up inner data path: {reason}"
            ),
        },
    };

    ActivationPlan { inner, ack }
}

/// MXL branch of [`make_activation_plan`]: re-resolve domain id +
/// flow meta against the activation's transport file, then run
/// [`decide_inner_config_mxl`]. Returns `Ok(InnerConfig)` on
/// success; on failure returns a fully-formed [`ActivationPlan`]
/// (fake inner + failure ack) so the caller can short-circuit with
/// the right error attribution.
///
/// The `Err` variant is boxed because the rare failure path's
/// `ActivationPlan` (~240 B) would otherwise dominate the
/// `Result`'s stack footprint on the common `Ok` path.
fn resolve_activation_inner_mxl(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    transport_file: &str,
) -> Result<InnerConfig, Box<ActivationPlan>> {
    let domain_resolution =
        match domain::resolve_mxl_domain_id(&settings.mxl_domain_id, &settings.mxl_domain_path) {
            Ok(r) => r,
            Err(e) => {
                return Err(Box::new(ActivationPlan {
                    inner: InnerConfig::Fake {
                        reason: "mxl-domain-id resolution failed".to_owned(),
                    },
                    ack: ActivationAck::Failure {
                        reason: format!(
                            "{element}: resolving MXL Domain identity for activation: {e:#}"
                        ),
                    },
                }));
            }
        };
    if domain_resolution.id.is_empty() {
        return Err(Box::new(ActivationPlan {
            inner: InnerConfig::Fake {
                reason: "mxl-domain-id unresolved".to_owned(),
            },
            ack: ActivationAck::Failure {
                reason: format!(
                    "{element}: activation rejected — `mxl-domain-id` is not resolvable on this \
                     host (neither the property nor `mxl-domain-path`/`domain_def.json` \
                     supplied an id)",
                ),
            },
        }));
    }
    match domain_resolution.origin {
        DomainIdOrigin::Property | DomainIdOrigin::DomainDef | DomainIdOrigin::Both => gst::debug!(
            cat,
            "{element}: activation mxl-domain-id resolved (origin={:?})",
            domain_resolution.origin,
        ),
        DomainIdOrigin::None => unreachable!("empty id handled above"),
    }

    // Activation: the daemon's transport file is authoritative. Pass
    // an empty `property_id` so the file always wins silently (the
    // element's `mxl-flow-id` property is just a NULL→READY default;
    // an IS-05 PATCH legitimately replaces it). The `caps` format
    // cross-check stays because a v210 video activation arriving at
    // an `nmossrc` configured for audio is a real misconfiguration
    // the element must ack-fail.
    let flow = match flow_def::resolve_mxl_flow_meta(
        "",
        caps_format(settings),
        Some(transport_file),
    ) {
        Ok(r) => r,
        Err(e) => {
            return Err(Box::new(ActivationPlan {
                inner: InnerConfig::Fake {
                    reason: "flow_def resolution failed".to_owned(),
                },
                ack: ActivationAck::Failure {
                    reason: format!(
                        "{element}: resolving MXL flow id / format from activation \
                         transport file: {e:#}"
                    ),
                },
            }));
        }
    };

    Ok(decide_inner_config_mxl(settings, &flow, Some(transport_file)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE_SEED: &str = "test-seed";
    const FLOW_ID_A: &str = "00000000-0000-0000-0000-000000000001";
    const FLOW_ID_B: &str = "00000000-0000-0000-0000-000000000002";
    const DOMAIN_ID: &str = "1ac254d9-c9be-475a-93a7-f80b9c1063a8";

    fn cat() -> gst::DebugCategory {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let _ = gst::init();
        });
        gst::DebugCategory::new("test", gst::DebugColorFlags::empty(), Some("test"))
    }

    fn settings(side: Side) -> CommonSettings {
        CommonSettings {
            daemon_uri: "unix:/dev/null".to_owned(),
            node_seed: NODE_SEED.to_owned(),
            http_port: 0,
            transport: Transport::Mxl,
            side,
            name: "test-name".to_owned(),
            mxl_domain_id: DOMAIN_ID.to_owned(),
            mxl_domain_path: "/var/lib/mxl/domain-a".to_owned(),
            mxl_flow_id: String::new(),
            transport_file: String::new(),
            transport_file_path: String::new(),
            label: String::new(),
            description: String::new(),
            caps: None,
            transport_caps: None,
            caps_mode: CapsMode::Auto,
            // IS-05 RTP transport_params: unset/0 for MXL
            // tests (transport=Mxl above ignores them anyway);
            // UDP-side coverage of these fields lives in
            // dedicated UDP tests.
            source_ip: String::new(),
            source_port: 0,
            destination_ip: String::new(),
            destination_port: 0,
            interface_ip: String::new(),
            multicast_ip: String::new(),
            // Defaults to false (matching CommonSettings's
            // documented default and the canonical NMOS flow).
            // Tests that exercise the eager-activation path
            // override this explicitly.
            auto_activate: false,
        }
    }

    fn video_caps() -> gst::Caps {
        use std::str::FromStr;
        cat(); // ensures gst::init() ran
        gst::Caps::from_str(
            "video/x-raw,format=v210,width=1920,height=1080,framerate=50/1",
        )
        .expect("static caps parse")
    }

    fn video_flow_def(id: &str) -> String {
        format!(r#"{{"id":"{id}","format":"urn:x-nmos:format:video"}}"#)
    }

    fn req(side: Side, transport_file: Option<&str>) -> ActivationRequest {
        ActivationRequest {
            activation_handle: "test-activation".to_owned(),
            resource_handle: "test-resource".to_owned(),
            side,
            transport_file: transport_file.map(str::to_owned),
        }
    }

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

    /// Wire-enum mapping. Locks down the proto value each
    /// [`Transport`] variant translates to so a refactor (e.g.
    /// reordering the GObject enum, adding a new variant) doesn't
    /// silently shift discriminants. Reordering the enum without
    /// updating the proto mapping is otherwise an easy mistake to
    /// make.
    mod proto_mapping {
        use super::*;

        #[test]
        fn mxl_maps_to_mxl_proto() {
            assert_eq!(transport_to_proto(Transport::Mxl), ProtoTransport::Mxl);
        }

        #[test]
        fn udp_maps_to_rtp_proto() {
            assert_eq!(transport_to_proto(Transport::Udp), ProtoTransport::Rtp);
        }

        #[test]
        fn udp2_maps_to_rtp_proto() {
            assert_eq!(transport_to_proto(Transport::Udp2), ProtoTransport::Rtp);
        }

        #[test]
        fn nvdsudp_maps_to_rtp_proto() {
            assert_eq!(transport_to_proto(Transport::NvDsUdp), ProtoTransport::Rtp);
        }
    }

    /// [`TransportConfig`] type-surface coverage. The
    /// [`TransportConfig::Mxl`] variant is exercised end-to-end by
    /// the `make_activation_plan` / `apply_auto_activate_gate`
    /// tests above; [`TransportConfig::Udp`] isn't (the SDP module
    /// that would produce one hasn't landed) so the tests here
    /// construct both variants directly to lock in the field
    /// surface and the [`TransportConfig::transport_file`]
    /// accessor's behaviour for each.
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
        fn transport_file_mxl_present() {
            let tc = TransportConfig::Mxl {
                domain_path: "/var/lib/mxl/x".to_owned(),
                flow_id: FLOW_ID_A.to_owned(),
                format: FlowFormat::Video,
                transport_file: Some("payload".to_owned()),
            };
            assert_eq!(tc.transport_file(), Some("payload"));
        }

        #[test]
        fn transport_file_mxl_absent() {
            let tc = TransportConfig::Mxl {
                domain_path: "/var/lib/mxl/x".to_owned(),
                flow_id: FLOW_ID_A.to_owned(),
                format: FlowFormat::Video,
                transport_file: None,
            };
            assert_eq!(tc.transport_file(), None);
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

    /// UDP transport dispatch surfaces in [`decide_inner_config_udp`]
    /// and [`make_activation_plan`] (for `Transport::Udp` /
    /// `Transport::Udp2` / `Transport::NvDsUdp`). The resolved
    /// `TransportConfig::Udp` carries a real `UdpMedia` produced by
    /// [`crate::sdp::parse_sdp`], which the chain factories
    /// (`crate::inner::build_udpsink` / `build_udpsrc`) then
    /// turn into `udpsrc` / `udpsink` GStreamer chains;
    /// `Transport::NvDsUdp` is rejected before reaching either.
    mod udp_dispatch {
        use super::*;
        use std::str::FromStr;

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
                decide_inner_config_udp("nmossrc", &s, UdpVariant::V1, Some(VIDEO_UDP_SDP))
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
                decide_inner_config_udp("nmossink", &s, UdpVariant::V2, Some(VIDEO_UDP_SDP))
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
                    decide_inner_config_udp("nmossrc", &s, UdpVariant::V1, None)
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
                decide_inner_config_udp("nmossrc", &s, UdpVariant::V1, Some("garbage"))
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
            decide_inner_config_udp("nmossrc", &s, UdpVariant::V1, Some(VIDEO_UDP_SDP))
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
            )
            .expect_err("video clock-rate mismatch must error");
            let chain = format!("{err:#}");
            assert!(
                chain.contains("transport-caps mismatch"),
                "error must attribute to cross-check; got: {chain}",
            );
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

    /// Property-overrides-file at NULL→READY: when the user sets
    /// `mxl-flow-id` on the element and also supplies a
    /// transport file with a different id, the property wins and the
    /// file is rewritten to match (rather than rejecting the
    /// mismatch as a hard error, which is what we did before the
    /// splice layer existed).
    #[test]
    fn setup_property_overrides_file_flow_id() {
        let s = CommonSettings {
            mxl_flow_id: FLOW_ID_B.to_owned(),
            ..settings(Side::Sender)
        };
        let overrides = property_overrides_mxl(&s, DOMAIN_ID);
        let spliced =
            flow_def::splice_overrides(&video_flow_def(FLOW_ID_A), &overrides).unwrap();
        let v: serde_json::Value = serde_json::from_str(&spliced).unwrap();
        assert_eq!(v["id"], FLOW_ID_B);
        // Subsequent resolve_mxl_flow_meta with property==B and
        // file==B agrees silently; the previous "hard error on
        // mismatch" branch is no longer reachable from the setup
        // path.
        let resolved =
            flow_def::resolve_mxl_flow_meta(FLOW_ID_B, FlowFormat::Video, Some(&spliced)).unwrap();
        assert_eq!(resolved.id, FLOW_ID_B);
        assert_eq!(resolved.id_origin, flow_def::ValueOrigin::Both);
    }

    #[test]
    fn deactivation_is_fake_success() {
        let plan = make_activation_plan(&cat(), "nmossink", &settings(Side::Sender), &req(Side::Sender, None));
        assert!(matches!(plan.inner, InnerConfig::Fake { .. }));
        assert!(matches!(plan.ack, ActivationAck::Success));
    }

    #[test]
    fn side_mismatch_is_failure() {
        let plan = make_activation_plan(
            &cat(),
            "nmossink",
            &settings(Side::Sender),
            &req(Side::Receiver, Some(&video_flow_def(FLOW_ID_A))),
        );
        assert!(matches!(plan.inner, InnerConfig::Fake { .. }));
        match plan.ack {
            ActivationAck::Failure { reason } => assert!(
                reason.contains("side mismatch") || reason.contains("does not match"),
                "expected side-mismatch reason: {reason}"
            ),
            ActivationAck::Success => panic!("expected failure ack on side mismatch"),
        }
    }

    #[test]
    fn nmossrc_caps_st2038_drives_data_format() {
        use std::str::FromStr;
        let caps = gst::Caps::from_str("meta/x-st-2038,framerate=30/1")
            .expect("static caps parse");
        let s = CommonSettings {
            mxl_flow_id: FLOW_ID_A.to_owned(),
            caps: Some(caps),
            ..settings(Side::Receiver)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossrc",
            &s,
            &req(
                Side::Receiver,
                Some(r#"{"id":"00000000-0000-0000-0000-000000000001","format":"urn:x-nmos:format:data"}"#),
            ),
        );
        match plan.inner {
            InnerConfig::Real(TransportConfig::Mxl { format, .. }) => {
                assert_eq!(format, FlowFormat::Data)
            }
            InnerConfig::Real(TransportConfig::Udp { .. }) => {
                panic!("expected Real(Mxl(data)), got Real(Udp)")
            }
            InnerConfig::Fake { reason } => {
                panic!("expected Real(Mxl(data)), got Fake({reason})")
            }
        }
        assert!(matches!(plan.ack, ActivationAck::Success));
    }

    #[test]
    fn nmossrc_caps_unset_falls_back_to_fake() {
        // Receiver with neither `caps` nor a transport file `format`
        // can't pick a `mxlsrc` slot, so it stays on the fake chain.
        let s = CommonSettings {
            mxl_flow_id: FLOW_ID_A.to_owned(),
            ..settings(Side::Receiver)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossrc",
            &s,
            &req(
                Side::Receiver,
                Some(r#"{"id":"00000000-0000-0000-0000-000000000001"}"#),
            ),
        );
        match plan.inner {
            InnerConfig::Fake { reason } => assert!(
                reason.contains("caps") && reason.contains("flow format"),
                "expected caps-driven reason: {reason}"
            ),
            InnerConfig::Real(_) => panic!("expected Fake, got Real"),
        }
    }

    #[test]
    fn happy_path_video_is_real_success() {
        let s = CommonSettings {
            mxl_flow_id: FLOW_ID_A.to_owned(),
            caps: Some(video_caps()),
            ..settings(Side::Receiver)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossrc",
            &s,
            &req(Side::Receiver, Some(&video_flow_def(FLOW_ID_A))),
        );
        match plan.inner {
            InnerConfig::Real(TransportConfig::Mxl {
                domain_path, flow_id, format, transport_file,
            }) => {
                assert_eq!(domain_path, "/var/lib/mxl/domain-a");
                assert_eq!(flow_id, FLOW_ID_A);
                assert_eq!(format, FlowFormat::Video);
                assert!(
                    transport_file.is_some(),
                    "make_activation_plan must thread req.transport_file into InnerConfig",
                );
            }
            InnerConfig::Real(TransportConfig::Udp { .. }) => {
                panic!("expected Real(Mxl), got Real(Udp)")
            }
            InnerConfig::Fake { reason } => panic!("expected Real(Mxl), got Fake({reason})"),
        }
        assert!(matches!(plan.ack, ActivationAck::Success));
    }

    /// IS-05 PATCHes legitimately replace the flow id the element
    /// was configured with at NULL→READY. The activation's
    /// transport file is authoritative, so the activation must
    /// silently succeed and the inner be reconfigured against the
    /// new flow id.
    #[test]
    fn activation_flow_id_overrides_element_property() {
        let s = CommonSettings {
            mxl_flow_id: FLOW_ID_B.to_owned(),
            ..settings(Side::Sender)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossink",
            &s,
            &req(Side::Sender, Some(&video_flow_def(FLOW_ID_A))),
        );
        match (&plan.inner, &plan.ack) {
            (InnerConfig::Real(TransportConfig::Mxl { flow_id, .. }), ActivationAck::Success) => {
                assert_eq!(flow_id, FLOW_ID_A, "activation file's id must win");
            }
            other => panic!("expected ack-success + inner using FLOW_ID_A, got: {other:?}"),
        }
    }

    #[test]
    fn domain_path_unset_is_failure_with_live_transport_file() {
        // Activation supplies the spliced transport file, but this
        // host has no `mxl-domain-path` so the element can't bring
        // up mxlsink/mxlsrc. Per design: fake chain + failure ack.
        let s = CommonSettings {
            mxl_domain_path: String::new(),
            mxl_flow_id: FLOW_ID_A.to_owned(),
            ..settings(Side::Sender)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossink",
            &s,
            &req(Side::Sender, Some(&video_flow_def(FLOW_ID_A))),
        );
        match plan.inner {
            InnerConfig::Fake { reason } => assert!(
                reason.contains("mxl-domain-path"),
                "expected mxl-domain-path reason, got: {reason}"
            ),
            InnerConfig::Real(_) => panic!("expected Fake when mxl-domain-path unset"),
        }
        match plan.ack {
            ActivationAck::Failure { reason } => assert!(
                reason.contains("cannot bring up inner data path")
                    && reason.contains("mxl-domain-path"),
                "expected user-facing failure reason: {reason}",
            ),
            ActivationAck::Success => panic!(
                "expected failure ack when activation can't be honoured locally; got Success",
            ),
        }
    }

    #[test]
    fn domain_id_unresolvable_is_failure() {
        let s = CommonSettings {
            mxl_domain_id: String::new(),
            mxl_domain_path: String::new(),
            mxl_flow_id: FLOW_ID_A.to_owned(),
            ..settings(Side::Sender)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossink",
            &s,
            &req(Side::Sender, Some(&video_flow_def(FLOW_ID_A))),
        );
        match plan.ack {
            ActivationAck::Failure { reason } => assert!(
                reason.contains("mxl-domain-id"),
                "expected mxl-domain-id failure reason: {reason}",
            ),
            ActivationAck::Success => {
                panic!("expected failure ack when mxl-domain-id is unresolvable")
            }
        }
    }

    #[test]
    fn bad_transport_file_json_is_failure() {
        let s = CommonSettings {
            mxl_flow_id: FLOW_ID_A.to_owned(),
            ..settings(Side::Sender)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossink",
            &s,
            &req(Side::Sender, Some("not json")),
        );
        assert!(matches!(plan.inner, InnerConfig::Fake { .. }));
        assert!(matches!(plan.ack, ActivationAck::Failure { .. }));
    }

    mod register_deferred {
        use super::*;
        use std::str::FromStr;

        fn no_session() -> Mutex<Option<Session>> {
            Mutex::new(None)
        }

        fn good_caps() -> gst::Caps {
            cat(); // ensures gst::init() ran
            gst::Caps::from_str(
                "video/x-raw,format=v210,width=1920,height=1080,framerate=50/1,\
                 interlace-mode=progressive,pixel-aspect-ratio=1/1",
            )
            .expect("static caps parse")
        }

        fn sender_settings() -> CommonSettings {
            CommonSettings {
                mxl_flow_id: FLOW_ID_A.to_owned(),
                ..settings(Side::Sender)
            }
        }

        #[test]
        fn empty_caps_is_error() {
            let res = register_deferred(
                &cat(),
                "nmossink",
                &sender_settings(),
                &no_session(),
                gst::Caps::new_empty(),
            );
            let err = res.expect_err("empty caps must be rejected");
            assert!(
                format!("{err:#}").contains("offered no caps"),
                "expected EMPTY-caps reason: {err:#}"
            );
        }

        #[test]
        fn any_caps_is_error() {
            let res = register_deferred(
                &cat(),
                "nmossink",
                &sender_settings(),
                &no_session(),
                gst::Caps::new_any(),
            );
            let err = res.expect_err("ANY caps must be rejected");
            assert!(
                format!("{err:#}").contains("ANY caps"),
                "expected ANY-caps reason: {err:#}"
            );
        }

        #[test]
        fn wrong_side_is_error() {
            // Receiver deferred mode is explicitly out of scope.
            let s = CommonSettings {
                side: Side::Receiver,
                ..sender_settings()
            };
            let res = register_deferred(&cat(), "nmossrc", &s, &no_session(), good_caps());
            let err = res.expect_err("receiver deferred mode is out of scope");
            assert!(
                format!("{err:#}").contains("sender-only"),
                "expected sender-only reason: {err:#}"
            );
        }

        #[test]
        fn unsupported_transport_is_error() {
            // Deferred mode rejects non-`mxl` transports up-front,
            // mirroring `validate_and_open`'s same check. This test
            // covers the synchronous branch; the async sibling is
            // covered by integration tests.
            for t in [Transport::Udp, Transport::Udp2, Transport::NvDsUdp] {
                let s = CommonSettings {
                    transport: t,
                    ..sender_settings()
                };
                let res = register_deferred(
                    &cat(),
                    "nmossink",
                    &s,
                    &no_session(),
                    good_caps(),
                );
                let err = res.expect_err(
                    "non-mxl transport must be rejected by deferred path",
                );
                let msg = format!("{err:#}");
                assert!(
                    msg.contains("deferred registration unsupported"),
                    "expected deferred-transport rejection reason for {t:?}: {msg}",
                );
            }
        }

        #[test]
        fn missing_domain_id_is_error() {
            let s = CommonSettings {
                mxl_domain_id: String::new(),
                mxl_domain_path: String::new(),
                ..sender_settings()
            };
            let res = register_deferred(&cat(), "nmossink", &s, &no_session(), good_caps());
            let err = res.expect_err("missing mxl-domain-id must be rejected");
            assert!(
                format!("{err:#}").contains("mxl-domain-id"),
                "expected mxl-domain-id reason: {err:#}"
            );
        }

        #[test]
        fn missing_flow_id_is_error_via_builder() {
            let s = CommonSettings {
                mxl_flow_id: String::new(),
                ..sender_settings()
            };
            let res = register_deferred(&cat(), "nmossink", &s, &no_session(), good_caps());
            let err = res.expect_err("missing mxl-flow-id must be rejected");
            assert!(
                format!("{err:#}").contains("flow_id") || format!("{err:#}").contains("flow-id"),
                "expected mxl-flow-id reason: {err:#}"
            );
        }

        #[test]
        fn unsupported_caps_shape_is_error_via_builder() {
            // I420 isn't in the MXL pad template; the builder must
            // reject it, and the user is expected to add a capsfilter.
            let caps = gst::Caps::from_str("video/x-raw,format=I420,width=1920,height=1080")
                .expect("static caps parse");
            let res = register_deferred(&cat(), "nmossink", &sender_settings(), &no_session(), caps);
            let err = res.expect_err("unsupported caps must be rejected");
            // exact message is owned by from_caps; we just want
            // the synthesis-context wrapper to be present.
            assert!(
                format!("{err:#}").contains("synthesising flow_def"),
                "expected synthesis context in error: {err:#}"
            );
        }

        #[test]
        fn no_open_session_is_error() {
            // Caps are valid and validation passes; we should reach
            // the session-take step and surface a clear error.
            let res = register_deferred(
                &cat(),
                "nmossink",
                &sender_settings(),
                &no_session(),
                good_caps(),
            );
            let err = res.expect_err("missing session must be reported");
            assert!(
                format!("{err:#}").contains("no open session"),
                "expected no-open-session reason: {err:#}"
            );
        }
    }

    mod synthesise_or_passthrough_mxl {
        use super::*;

        fn parse(json: &str) -> serde_json::Value {
            serde_json::from_str(json).expect("synthesised JSON must parse")
        }

        /// Caps + `mxl-flow-id` on a Receiver synthesises a configuring
        /// flow_def the daemon can use to advertise narrow Receiver
        /// Caps on IS-04. The synthesised shape matches what the
        /// equivalent Sender call would produce — `from_caps`
        /// is symmetric.
        #[test]
        fn receiver_caps_and_flow_id_synthesise_flow_def() {
            let s = CommonSettings {
                mxl_flow_id: FLOW_ID_A.to_owned(),
                name: "video-receiver".to_owned(),
                label: "Studio A camera".to_owned(),
                description: "v210 1080p50".to_owned(),
                caps: Some(video_caps()),
                ..settings(Side::Receiver)
            };
            let out = super::synthesise_or_passthrough_mxl(&cat(), "nmossrc", &s, DOMAIN_ID, None)
                .expect("synthesis must succeed");
            let text = out.expect("Receiver synthesis must yield Some(json) when caps + flow id are set");
            let v = parse(&text);
            assert_eq!(v["id"], FLOW_ID_A);
            assert_eq!(v["format"], "urn:x-nmos:format:video");
            assert_eq!(v["media_type"], "video/v210");
            assert_eq!(v["label"], "Studio A camera");
            assert_eq!(v["description"], "v210 1080p50");
            assert_eq!(v["tags"]["urn:x-nvnmos:tag:name"][0], "video-receiver");
            assert_eq!(v["tags"]["urn:x-nvnmos:tag:mxl-domain-id"][0], DOMAIN_ID);
        }

        /// Receiver synthesis is gated on `mxl-flow-id`: without it
        /// we have nothing to subscribe to and no stable id for the
        /// configuring flow_def. Returning `None` puts the element
        /// on the fake chain until an IS-05 activation supplies the
        /// missing piece.
        #[test]
        fn receiver_caps_without_flow_id_returns_none() {
            let s = CommonSettings {
                caps: Some(video_caps()),
                ..settings(Side::Receiver)
            };
            assert!(s.mxl_flow_id.is_empty(), "test precondition");
            let out = super::synthesise_or_passthrough_mxl(&cat(), "nmossrc", &s, DOMAIN_ID, None)
                .expect("absent flow id must not error");
            assert!(out.is_none(), "Receiver without flow id must not synthesise");
        }

        /// Sender synthesis still works the same way. Sanity check
        /// against future refactors of the shared arm.
        #[test]
        fn sender_caps_and_flow_id_synthesise_flow_def() {
            let s = CommonSettings {
                mxl_flow_id: FLOW_ID_A.to_owned(),
                name: "video-sender".to_owned(),
                caps: Some(video_caps()),
                ..settings(Side::Sender)
            };
            let out = super::synthesise_or_passthrough_mxl(&cat(), "nmossink", &s, DOMAIN_ID, None)
                .expect("Sender synthesis must succeed");
            let v = parse(&out.expect("Sender synthesis yields Some(json)"));
            assert_eq!(v["id"], FLOW_ID_A);
            assert_eq!(v["tags"]["urn:x-nvnmos:tag:name"][0], "video-sender");
        }

        /// When the user supplies a literal transport file, it is
        /// passed through verbatim regardless of side or whether
        /// `caps` is also set (caps cross-check happens further down).
        #[test]
        fn passthrough_wins_over_caps_synthesis() {
            let s = CommonSettings {
                mxl_flow_id: FLOW_ID_B.to_owned(),
                caps: Some(video_caps()),
                ..settings(Side::Receiver)
            };
            let resolved = Some(video_flow_def(FLOW_ID_A));
            let out = super::synthesise_or_passthrough_mxl(&cat(), "nmossrc", &s, DOMAIN_ID, resolved.clone())
                .expect("passthrough must succeed");
            assert_eq!(
                out.as_deref(),
                resolved.as_deref(),
                "transport file must pass through unchanged when supplied"
            );
        }
    }

    /// Setup-time `auto-activate` gate covers the
    /// "data path live without IS-05 PATCH" toggle. The gate is
    /// orthogonal to how the configuring flow_def is supplied — a
    /// flow id from `mxl-flow-id`, from a transport file's
    /// top-level `id`, or from caps→flow_def synthesis all reach
    /// the same `apply_auto_activate_gate` call.
    mod auto_activate {
        use super::*;

        fn real_inner() -> InnerConfig {
            InnerConfig::Real(TransportConfig::Mxl {
                domain_path: "/var/lib/mxl/domain-a".to_owned(),
                flow_id: FLOW_ID_A.to_owned(),
                format: FlowFormat::Video,
                transport_file: Some(video_flow_def(FLOW_ID_A)),
            })
        }

        fn fake_inner() -> InnerConfig {
            InnerConfig::Fake {
                reason: "test fixture fake chain".to_owned(),
            }
        }

        #[test]
        fn gate_passes_real_through_when_auto_activate_true() {
            let after = super::super::apply_auto_activate_gate(real_inner(), true);
            match after {
                InnerConfig::Real(TransportConfig::Mxl { flow_id, format, .. }) => {
                    assert_eq!(flow_id, FLOW_ID_A);
                    assert_eq!(format, FlowFormat::Video);
                }
                InnerConfig::Real(TransportConfig::Udp { .. }) => {
                    panic!("auto-activate=true must not change Mxl into Udp")
                }
                InnerConfig::Fake { reason } => {
                    panic!("auto-activate=true must not downgrade Real: {reason}")
                }
            }
        }

        #[test]
        fn gate_downgrades_real_to_fake_when_auto_activate_false() {
            let after = super::super::apply_auto_activate_gate(real_inner(), false);
            match after {
                InnerConfig::Real(_) => {
                    panic!("auto-activate=false must downgrade Real to Fake")
                }
                InnerConfig::Fake { reason } => {
                    assert!(
                        reason.contains("auto-activate=false"),
                        "expected gate-attributed reason, got: {reason}"
                    );
                    assert!(
                        reason.contains("IS-05"),
                        "expected reason to point at IS-05 path, got: {reason}"
                    );
                }
            }
        }

        /// The gate never *upgrades* a fake chain. If
        /// `decide_inner_config` already produced a fake
        /// (e.g. receiver with no caps), the gate must leave it
        /// alone regardless of `auto-activate`.
        #[test]
        fn gate_leaves_fake_alone_under_both_settings() {
            let after_true = super::super::apply_auto_activate_gate(fake_inner(), true);
            assert!(matches!(after_true, InnerConfig::Fake { .. }));
            let after_false = super::super::apply_auto_activate_gate(fake_inner(), false);
            assert!(matches!(after_false, InnerConfig::Fake { .. }));
        }

        /// IS-05 activations (`make_activation_plan`) are
        /// controller-driven and must not be gated by
        /// `auto-activate`: that property is the *setup-time*
        /// switch for "start without a controller", not a runtime
        /// admission policy. An IS-05 PATCH activating a Sender
        /// against a Sender-side session must still apply
        /// `master_enable: true` no matter what `auto-activate`
        /// was set to at NULL→READY.
        #[test]
        fn is05_activation_path_ignores_auto_activate() {
            let s = CommonSettings {
                mxl_flow_id: FLOW_ID_A.to_owned(),
                caps: Some(video_caps()),
                // The element was started with the controller-driven
                // path (auto-activate=false), so the inner sat on the
                // fake chain at NULL→READY. An IS-05 PATCH then
                // arrives — `make_activation_plan` must produce a real
                // plan regardless of this flag.
                auto_activate: false,
                ..settings(Side::Sender)
            };
            let plan = make_activation_plan(
                &cat(),
                "nmossink",
                &s,
                &req(Side::Sender, Some(&video_flow_def(FLOW_ID_A))),
            );
            match plan.inner {
                InnerConfig::Real(TransportConfig::Mxl { flow_id, .. }) => {
                    assert_eq!(flow_id, FLOW_ID_A)
                }
                InnerConfig::Real(TransportConfig::Udp { .. }) => {
                    panic!("expected Real(Mxl), got Real(Udp)")
                }
                InnerConfig::Fake { reason } => {
                    panic!("IS-05 activation must reach Real regardless of auto-activate: {reason}")
                }
            }
            assert!(matches!(plan.ack, ActivationAck::Success));
        }

        /// The point of the property is that the route by which the
        /// flow id became available doesn't change the gate's
        /// decision. Run the gate over a `decide_inner_config`
        /// result that was produced via the caps→flow_def
        /// synthesis route (no transport file, just `caps` +
        /// `mxl-flow-id` property) and confirm both branches.
        #[test]
        fn gate_works_for_caps_synthesised_flow_id() {
            let s = CommonSettings {
                mxl_flow_id: FLOW_ID_A.to_owned(),
                caps: Some(video_caps()),
                ..settings(Side::Sender)
            };
            // Mimic the validate_and_open chain up to the gate:
            // synthesise the flow_def, resolve flow meta, decide
            // inner config — then apply the gate twice.
            let synth = super::super::synthesise_or_passthrough_mxl(
                &cat(),
                "nmossink",
                &s,
                DOMAIN_ID,
                None,
            )
            .expect("synthesis must succeed")
            .expect("caps + flow id must synthesise");
            let flow = flow_def::resolve_mxl_flow_meta(
                &s.mxl_flow_id,
                FlowFormat::Video,
                Some(&synth),
            )
            .expect("resolve_mxl_flow_meta");
            let inner = super::super::decide_inner_config_mxl(&s, &flow, Some(&synth));
            assert!(
                matches!(inner, InnerConfig::Real(_)),
                "fixture must produce Real before the gate"
            );

            // Same inner, but pass through the gate with both
            // toggle values:
            let eager = super::super::apply_auto_activate_gate(inner.clone(), true);
            assert!(
                matches!(eager, InnerConfig::Real(_)),
                "auto-activate=true: caps-synthesised flow_id must keep the Real path"
            );
            let lazy = super::super::apply_auto_activate_gate(inner, false);
            match lazy {
                InnerConfig::Fake { reason } => {
                    assert!(reason.contains("auto-activate=false"))
                }
                InnerConfig::Real(_) => {
                    panic!("auto-activate=false: caps-synthesised flow_id must defer to IS-05")
                }
            }
        }
    }
}
