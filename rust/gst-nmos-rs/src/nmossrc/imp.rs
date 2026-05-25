// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nmossrc` impl: GstBin subclass that opens a session against
//! `nvnmosd` at NULL→READY and closes it at READY→NULL. The inner
//! data path is a `mxlsrc` when the resolved configuration pins a
//! Domain path + Flow id + a recognised essence shape (from `caps`
//! or the transport_file's `format`); otherwise the bin keeps a
//! placeholder `fakesrc` so the element looks valid in the pipeline
//! until an IS-05 activation (or a later configuration update)
//! supplies the missing pieces.
//!
//! Activations arriving on the daemon subscription are dispatched to
//! [`NmosSrc::apply_activation`], which marshals the work onto the
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
        "nmossrc",
        gst::DebugColorFlags::empty(),
        Some("NMOS receiver wrapper element"),
    )
});

#[derive(Debug, Clone)]
struct Settings {
    daemon_uri: String,
    node_seed: String,
    transport: Transport,
    receiver_name: String,
    mxl_domain_id: String,
    mxl_domain_path: String,
    mxl_flow_id: String,
    label: String,
    description: String,
    transport_file: String,
    transport_file_path: String,
    caps: Option<gst::Caps>,
    transport_caps: Option<gst::Caps>,
    receiver_caps: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            daemon_uri: DEFAULT_DAEMON_URI.to_owned(),
            node_seed: String::new(),
            transport: Transport::default(),
            receiver_name: String::new(),
            mxl_domain_id: String::new(),
            mxl_domain_path: String::new(),
            mxl_flow_id: String::new(),
            label: String::new(),
            description: String::new(),
            transport_file: String::new(),
            transport_file_path: String::new(),
            caps: None,
            transport_caps: None,
            receiver_caps: true,
        }
    }
}

#[derive(Default)]
pub struct NmosSrc {
    settings: Mutex<Settings>,
    session: Mutex<Option<Session>>,
    /// Ghost pad that hides the current inner element behind the bin.
    /// Created at `constructed`, re-targeted at NULL↔READY transitions
    /// as the inner element swaps between the placeholder and a real
    /// `mxlsrc`.
    ghost: Mutex<Option<gst::GhostPad>>,
}

#[glib::object_subclass]
impl ObjectSubclass for NmosSrc {
    const NAME: &'static str = "GstNmosSrc";
    type Type = super::NmosSrc;
    type ParentType = gst::Bin;
}

