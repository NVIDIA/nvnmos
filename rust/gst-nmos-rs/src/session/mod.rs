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
use crate::domain;
use crate::flow_def::{self, FlowDefBuildInput};
use crate::runtime::SHARED_RUNTIME;
use crate::types::{CapsMode, DEFAULT_DAEMON_URI, FlowFormat, Transport};

/// Open-session timeout. Aligned with the daemon's activation ack
/// timeout — same order of magnitude, no special meaning.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) mod types;

use self::types::Side;

/// Translate the GObject `Transport` enum to the wire enum.
///
/// `Mxl` carries data via the MXL Domain. `Udp` and `Udp2` reach
/// this helper once `validate_and_open` resolves an SDP into a
/// [`TransportConfig::Udp`] and the inner chain factories
/// instantiate `udpsrc` / `udpsink`, or `nvdsudpsrc` / `nvdsudpsink`
/// for [`Transport::NvDsUdp`]. The mapping is provided eagerly so
/// the helper stays exhaustive across all `Transport` variants.
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

pub(crate) const HOST_NAME_BLURB: &str =
    "NMOS Node host name (`node_config.host_name`). Empty (the \
     default) leaves libnvnmos to autodetect. Honoured only by the \
     OpenSession that actually creates the Node; ignored when \
     attaching to a pre-existing Node with the same `node-seed`.";

pub(crate) const DOMAIN_BLURB: &str =
    "DNS domain for NMOS network services (`network_services.domain`). \
     Use `local` to force mDNS. Empty (the default) leaves libnvnmos \
     on automatic discovery. Not to be confused with `mxl-domain-id` \
     / `mxl-domain-path`, which identify an MXL shared-memory Domain. \
     Honoured only by the OpenSession that creates the Node.";

pub(crate) const REGISTRATION_URL_BLURB: &str =
    "Fixed IS-04 Registration API URL. Format: \
     `http://host[:port]/x-nmos/registration/v<X.Y>[/]`. Parsed into \
     `network_services.registration_*`; invalid URLs are logged and \
     ignored. Empty (the default) leaves libnvnmos on DNS-SD discovery \
     based on `host-name`. Honoured only by the OpenSession that \
     creates the Node.";

pub(crate) const SYSTEM_URL_BLURB: &str =
    "Fixed IS-09 System API URL. Format: \
     `http://host[:port]/x-nmos/system/v<X.Y>[/]`. Parsed into \
     `network_services.system_*`; invalid URLs are logged and ignored. \
     Honoured only when `registration-url` is also set (libnvnmos \
     ignores a standalone System API). Honoured only by the OpenSession \
     that creates the Node.";

pub(crate) const TRANSPORT_BLURB: &str =
    "Inner data path family. \
     `mxl`: MXL shared-memory transport (`mxlsrc` / `mxlsink`). \
     `udp`: ST 2110 over RTP/UDP via gst-plugins-good (`udpsrc` / \
     `udpsink` + the `rtpvrawpay` / `rtpL24pay` / `rtpsmpte291pay` \
     family). \
     `udp2`: ST 2110 over RTP/UDP via gst-plugins-rs (`udpsrc2` + \
     the `*pay2` / `*depay2` family where available, falling back \
     to gst-plugins-good per-element). \
     `nvdsudp`: ST 2110 via DeepStream's `nvdsudpsrc` / `nvdsudpsink` \
     (Rivermax kernel-bypass, built-in RTP (de)payload, Mode 3). \
     Requires ConnectX-5+ and the Rivermax SDK.";

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
     `nvdsudpsrc`, `nvdsudpsink`, `mxlsrc`, or `mxlsink`) every time the \
     data-path chain is built. \
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
    /// See [`HOST_NAME_BLURB`].
    pub(crate) host_name: String,
    /// See [`DOMAIN_BLURB`].
    pub(crate) domain: String,
    /// See [`REGISTRATION_URL_BLURB`].
    pub(crate) registration_url: String,
    /// See [`SYSTEM_URL_BLURB`].
    pub(crate) system_url: String,
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

