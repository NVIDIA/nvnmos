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
use crate::types::{CapsMode, FlowFormat, Transport};

/// Open-session timeout. Aligned with the daemon's activation ack
/// timeout — same order of magnitude, no special meaning.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the snapshot came from `nmossink` or `nmossrc`. Surfaces in
/// error/log messages so validation failures point the user at the
/// right property name, and selects which gRPC AddSender/AddReceiver
/// call the session opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// `Transport::Mxl` is the only variant on this branch; new
/// transport variants and their proto mappings are added by the
/// follow-up branches that wire their inner chains in.
pub(crate) fn transport_to_proto(t: Transport) -> ProtoTransport {
    match t {
        Transport::Mxl => ProtoTransport::Mxl,
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
    "Inner data path family. Only `mxl` is currently supported; the \
     other values exist for ABI stability and are rejected.";

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
    /// Controls whether the resource advertises narrow or wide caps
    /// in IS-04. See [`CapsMode`] for the full semantics. Honoured
    /// only when `side` is `Receiver` (driven by the
    /// `receiver-caps-mode` property on `nmossrc`); `nmossink` leaves
    /// it at [`CapsMode::Auto`].
    pub(crate) caps_mode: CapsMode,
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
/// (NVDS-UDP, plain UDP/RTP, ...) get their own variants alongside
/// [`TransportConfig::Mxl`].
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

    let resolved_transport_file = resolve_transport_file(element, settings)?;
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

    let mut inner = decide_inner_config(settings, &flow, transport_file.as_deref());
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
    let inner_summary = match &inner {
        InnerConfig::Real(TransportConfig::Mxl { domain_path, flow_id, format, .. }) => {
            format!("inner data path: mxl (domain_path={domain_path:?}, flow_id={flow_id}, format={format:?})")
        }
        InnerConfig::Fake { reason } => {
            format!("inner data path: fake ({reason})")
        }
    };
    gst::info!(
        cat,
        "session opened: handle={} node_id={} created_node={} \
         (node_seed={}, side={:?}, name={}, \
         mxl-domain-id={}); {}; {}",
        new_session.session_handle,
        new_session.node_id,
        new_session.created_node,
        settings.node_seed,
        side,
        settings.name,
        domain_resolution.id,
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
fn decide_inner_config(
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

/// Honour `auto-activate` at setup time. When `auto_activate` is
/// `false` and `decide_inner_config` would have produced a real
/// transport chain, downgrade to [`InnerConfig::Fake`] so the data
/// path stays inactive until an IS-05 PATCH activates the resource
/// (the canonical NMOS path).
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
        decide_inner_config(settings, &flow, Some(&json)),
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
/// * Otherwise re-resolve `mxl-domain-id` (re-runs the
///   `domain_def.json` cross-check) and the flow id/format
///   (`flow_def::resolve_mxl_flow_meta` against the new
///   `transport_file`). Either resolver failing → fake chain +
///   failure ack.
///
/// * Run `decide_inner_config`: if it returns [`InnerConfig::Real`],
///   ack success; if it returns [`InnerConfig::Fake`] we have a
///   live transport file but can't bring up the real chain
///   (typically `mxl-domain-path` is unset on this host) — swap to
///   the fake chain but ack **failure** so the controller surfaces
///   the misconfiguration.
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

    let domain_resolution =
        match domain::resolve_mxl_domain_id(&settings.mxl_domain_id, &settings.mxl_domain_path) {
            Ok(r) => r,
            Err(e) => {
                return ActivationPlan {
                    inner: InnerConfig::Fake {
                        reason: "mxl-domain-id resolution failed".to_owned(),
                    },
                    ack: ActivationAck::Failure {
                        reason: format!(
                            "{element}: resolving MXL Domain identity for activation: {e:#}"
                        ),
                    },
                };
            }
        };
    if domain_resolution.id.is_empty() {
        return ActivationPlan {
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
        };
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
            return ActivationPlan {
                inner: InnerConfig::Fake {
                    reason: "flow_def resolution failed".to_owned(),
                },
                ack: ActivationAck::Failure {
                    reason: format!(
                        "{element}: resolving MXL flow id / format from activation \
                         transport file: {e:#}"
                    ),
                },
            };
        }
    };

    let inner = decide_inner_config(settings, &flow, Some(transport_file));
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
            caps_mode: CapsMode::Auto,
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
            let inner = super::super::decide_inner_config(&s, &flow, Some(&synth));
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