impl ObjectImpl for NmosSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("daemon-uri")
                    .nick("Daemon URI")
                    .blurb(
                        "gRPC endpoint for nvnmosd. Only `unix:/path/to/sock` URIs are \
                         currently supported.",
                    )
                    .default_value(Some(DEFAULT_DAEMON_URI))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("node-seed")
                    .nick("Node seed")
                    .blurb(
                        "NvNmos Node seed (node_config.seed). Required. Sessions sharing \
                         this seed contribute to the same NMOS Node.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("transport", Transport::Mxl)
                    .nick("Transport")
                    .blurb(
                        "Inner data path family. Only `mxl` is currently supported; the \
                         other values exist for ABI stability and are rejected.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("receiver-name")
                    .nick("NMOS receiver name")
                    .blurb(
                        "Name for this Receiver within the Node (becomes the \
                         `x-nvnmos-name` SDP attribute or the \
                         `urn:x-nvnmos:tag:name` flow-def tag in the \
                         transport file). Unique across Receivers on the \
                         Node; a Sender on the same Node may share the \
                         same name (the daemon scopes names by side).",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("mxl-domain-id")
                    .nick("MXL Domain id")
                    .blurb(
                        "MXL Domain identifier (UUID) advertised in NMOS as \
                         `urn:x-nvnmos:tag:mxl-domain-id` in the transport_file. \
                         Required when transport=mxl, but may be omitted if \
                         `mxl-domain-path` points at a directory containing a \
                         `domain_def.json` (AMWA BCP-007-03 WIP): the file's \
                         `id` is then used. When both are supplied they must \
                         agree.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("mxl-domain-path")
                    .nick("MXL Domain path")
                    .blurb(
                        "Local filesystem path identifying the MXL Domain on \
                         this host. If the directory contains a \
                         `domain_def.json` (AMWA BCP-007-03 WIP) its `id` is \
                         used to populate `mxl-domain-id` (or cross-checked \
                         against it when both are set). Fed into the inner \
                         `mxlsrc` `domain=` property at NULL→READY when an \
                         `mxl-flow-id` is also pinned.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("mxl-flow-id")
                    .nick("MXL flow id")
                    .blurb(
                        "MXL flow id (UUID) the inner `mxlsrc` should pull. \
                         An NMOS Receiver is normally configured by IS-05 \
                         PATCH activation, so this is mainly a development \
                         convenience: setting it (plus `caps` and \
                         `mxl-domain-path`) lets the receiver start up \
                         pre-bound to a known flow. When `transport-file` \
                         also carries an `id` the two must agree.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("label")
                    .nick("Label")
                    .blurb("NMOS label for the receiver. Optional.")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("description")
                    .nick("Description")
                    .blurb("NMOS description for the receiver. Optional.")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("transport-file")
                    .nick("Transport file")
                    .blurb(
                        "Literal contents of the IS-05 transport file: MXL flow_def JSON \
                         today; SDP later. Pass the text, not a path. Convenient for \
                         programmatic callers; from gst-launch use `transport-file-path` \
                         instead. Mutually exclusive with `transport-file-path`. \
                         Required unless `caps` is provided.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("transport-file-path")
                    .nick("Transport file path")
                    .blurb(
                        "Filesystem path read at NULL\u{2192}READY into `transport-file`. \
                         Convenience for gst-launch; mutually exclusive with \
                         `transport-file`.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("caps")
                    .nick("Essence caps")
                    .blurb(
                        "Essence-shaped pad caps. Required if `transport-file` is not \
                         provided: the media-type structure name (`video/x-raw` / \
                         `audio/x-raw` / `meta/x-st-2038`) decides which `mxlsrc` flow-id \
                         slot receives `mxl-flow-id`. Cross-checked against the \
                         transport_file's `format` field when both are supplied.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("transport-caps")
                    .nick("Transport caps")
                    .blurb(
                        "Per-transport overrides (SDP fmtp-style). Typically empty for MXL.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("receiver-caps")
                    .nick("Receiver caps mode")
                    .blurb(
                        "When true (default), IS-04 publishes narrow Receiver Caps \
                         derived from the transport_file and activations carrying a \
                         structurally different transport_file are rejected. \
                         When false, IS-04 publishes wide Receiver Caps. Narrow-mode \
                         rejection is not yet wired up; the property is accepted today.",
                    )
                    .default_value(true)
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
            "transport" => {
                settings.transport = value.get().expect("type checked upstream");
            }
            "receiver-name" => {
                settings.receiver_name = string_or_empty(value);
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
            "receiver-caps" => {
                settings.receiver_caps = value.get().expect("type checked upstream");
            }
            _ => unimplemented!("unknown property {}", pspec.name()),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "daemon-uri" => settings.daemon_uri.to_value(),
            "node-seed" => settings.node_seed.to_value(),
            "transport" => settings.transport.to_value(),
            "receiver-name" => settings.receiver_name.to_value(),
            "mxl-domain-id" => settings.mxl_domain_id.to_value(),
            "mxl-domain-path" => settings.mxl_domain_path.to_value(),
            "mxl-flow-id" => settings.mxl_flow_id.to_value(),
            "label" => settings.label.to_value(),
            "description" => settings.description.to_value(),
            "transport-file" => settings.transport_file.to_value(),
            "transport-file-path" => settings.transport_file_path.to_value(),
            "caps" => settings.caps.to_value(),
            "transport-caps" => settings.transport_caps.to_value(),
            "receiver-caps" => settings.receiver_caps.to_value(),
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
            Err(e) => gst::error!(CAT, "failed to build nmossrc placeholder data path: {e}"),
        }
    }
}

impl GstObjectImpl for NmosSrc {}

impl ElementImpl for NmosSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "NMOS receiver",
                "Source/Network/NMOS",
                "NMOS Receiver wrapper element backed by nvnmosd",
                "NVIDIA Corporation",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            vec![
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &gst::Caps::new_any(),
                )
                .expect("building nmossrc src pad template"),
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
                        gst::ResourceError::OpenRead,
                        ["nmossrc NULL\u{2192}READY failed: {e:#}"]
                    );
                    return Err(gst::StateChangeError);
                }
            }
            gst::StateChange::ReadyToNull => {
                self.close_session();
            }
            _ => (),
        }
        self.parent_change_state(transition)
    }
}

impl BinImpl for NmosSrc {}

impl NmosSrc {
    fn open_session(&self) -> Result<(), anyhow::Error> {
        let snapshot = self.settings.lock().unwrap().clone();
        let bin = self.obj();
        let bin_ref: &gst::Bin = bin.upcast_ref();
        let handler = self.activation_handler();
        let outcome = crate::session::validate_and_open(
            &CAT,
            "nmossrc",
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
        if let InnerConfig::Mxl { domain_path, flow_id, format, transport_file } = outcome {
            let advertise_caps = derive_advertise_caps(transport_file.as_deref())?;
            let mxlsrc = inner::build_mxlsrc(
                domain_path,
                flow_id,
                *format,
                advertise_caps.as_ref(),
            )?;
            self.swap_inner(bin, &mxlsrc)?;
        }
        Ok(())
    }

    fn close_session(&self) {
        crate::session::close(&CAT, "nmossrc", &self.session);
        let bin = self.obj();
        let bin_ref: &gst::Bin = bin.upcast_ref();
        match inner::build_placeholder_src() {
            Ok(placeholder) => {
                if let Err(e) = self.swap_inner(bin_ref, &placeholder) {
                    gst::warning!(CAT, "restoring nmossrc placeholder: {e:#}");
                }
            }
            Err(e) => gst::warning!(CAT, "rebuilding nmossrc placeholder: {e:#}"),
        }
    }

    fn swap_inner(&self, bin: &gst::Bin, new_inner: &gst::Element) -> Result<(), anyhow::Error> {
        let ghost_guard = self.ghost.lock().unwrap();
        let ghost = ghost_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("nmossrc ghost pad missing"))?;
        inner::swap_inner(bin, ghost, new_inner, "src")
    }

    /// Build the [`ActivationHandler`] passed to
    /// [`crate::session::validate_and_open`]. Captures a weak ref to
    /// the element so the session's activation task doesn't keep
    /// the bin alive on its own.
    fn activation_handler(&self) -> ActivationHandler {
        let weak: glib::WeakRef<super::NmosSrc> = self.obj().downgrade();
        Arc::new(move |req: ActivationRequest, tx: oneshot::Sender<ActivationOutcome>| {
            let Some(bin) = weak.upgrade() else {
                let _ = tx.send(ActivationOutcome::Failed {
                    reason: "nmossrc element was dropped before activation could be applied"
                        .to_owned(),
                });
                return;
            };
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
        let plan = crate::session::plan_activation(&CAT, "nmossrc", &snapshot.into(), &req);
        gst::info!(
            CAT,
            "nmossrc applying activation (activation_handle={}, resource_handle={}, side={:?}): \
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
            InnerConfig::Mxl { domain_path, flow_id, format, transport_file } => {
                let advertise_caps = match derive_advertise_caps(transport_file.as_deref()) {
                    Ok(c) => c,
                    Err(e) => {
                        return ActivationOutcome::Failed {
                            reason: format!(
                                "nmossrc: deriving caps from activation transport_file: {e:#}"
                            ),
                        };
                    }
                };
                match inner::build_mxlsrc(domain_path, flow_id, *format, advertise_caps.as_ref()) {
                    Ok(e) => e,
                    Err(e) => {
                        return ActivationOutcome::Failed {
                            reason: format!("nmossrc: building inner mxlsrc: {e:#}"),
                        };
                    }
                }
            }
            InnerConfig::Placeholder { .. } => match inner::build_placeholder_src() {
                Ok(e) => e,
                Err(e) => {
                    return ActivationOutcome::Failed {
                        reason: format!("nmossrc: building placeholder: {e:#}"),
                    };
                }
            },
        };
        if let Err(e) = self.swap_inner(bin_ref, &new_inner) {
            if let Ok(p) = inner::build_placeholder_src() {
                let _ = self.swap_inner(bin_ref, &p);
            }
            return ActivationOutcome::Failed {
                reason: format!("nmossrc: swapping inner element: {e:#}"),
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
    /// ghost pad; when it fires, run [`apply_plan_inline`] and
    /// forward the outcome via `tx`.
    fn schedule_apply_via_probe(
        &self,
        plan: ActivationPlan,
        tx: oneshot::Sender<ActivationOutcome>,
    ) {
        let ghost = self.ghost.lock().unwrap().clone();
        let Some(ghost) = ghost else {
            let _ = tx.send(ActivationOutcome::Failed {
                reason: "nmossrc ghost pad missing when scheduling activation probe".to_owned(),
            });
            return;
        };
        let weak: glib::WeakRef<super::NmosSrc> = self.obj().downgrade();
        let plan_cell = Arc::new(Mutex::new(Some(plan)));
        let tx_cell = Arc::new(Mutex::new(Some(tx)));
        ghost.add_probe(gst::PadProbeType::IDLE, move |_pad, _info| {
            let plan = plan_cell.lock().unwrap().take();
            let tx = tx_cell.lock().unwrap().take();
            if let (Some(plan), Some(tx)) = (plan, tx) {
                let outcome = match weak.upgrade() {
                    Some(bin) => bin.imp().apply_plan_inline(&plan),
                    None => ActivationOutcome::Failed {
                        reason: "nmossrc dropped before activation probe fired".to_owned(),
                    },
                };
                let _ = tx.send(outcome);
            }
            gst::PadProbeReturn::Remove
        });
    }
}

fn install_initial_placeholder(bin: &gst::Bin) -> Result<gst::GhostPad, glib::BoolError> {
    let placeholder = inner::build_placeholder_src()
        .map_err(|e| glib::bool_error!("{e}"))?;
    let ghost = inner::build_initial(bin, placeholder, "src", gst::PadDirection::Src)?;
    bin.add_pad(&ghost)
        .map_err(|e| glib::bool_error!("adding ghost pad to nmossrc: {e}"))?;
    Ok(ghost)
}

/// Reverse-map a resolved transport_file into essence caps that
/// the bin should advertise on its ghost src pad. `None` is
/// returned when no transport_file is in play (development
/// convenience path where only properties are set); the caller
/// then builds a bare `mxlsrc` whose broad pad template propagates.
fn derive_advertise_caps(
    transport_file: Option<&str>,
) -> Result<Option<gst::Caps>, anyhow::Error> {
    let Some(text) = transport_file.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let caps = crate::flow_def::caps_from_flow_def(text).map_err(|e| {
        anyhow::anyhow!(
            "deriving essence caps from transport_file for ghost-pad advertisement: {e}",
        )
    })?;
    gst::info!(CAT, "nmossrc: advertising caps `{caps}` from transport_file");
    Ok(Some(caps))
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
            transport: s.transport,
            side: crate::session::Side::Receiver,
            name: s.receiver_name,
            mxl_domain_id: s.mxl_domain_id,
            mxl_domain_path: s.mxl_domain_path,
            mxl_flow_id: s.mxl_flow_id,
            transport_file: s.transport_file,
            transport_file_path: s.transport_file_path,
            label: s.label,
            description: s.description,
            caps: s.caps,
        }
    }
}