impl Default for CommonSettings {
    fn default() -> Self {
        Self {
            daemon_uri: DEFAULT_DAEMON_URI.to_owned(),
            node_seed: String::new(),
            http_port: 0,
            host_name: String::new(),
            domain: String::new(),
            registration_url: String::new(),
            system_url: String::new(),
            transport: Transport::default(),
            side: Side::Sender,
            name: String::new(),
            mxl_domain_id: String::new(),
            mxl_domain_path: String::new(),
            mxl_flow_id: String::new(),
            transport_file: String::new(),
            transport_file_path: String::new(),
            label: String::new(),
            description: String::new(),
            caps: None,
            transport_caps: None,
            caps_mode: CapsMode::default(),
            source_ip: String::new(),
            source_port: 0,
            destination_ip: String::new(),
            destination_port: 0,
            interface_ip: String::new(),
            multicast_ip: String::new(),
            auto_activate: false,
        }
    }
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

/// Why the element stays on the fake data path instead of a real
/// transport chain — coarse category only. Subcases (e.g. which
/// dormant IS-05 state) live in [`InnerConfig::Fake::detail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FakeKind {
    /// Not enough state to register or build a real chain yet (e.g.
    /// MXL sender awaiting peer caps for deferred `AddSender`).
    NotConfigured,
    /// Invalid or inconsistent configuration — cannot honour activation.
    Misconfigured,
    /// Registered and valid, but no live RTP/media path (initial
    /// `auto-activate=false`, deactivated, or all SDP legs inactive).
    NotActive,
}

impl std::fmt::Display for FakeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "not configured"),
            Self::Misconfigured => write!(f, "misconfigured"),
            Self::NotActive => write!(f, "not active"),
        }
    }
}

impl FakeKind {
    /// Whether an IS-05 activation that produced this fake state should
    /// ack **success** (valid inactive state) rather than failure.
    pub(crate) fn activation_succeeds(self) -> bool {
        matches!(self, Self::NotActive)
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
        /// Coarse category — drives activation ack policy via
        /// [`FakeKind::activation_succeeds`].
        kind: FakeKind,
        /// Optional human-readable subcase; included in logs and activation
        /// failure reasons when non-empty.
        detail: String,
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
    /// `udpsrc ! rtp*depay [! capssetter(raw_caps)]` for receivers.
    /// Wide receivers (activation SDP carries `a=x-nvnmos-caps:`)
    /// omit `capssetter`; narrow receivers pin configuring essence
    /// caps parsed from the transport file. The exact element factory
    /// names dispatch on [`UdpVariant`].
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
        media: udp::types::UdpMedia,
        /// SDP transport file the daemon advertises on IS-04 (either
        /// the user-supplied one or the synthesised one). Retained
        /// verbatim for logs / diagnostics; the inner chain reads the
        /// logical RTP stream in [`udp::types::UdpMedia`] (not raw SDP).
        transport_file: Option<String>,
    },
    /// DeepStream Rivermax transport. Inner chain is a bare
    /// `nvdsudpsink` / `nvdsudpsrc` in Mode 3 (built-in RTP
    /// packetization / depacketization; no external `rtp*pay` /
    /// `rtp*depay`).
    NvDsUdp {
        media: udp::types::UdpMedia,
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
            Self::Mxl { transport_file, .. }
            | Self::Udp { transport_file, .. }
            | Self::NvDsUdp { transport_file, .. } => transport_file.as_deref(),
        }
    }
}

mod mxl;
pub(crate) mod udp;

pub(crate) use udp::UdpVariant;
pub(crate) use mxl::decide_inner_config_mxl;
pub(crate) use udp::{
    decide_inner_config_nvdsudp, decide_inner_config_udp, udp_essence_cross_check_mode,
};

use mxl::{resolve_activation_inner_mxl, resolve_inner_config_mxl};
use udp::{resolve_inner_config_nvdsudp, resolve_inner_config_udp};

