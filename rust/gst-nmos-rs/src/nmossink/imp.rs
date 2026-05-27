// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nmossink` impl: GstBin subclass that opens a session against
//! `nvnmosd` at NULL→READY and closes it at READY→NULL. The inner
//! data path is a `mxlsink` when the resolved configuration pins a
//! Domain path + Flow id; otherwise the bin keeps a placeholder
//! `fakesink` so the element looks valid in the pipeline until an
//! IS-05 activation (or a later configuration update) supplies the
//! missing pieces.
//!
//! Activations arriving on the daemon subscription are dispatched to
//! [`NmosSink::apply_activation`], which marshals the work onto the
//! GStreamer thread via `Element::call_async`. At state ≤ READY the
//! swap happens inline; at state ≥ PAUSED we gate it on a single-shot
//! IDLE pad probe on the bin's external ghost pad so the streaming
//! thread isn't inside the inner element when we tear it down.

use std::sync::{Arc, LazyLock, Mutex};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use tokio::sync::oneshot;

use crate::daemon::{ActivationHandler, ActivationOutcome, ActivationRequest, Session};
use crate::inner;
use crate::session::{ActivationAck, ActivationPlan, InnerConfig};
use crate::types::{DEFAULT_DAEMON_URI, Transport};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "nmossink",
        gst::DebugColorFlags::empty(),
        Some("NMOS sender wrapper element"),
    )
});

#[derive(Debug, Clone)]
struct Settings {
    daemon_uri: String,
    node_seed: String,
    http_port: u16,
    transport: Transport,
    sender_name: String,
    mxl_domain_id: String,
    mxl_domain_path: String,
    mxl_flow_id: String,
    label: String,
    description: String,
    transport_file: String,
    transport_file_path: String,
    caps: Option<gst::Caps>,
    transport_caps: Option<gst::Caps>,
    auto_activate: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            daemon_uri: DEFAULT_DAEMON_URI.to_owned(),
            node_seed: String::new(),
            http_port: 0,
            transport: Transport::default(),
            sender_name: String::new(),
            mxl_domain_id: String::new(),
            mxl_domain_path: String::new(),
            mxl_flow_id: String::new(),
            label: String::new(),
            description: String::new(),
            transport_file: String::new(),
            transport_file_path: String::new(),
            caps: None,
            transport_caps: None,
            auto_activate: false,
        }
    }
}

#[derive(Default)]
pub struct NmosSink {
    settings: Mutex<Settings>,
    session: Mutex<Option<Session>>,
    /// Ghost pad that hides the current inner element behind the bin.
    /// Created at `constructed`, re-targeted at NULL↔READY transitions
    /// as the inner element swaps between the placeholder and a real
    /// `mxlsink`.
    ghost: Mutex<Option<gst::GhostPad>>,
}

#[glib::object_subclass]
impl ObjectSubclass for NmosSink {
    const NAME: &'static str = "GstNmosSink";
    type Type = super::NmosSink;
    type ParentType = gst::Bin;
}

