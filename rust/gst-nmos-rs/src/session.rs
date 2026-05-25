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

use crate::daemon::Session;
use crate::domain::{self, DomainIdOrigin};
use crate::flow_def::{self, ValueOrigin};
use crate::runtime::SHARED_RUNTIME;
use crate::types::{FlowFormat, Transport};

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

/// Snapshot of the properties needed to open a session, taken under
/// the per-element settings lock so the lock isn't held over the
/// blocking RPC.
#[derive(Debug, Clone)]
pub(crate) struct CommonSettings {
    pub(crate) daemon_uri: String,
    pub(crate) node_seed: String,
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
    /// against the transport_file's top-level `id` when both are
    /// supplied; either source alone is enough.
    pub(crate) mxl_flow_id: String,
    /// NMOS format family for the flow. Required on `nmossrc` (to
    /// pick the right `mxlsrc` flow-id property); on `nmossink` it
    /// is informational because `mxlsink` has only one flow-id slot.
    /// Cross-checked against the transport_file's `format` field
    /// when both are supplied.
    pub(crate) mxl_flow_format: FlowFormat,
    /// Literal transport file contents (MXL `flow_def` JSON today).
    /// Convenient for programmatic callers (e.g. Rust/C apps that
    /// compute the flow_def in memory) but awkward to pass from
    /// `gst-launch-1.0` because the JSON contains newlines and
    /// quotes — those callers use `transport_file_path` instead.
    pub(crate) transport_file: String,
    /// Filesystem path that's read into `transport_file` at
    /// NULL→READY. Mutually exclusive with `transport_file`.
    pub(crate) transport_file_path: String,
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
/// [`InnerConfig::Mxl`] carries everything the element needs to
/// instantiate a `mxlsink` or `mxlsrc` (Domain path + Flow id, plus
/// the flow format for the receiver). [`InnerConfig::Placeholder`]
/// means the resolved configuration didn't pin a Domain path and/or a
/// Flow id; the element keeps its placeholder `fakesink` / `fakesrc`
/// in place and a later step (caps→flow_def, IS-05 activation) will
/// supply the missing pieces.
#[derive(Debug, Clone)]
pub(crate) enum InnerConfig {
    Mxl {
        domain_path: String,
        flow_id: String,
        /// Unspecified on `nmossink` — `mxlsink` has only one
        /// flow-id slot — and one of Video/Audio/Data on `nmossrc`.
        format: FlowFormat,
    },
    Placeholder {
        /// One-line summary of which piece of state was missing.
        /// Logged at INFO so it's clear why the placeholder path is
        /// in use.
        reason: String,
    },
}

/// Validate the settings snapshot and open a session via the shared
/// tokio runtime. On success the session is stored under `session`
/// and the returned [`InnerConfig`] tells the element how to wire its
/// data path.
pub(crate) fn validate_and_open(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    session: &Mutex<Option<Session>>,
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

    let transport_file = resolve_transport_file(element, settings)?;

    let flow = flow_def::resolve_mxl_flow_meta(
        &settings.mxl_flow_id,
        settings.mxl_flow_format,
        transport_file.as_deref(),
    )
    .with_context(|| format!("{element}: resolving MXL flow id / format"))?;
    log_flow_origin(cat, "mxl-flow-id", flow.id_origin);
    log_flow_origin(cat, "mxl-flow-format", flow.format_origin);

    let inner = decide_inner_config(settings, &flow);

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
                    side,
                    &name,
                    transport,
                    transport_file.as_deref(),
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
        InnerConfig::Mxl { domain_path, flow_id, format } => {
            format!("inner data path: mxl (domain_path={domain_path:?}, flow_id={flow_id}, format={format:?})")
        }
        InnerConfig::Placeholder { reason } => {
            format!("inner data path: placeholder ({reason})")
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

fn log_flow_origin(cat: &gst::DebugCategory, field: &str, origin: ValueOrigin) {
    match origin {
        ValueOrigin::Property => gst::debug!(cat, "{field} from property; no transport_file constraint"),
        ValueOrigin::File => gst::info!(cat, "{field} taken from transport_file"),
        ValueOrigin::Both => gst::debug!(cat, "{field} cross-checked against transport_file"),
        ValueOrigin::None => gst::debug!(cat, "{field} not supplied by either source"),
    }
}

/// Decide whether the element can build a real `mxlsink` / `mxlsrc`
/// or has to fall back to its placeholder. Both sides need a
/// non-empty Domain path and a non-empty flow id; the receiver
/// additionally needs a specific [`FlowFormat`] (because `mxlsrc`
/// has separate `video-flow-id` / `audio-flow-id` / `data-flow-id`
/// properties).
fn decide_inner_config(settings: &CommonSettings, flow: &flow_def::FlowResolution) -> InnerConfig {
    if settings.mxl_domain_path.is_empty() {
        return InnerConfig::Placeholder {
            reason: "`mxl-domain-path` unset".to_owned(),
        };
    }
    if flow.id.is_empty() {
        return InnerConfig::Placeholder {
            reason: "`mxl-flow-id` unset (neither property nor transport_file supplied it)".to_owned(),
        };
    }
    if settings.side == Side::Receiver && flow.format == FlowFormat::Unspecified {
        return InnerConfig::Placeholder {
            reason:
                "`mxl-flow-format` unset on nmossrc (neither property nor transport_file supplied it)"
                    .to_owned(),
        };
    }
    InnerConfig::Mxl {
        domain_path: settings.mxl_domain_path.clone(),
        flow_id: flow.id.clone(),
        format: flow.format,
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