fn udp_inner_summary(
    family: &str,
    variant: Option<UdpVariant>,
    media: &udp::types::UdpMedia,
) -> String {
    fn leg_summary(leg: &udp::types::UdpLeg) -> String {
        format!(
            "{}:{} iface={:?} source_ip={:?} source_port={:?}",
            leg.destination_ip,
            leg.destination_port,
            leg.interface_ip,
            leg.source_ip,
            leg.source_port,
        )
    }

    let legs = match &media.secondary {
        Some(secondary) => format!(
            "primary=[{}], secondary=[{}]",
            leg_summary(&media.primary),
            leg_summary(secondary),
        ),
        None => format!("primary=[{}]", leg_summary(&media.primary)),
    };
    let body = format!("format={:?}, {legs}", media.format);

    match variant {
        Some(v) => format!("inner data path: {family} ({v:?}, {body})"),
        None => format!("inner data path: {family} ({body})"),
    }
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
        Transport::NvDsUdp => {
            resolve_inner_config_nvdsudp(cat, element, settings, resolved_transport_file)?
        }
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
                    settings,
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
            udp_inner_summary("udp", Some(*variant), media)
        }
        InnerConfig::Real(TransportConfig::NvDsUdp { media, .. }) => {
            udp_inner_summary("nvdsudp", None, media)
        }
        InnerConfig::Fake { kind, detail } => {
            if detail.is_empty() {
                format!("inner data path: fake ({kind})")
            } else {
                format!("inner data path: fake ({kind}: {detail})")
            }
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
pub(super) fn caps_format(settings: &CommonSettings) -> FlowFormat {
    settings
        .caps
        .as_ref()
        .map(FlowFormat::from_caps)
        .unwrap_or(FlowFormat::Unspecified)
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
            kind: FakeKind::NotActive,
            detail: "auto-activate=false; waiting for IS-05 PATCH to activate".into(),
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
///   - `NvDsUdp`: parse the SDP via [`decide_inner_config_nvdsudp`].
///     SDP parse errors → fake chain + failure ack with attribution
///     (same pattern as `Udp` / `Udp2`).
///
/// * If the chosen `decide_inner_config_*` returns
///   [`InnerConfig::Real`], ack success. If it returns
///   [`InnerConfig::Fake`] with [`FakeKind::NotActive`] (all legs
///   inactive), ack **success** — same spirit as deactivation: valid
///   IS-05 state, fake data path. [`FakeKind::Misconfigured`] acks
///   **failure** so the controller surfaces bad configuration.
pub(crate) fn make_activation_plan(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    req: &ActivationRequest,
) -> ActivationPlan {
    if req.side != settings.side {
        return ActivationPlan {
            inner: InnerConfig::Fake {
                kind: FakeKind::Misconfigured,
                detail: "activation side mismatch".into(),
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
                kind: FakeKind::NotActive,
                detail: "deactivation".into(),
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
            udp_essence_cross_check_mode(settings, true, Some(transport_file)),
        ) {
            Ok(inner) => inner,
            Err(e) => {
                return ActivationPlan {
                    inner: InnerConfig::Fake {
                        kind: FakeKind::Misconfigured,
                        detail: "SDP transport file rejected".into(),
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
            udp_essence_cross_check_mode(settings, true, Some(transport_file)),
        ) {
            Ok(inner) => inner,
            Err(e) => {
                return ActivationPlan {
                    inner: InnerConfig::Fake {
                        kind: FakeKind::Misconfigured,
                        detail: "SDP transport file rejected".into(),
                    },
                    ack: ActivationAck::Failure {
                        reason: format!("{element}: parsing activation SDP: {e:#}"),
                    },
                };
            }
        },
        Transport::NvDsUdp => match decide_inner_config_nvdsudp(
            element,
            settings,
            Some(transport_file),
            udp_essence_cross_check_mode(settings, true, Some(transport_file)),
        ) {
            Ok(inner) => inner,
            Err(e) => {
                return ActivationPlan {
                    inner: InnerConfig::Fake {
                        kind: FakeKind::Misconfigured,
                        detail: "SDP transport file rejected".into(),
                    },
                    ack: ActivationAck::Failure {
                        reason: format!("{element}: parsing activation SDP: {e:#}"),
                    },
                };
            }
        },
    };

    let ack = match &inner {
        InnerConfig::Real(_) => ActivationAck::Success,
        InnerConfig::Fake { kind, .. } if kind.activation_succeeds() => {
            gst::info!(
                cat,
                "{element}: activation is master_enable with all legs inactive; \
                 fake chain, ack success",
            );
            ActivationAck::Success
        }
        InnerConfig::Fake { kind, detail } => {
            let msg = if detail.is_empty() {
                kind.to_string()
            } else {
                detail.clone()
            };
            ActivationAck::Failure {
                reason: format!(
                    "{element}: activation cannot bring up inner data path: {msg}"
                ),
            }
        }
    };

    ActivationPlan { inner, ack }
}
/// Shared fixtures for `session` unit tests. Transport-specific
/// tests live in [`super::mxl`] and [`super::udp`]; this module
/// holds helpers both sides need (`settings`, `req`, flow ids, …).
#[cfg(test)]
mod support {
    use super::*;

    pub const NODE_SEED: &str = "test-seed";
    pub const FLOW_ID_A: &str = "00000000-0000-0000-0000-000000000001";
    pub const FLOW_ID_B: &str = "00000000-0000-0000-0000-000000000002";
    pub const DOMAIN_ID: &str = "1ac254d9-c9be-475a-93a7-f80b9c1063a8";

    pub fn cat() -> gst::DebugCategory {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let _ = gst::init();
        });
        gst::DebugCategory::new("test", gst::DebugColorFlags::empty(), Some("test"))
    }

    pub fn settings(side: Side) -> CommonSettings {
        CommonSettings {
            daemon_uri: "unix:/dev/null".to_owned(),
            node_seed: NODE_SEED.to_owned(),
            http_port: 0,
            host_name: String::new(),
            domain: String::new(),
            registration_url: String::new(),
            system_url: String::new(),
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

    pub fn video_caps() -> gst::Caps {
        use std::str::FromStr;
        cat(); // ensures gst::init() ran
        gst::Caps::from_str(
            "video/x-raw,format=v210,width=1920,height=1080,framerate=50/1",
        )
        .expect("static caps parse")
    }

    pub fn video_flow_def(id: &str) -> String {
        format!(r#"{{"id":"{id}","format":"urn:x-nmos:format:video"}}"#)
    }

    pub fn req(side: Side, transport_file: Option<&str>) -> ActivationRequest {
        ActivationRequest {
            activation_handle: "test-activation".to_owned(),
            resource_handle: "test-resource".to_owned(),
            side,
            transport_file: transport_file.map(str::to_owned),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::support::*;
    use super::*;

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
                kind: FakeKind::Misconfigured,
                detail: "test fixture fake chain".into(),
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
                InnerConfig::Real(TransportConfig::Udp { .. })
                | InnerConfig::Real(TransportConfig::NvDsUdp { .. }) => {
                    panic!("auto-activate=true must not change Mxl into RTP transport")
                }
                InnerConfig::Fake { kind, .. } => {
                    panic!("auto-activate=true must not downgrade Real: {kind}")
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
                InnerConfig::Fake { kind, .. } => {
                    assert_eq!(kind, FakeKind::NotActive);
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
                InnerConfig::Real(TransportConfig::Udp { .. })
                | InnerConfig::Real(TransportConfig::NvDsUdp { .. }) => {
                    panic!("expected Real(Mxl), got Real(RTP transport)")
                }
                InnerConfig::Fake { kind, .. } => {
                    panic!("IS-05 activation must reach Real regardless of auto-activate: {kind}")
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
            let synth = crate::session::mxl::synthesise_or_passthrough_mxl(
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
            let inner = crate::session::decide_inner_config_mxl(&s, &flow, Some(&synth));
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
                InnerConfig::Fake { kind, .. } => {
                    assert_eq!(kind, FakeKind::NotActive);
                }
                InnerConfig::Real(_) => {
                    panic!("auto-activate=false: caps-synthesised flow_id must defer to IS-05")
                }
            }
        }
    }
}