impl ObjectImpl for NmosSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("daemon-uri")
                    .nick("Daemon URI")
                    .blurb(crate::session::DAEMON_URI_BLURB)
                    .default_value(Some(DEFAULT_DAEMON_URI))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("node-seed")
                    .nick("Node seed")
                    .blurb(crate::session::NODE_SEED_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("http-port")
                    .nick("HTTP port")
                    .blurb(crate::session::HTTP_PORT_BLURB)
                    .minimum(0)
                    .maximum(65535)
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("transport", Transport::Mxl)
                    .nick("Transport")
                    .blurb(crate::session::TRANSPORT_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("sender-name")
                    .nick("NMOS sender name")
                    .blurb(
                        "Name for this Sender within the Node (becomes the \
                         `x-nvnmos-name` SDP attribute or the \
                         `urn:x-nvnmos:tag:name` flow-def tag in the \
                         transport file). Unique across Senders on the \
                         Node; a Receiver on the same Node may share the \
                         same name (the daemon scopes names by side). \
                         Overrides the transport file's tag when both \
                         are supplied.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("mxl-domain-id")
                    .nick("MXL Domain id")
                    .blurb(crate::session::MXL_DOMAIN_ID_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("mxl-domain-path")
                    .nick("MXL Domain path")
                    .blurb(
                        "Local filesystem path identifying the MXL Domain on \
                         this host. If the directory contains a \
                         `domain_def.json` (AMWA BCP-007-03 WIP) its `id` is \
                         used to populate `mxl-domain-id` (or cross-checked \
                         against it when both are set). The path itself will \
                         be consumed by the inner `mxlsink` `domain=` \
                         property when the data path is wired up.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("mxl-flow-id")
                    .nick("MXL flow id")
                    .blurb(
                        "MXL flow id (UUID) the inner `mxlsink` should push into. \
                         Overrides the transport file's top-level `id` when both \
                         are supplied.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("auto-activate")
                    .nick("Auto-activate")
                    .blurb(crate::session::AUTO_ACTIVATE_BLURB)
                    .default_value(false)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("label")
                    .nick("Label")
                    .blurb(
                        "NMOS label for the Sender. Optional. Overrides \
                         the transport file's top-level `label` when both \
                         are supplied.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("description")
                    .nick("Description")
                    .blurb(
                        "NMOS description for the Sender. Optional. \
                         Overrides the transport file's top-level \
                         `description` when both are supplied.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("transport-file")
                    .nick("Transport file")
                    .blurb(
                        "Literal contents of the NvNmos transport file: MXL flow_def \
                         JSON today; SDP later. The daemon registers it with the \
                         resource and re-publishes it via IS-05. Pass the text, not a \
                         path. Convenient for programmatic callers; from gst-launch \
                         use `transport-file-path` instead. Mutually exclusive with \
                         `transport-file-path`. When unset and `caps` is supplied the \
                         element synthesises a flow_def from the essence caps.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("transport-file-path")
                    .nick("Transport file path")
                    .blurb(crate::session::TRANSPORT_FILE_PATH_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("caps")
                    .nick("Essence caps")
                    .blurb(
                        "Essence caps used to synthesise the MXL `flow_def` JSON when \
                         `transport-file` / `transport-file-path` are unset. Supported \
                         shapes match `mxlsink`'s pad template: `video/x-raw,format=v210,…`, \
                         `audio/x-raw,format=F32LE,…`, and `meta/x-st-2038,framerate=…`. \
                         Requires `mxl-flow-id` to be set. Cross-checked against the \
                         transport file's `format` field when both are supplied — \
                         mismatch is a hard error.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("transport-caps")
                    .nick("Transport caps")
                    .blurb(crate::session::TRANSPORT_CAPS_BLURB)
                    .mutable_ready()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "daemon-uri" => {
                settings.daemon_uri = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| DEFAULT_DAEMON_URI.to_owned());
            }
            "node-seed" => {
                settings.node_seed = string_or_empty(value);
            }
            "http-port" => {
                let v: u32 = value.get().expect("type checked upstream");
                // ParamSpec range-checks against [0, 65535] upstream.
                settings.http_port = u16::try_from(v).expect("range checked by ParamSpec");
            }
            "transport" => {
                settings.transport = value.get().expect("type checked upstream");
            }
            "sender-name" => {
                settings.sender_name = string_or_empty(value);
            }
            "mxl-domain-id" => {
                settings.mxl_domain_id = string_or_empty(value);
            }
            "mxl-domain-path" => {
                settings.mxl_domain_path = string_or_empty(value);
            }
            "mxl-flow-id" => {
                settings.mxl_flow_id = string_or_empty(value);
            }
            "auto-activate" => {
                settings.auto_activate = value.get().expect("type checked upstream");
            }
            "label" => {
                settings.label = string_or_empty(value);
            }
            "description" => {
                settings.description = string_or_empty(value);
            }
            "transport-file" => {
                settings.transport_file = string_or_empty(value);
            }
            "transport-file-path" => {
                settings.transport_file_path = string_or_empty(value);
            }
            "caps" => {
                settings.caps = value.get().expect("type checked upstream");
            }
            "transport-caps" => {
                settings.transport_caps = value.get().expect("type checked upstream");
            }
            _ => unimplemented!("unknown property {}", pspec.name()),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "daemon-uri" => settings.daemon_uri.to_value(),
            "node-seed" => settings.node_seed.to_value(),
            "http-port" => u32::from(settings.http_port).to_value(),
            "transport" => settings.transport.to_value(),
            "sender-name" => settings.sender_name.to_value(),
            "mxl-domain-id" => settings.mxl_domain_id.to_value(),
            "mxl-domain-path" => settings.mxl_domain_path.to_value(),
            "mxl-flow-id" => settings.mxl_flow_id.to_value(),
            "auto-activate" => settings.auto_activate.to_value(),
            "label" => settings.label.to_value(),
            "description" => settings.description.to_value(),
            "transport-file" => settings.transport_file.to_value(),
            "transport-file-path" => settings.transport_file_path.to_value(),
            "caps" => settings.caps.to_value(),
            "transport-caps" => settings.transport_caps.to_value(),
            _ => unimplemented!("unknown property {}", pspec.name()),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        let bin = self.obj();
        let bin_ref: &gst::Bin = bin.upcast_ref();
        match install_initial_placeholder(bin_ref) {
            Ok(ghost) => {
                *self.ghost.lock().unwrap() = Some(ghost);
            }
            Err(e) => gst::error!(CAT, "failed to build nmossink placeholder data path: {e}"),
        }
    }
}

impl GstObjectImpl for NmosSink {}

impl ElementImpl for NmosSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "NMOS sender",
                "Sink/Network/NMOS",
                "NMOS Sender wrapper element backed by nvnmosd",
                "NVIDIA Corporation",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &gst::Caps::new_any(),
                )
                .expect("building nmossink sink pad template"),
            ]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::debug!(CAT, imp = self, "state transition {transition:?}");
        match transition {
            gst::StateChange::NullToReady => {
                if let Err(e) = self.open_session() {
                    gst::element_imp_error!(
                        self,
                        gst::ResourceError::OpenWrite,
                        ["nmossink NULL\u{2192}READY failed: {e:#}"]
                    );
                    return Err(gst::StateChangeError);
                }
            }
            gst::StateChange::ReadyToNull => {
                self.close_session();
            }
            gst::StateChange::ReadyToPaused => {
                // Children transition up first so caps negotiate
                // (the placeholder fakesink accepts whatever upstream
                // proposes); we then query the negotiated peer caps
                // and, if no resource is yet registered, drive the
                // deferred AddSender.
                let res = self.parent_change_state(transition)?;
                if let Err(e) = self.maybe_register_deferred() {
                    gst::element_imp_error!(
                        self,
                        gst::ResourceError::OpenWrite,
                        ["nmossink READY\u{2192}PAUSED deferred registration failed: {e:#}"]
                    );
                    return Err(gst::StateChangeError);
                }
                return Ok(res);
            }
            _ => (),
        }
        self.parent_change_state(transition)
    }
}

impl BinImpl for NmosSink {}

impl NmosSink {
    fn open_session(&self) -> Result<(), anyhow::Error> {
        let snapshot = self.settings.lock().unwrap().clone();
        let bin = self.obj();
        let bin_ref: &gst::Bin = bin.upcast_ref();
        let handler = self.activation_handler();
        let outcome = crate::session::validate_and_open(
            &CAT,
            "nmossink",
            &snapshot.into(),
            &self.session,
            handler,
        )?;
        if let Err(e) = self.activate_inner(bin_ref, &outcome) {
            // Close the daemon session and restore the placeholder so the
            // bin is left as if NULL→READY had never been attempted.
            self.close_session();
            return Err(e);
        }
        Ok(())
    }

    fn activate_inner(
        &self,
        bin: &gst::Bin,
        outcome: &InnerConfig,
    ) -> Result<(), anyhow::Error> {
        if let InnerConfig::Mxl {
            domain_path,
            flow_id,
            transport_file,
            ..
        } = outcome
        {
            let mxlsink = inner::build_mxlsink(domain_path, flow_id)?;
            self.swap_inner(bin, &mxlsink)?;
            // Reaching the `Mxl` branch at NULL→READY / READY→PAUSED
            // implies `auto-activate=true` (the `validate_and_open`
            // and `register_deferred` gates downgrade to
            // `Placeholder` otherwise). Tell the daemon to bring
            // the resource's IS-04/IS-05 view up to match the live
            // data path so external state stays consistent without
            // an external IS-05 PATCH.
            if let Err(e) = crate::session::sync_active(
                &CAT,
                "nmossink",
                &self.session,
                transport_file.as_deref(),
            ) {
                // Inner is up and pushing already; don't tear it
                // down for a SyncResourceState glitch, but surface
                // the failure as a warning so it shows up in logs.
                gst::warning!(CAT, "nmossink auto-activate sync failed: {e:#}");
            }
        }
        Ok(())
    }

    fn close_session(&self) {
        crate::session::close(&CAT, "nmossink", &self.session);
        let bin = self.obj();
        let bin_ref: &gst::Bin = bin.upcast_ref();
        match inner::build_placeholder_sink() {
            Ok(placeholder) => {
                if let Err(e) = self.swap_inner(bin_ref, &placeholder) {
                    gst::warning!(CAT, "restoring nmossink placeholder: {e:#}");
                }
            }
            Err(e) => gst::warning!(CAT, "rebuilding nmossink placeholder: {e:#}"),
        }
    }

    fn swap_inner(&self, bin: &gst::Bin, new_inner: &gst::Element) -> Result<(), anyhow::Error> {
        let ghost_guard = self.ghost.lock().unwrap();
        let ghost = ghost_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("nmossink ghost pad missing"))?;
        inner::swap_inner(bin, ghost, new_inner, "sink")
    }

    /// Drive a deferred `AddSender` from inside
    /// `change_state(ReadyToPaused)`. Only attempts registration when
    /// the session is open without a resource and neither
    /// `transport-file*` nor `caps` were supplied at NULL→READY. The
    /// ghost sink pad is queried for the upstream peer's caps, which
    /// are then fed to the shared caps-driven flow_def builder; on
    /// success the inner element is swapped to a real `mxlsink`.
    ///
    /// Returns `Ok(())` both when deferred mode is not applicable and
    /// when registration succeeds. Errors are propagated only on real
    /// failures (ANY/EMPTY caps, builder rejection, AddSender RPC
    /// failure) so that change_state surfaces a clear,
    /// pipeline-visible error.
    fn maybe_register_deferred(&self) -> Result<(), anyhow::Error> {
        let snapshot = self.settings.lock().unwrap().clone();
        let static_inputs_set = !snapshot.transport_file.is_empty()
            || !snapshot.transport_file_path.is_empty()
            || snapshot.caps.is_some();
        let resource_registered = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.resource_id().map(|_| ()))
            .is_some();
        let session_open = self.session.lock().unwrap().is_some();
        if !session_open || resource_registered || static_inputs_set {
            return Ok(());
        }

        let ghost = self
            .ghost
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("nmossink ghost pad missing"))?;
        let peer_caps = ghost.peer_query_caps(None);
        gst::debug!(CAT, imp = self, "deferred mode peer_query_caps -> {peer_caps}");

        let common: crate::session::CommonSettings = snapshot.into();
        let outcome = crate::session::register_deferred(
            &CAT,
            "nmossink",
            &common,
            &self.session,
            peer_caps,
        )?;

        let bin = self.obj();
        let bin_ref: &gst::Bin = bin.upcast_ref();
        self.activate_inner(bin_ref, &outcome)?;
        Ok(())
    }

    /// Build the [`ActivationHandler`] passed to
    /// [`crate::session::validate_and_open`]. Captures a weak ref to
    /// the element so the session's activation task doesn't keep
    /// the bin alive on its own.
    fn activation_handler(&self) -> ActivationHandler {
        let weak: glib::WeakRef<super::NmosSink> = self.obj().downgrade();
        Arc::new(move |req: ActivationRequest, tx: oneshot::Sender<ActivationOutcome>| {
            let Some(bin) = weak.upgrade() else {
                let _ = tx.send(ActivationOutcome::Failed {
                    reason: "nmossink element was dropped before activation could be applied"
                        .to_owned(),
                });
                return;
            };
            // Hop onto the GStreamer thread to inspect state and
            // touch the bin's children. `call_async` is the
            // canonical bridge from a tokio worker.
            bin.call_async(move |bin| {
                bin.imp().apply_activation(req, tx);
            });
        })
    }

    /// Apply an activation. Runs on a GStreamer worker thread (via
    /// `call_async`). The state-≤-READY branch swaps inline; the
    /// state-≥-PAUSED branch gates the swap on a single-shot IDLE
    /// pad probe and reports back asynchronously when the probe
    /// fires.
    fn apply_activation(&self, req: ActivationRequest, tx: oneshot::Sender<ActivationOutcome>) {
        let snapshot = self.settings.lock().unwrap().clone();
        let plan = crate::session::plan_activation(&CAT, "nmossink", &snapshot.into(), &req);
        gst::info!(
            CAT,
            "nmossink applying activation (activation_handle={}, resource_handle={}, side={:?}): \
             plan inner={:?}, ack={:?}",
            req.activation_handle,
            req.resource_handle,
            req.side,
            plan.inner,
            plan.ack,
        );

        let current = self.obj().current_state();
        if current <= gst::State::Ready {
            let outcome = self.apply_plan_inline(&plan);
            let _ = tx.send(outcome);
        } else {
            self.schedule_apply_via_probe(plan, tx);
        }
    }

    /// Perform the inner swap and translate the result into an
    /// [`ActivationOutcome`]. Called either directly (state ≤ READY)
    /// or from the IDLE probe (state ≥ PAUSED). On swap failure the
    /// element is left on the placeholder and the outcome is
    /// `Failed`.
    fn apply_plan_inline(&self, plan: &ActivationPlan) -> ActivationOutcome {
        let bin = self.obj();
        let bin_ref: &gst::Bin = bin.upcast_ref();
        let new_inner = match &plan.inner {
            InnerConfig::Mxl { domain_path, flow_id, .. } => {
                match inner::build_mxlsink(domain_path, flow_id) {
                    Ok(e) => e,
                    Err(e) => {
                        return ActivationOutcome::Failed {
                            reason: format!("nmossink: building inner mxlsink: {e:#}"),
                        };
                    }
                }
            }
            InnerConfig::Placeholder { .. } => match inner::build_placeholder_sink() {
                Ok(e) => e,
                Err(e) => {
                    return ActivationOutcome::Failed {
                        reason: format!("nmossink: building placeholder: {e:#}"),
                    };
                }
            },
        };
        if let Err(e) = self.swap_inner(bin_ref, &new_inner) {
            // Try one more time with a fresh placeholder so the bin
            // is left in a known state even on the failure path.
            if let Ok(p) = inner::build_placeholder_sink() {
                let _ = self.swap_inner(bin_ref, &p);
            }
            return ActivationOutcome::Failed {
                reason: format!("nmossink: swapping inner element: {e:#}"),
            };
        }
        match &plan.ack {
            ActivationAck::Success => ActivationOutcome::Applied,
            ActivationAck::Failure { reason } => ActivationOutcome::Failed {
                reason: reason.clone(),
            },
        }
    }

    /// Install a single-shot IDLE pad probe on the bin's external
    /// ghost pad; when it fires (i.e. no buffer is in flight through
    /// the bin's data path), run [`apply_plan_inline`] and forward
    /// the outcome via `tx`. Logged-only if the ghost pad is gone
    /// (the bin is being torn down).
    fn schedule_apply_via_probe(
        &self,
        plan: ActivationPlan,
        tx: oneshot::Sender<ActivationOutcome>,
    ) {
        let ghost = self.ghost.lock().unwrap().clone();
        let Some(ghost) = ghost else {
            let _ = tx.send(ActivationOutcome::Failed {
                reason: "nmossink ghost pad missing when scheduling activation probe".to_owned(),
            });
            return;
        };
        // Probe callbacks are `Fn` so they may run multiple times;
        // the take()-once pattern guarantees we only do the swap +
        // ack on the first invocation. `Remove` afterwards prevents
        // it from being called again, but we belt-and-brace the
        // bookkeeping anyway.
        let weak: glib::WeakRef<super::NmosSink> = self.obj().downgrade();
        let plan_cell = Arc::new(Mutex::new(Some(plan)));
        let tx_cell = Arc::new(Mutex::new(Some(tx)));
        ghost.add_probe(gst::PadProbeType::IDLE, move |_pad, _info| {
            let plan = plan_cell.lock().unwrap().take();
            let tx = tx_cell.lock().unwrap().take();
            if let (Some(plan), Some(tx)) = (plan, tx) {
                let outcome = match weak.upgrade() {
                    Some(bin) => bin.imp().apply_plan_inline(&plan),
                    None => ActivationOutcome::Failed {
                        reason: "nmossink dropped before activation probe fired".to_owned(),
                    },
                };
                let _ = tx.send(outcome);
            }
            gst::PadProbeReturn::Remove
        });
    }
}

fn install_initial_placeholder(bin: &gst::Bin) -> Result<gst::GhostPad, glib::BoolError> {
    let placeholder = inner::build_placeholder_sink()
        .map_err(|e| glib::bool_error!("{e}"))?;
    let ghost = inner::build_initial(bin, placeholder, "sink", gst::PadDirection::Sink)?;
    bin.add_pad(&ghost)
        .map_err(|e| glib::bool_error!("adding ghost pad to nmossink: {e}"))?;
    Ok(ghost)
}

fn string_or_empty(value: &glib::Value) -> String {
    value
        .get::<Option<String>>()
        .expect("type checked upstream")
        .unwrap_or_default()
}

impl From<Settings> for crate::session::CommonSettings {
    fn from(s: Settings) -> Self {
        crate::session::CommonSettings {
            daemon_uri: s.daemon_uri,
            node_seed: s.node_seed,
            http_port: s.http_port,
            transport: s.transport,
            side: crate::session::Side::Sender,
            name: s.sender_name,
            mxl_domain_id: s.mxl_domain_id,
            mxl_domain_path: s.mxl_domain_path,
            mxl_flow_id: s.mxl_flow_id,
            transport_file: s.transport_file,
            transport_file_path: s.transport_file_path,
            label: s.label,
            description: s.description,
            caps: s.caps,
            caps_mode: crate::types::CapsMode::Auto,
            auto_activate: s.auto_activate,
        }
    }
}
