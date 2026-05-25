// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nmossink` impl: GstBin subclass that opens a session against
//! `nvnmosd` at NULL→READY and closes it at READY→NULL. The inner
//! data path is a `mxlsink` when the resolved configuration pins a
//! Domain path + Flow id; otherwise the bin keeps a placeholder
//! `fakesink` so the element looks valid in the pipeline until a
//! later step supplies the missing pieces.

use std::sync::{LazyLock, Mutex};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;

use crate::daemon::Session;
use crate::inner;
use crate::session::InnerConfig;
use crate::types::{DEFAULT_DAEMON_URI, FlowFormat, Transport};

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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            daemon_uri: DEFAULT_DAEMON_URI.to_owned(),
            node_seed: String::new(),
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
                glib::ParamSpecString::builder("sender-name")
                    .nick("NMOS sender name")
                    .blurb(
                        "Name for this Sender within the Node (becomes the \
                         `x-nvnmos-name` SDP attribute or the \
                         `urn:x-nvnmos:tag:name` flow-def tag in the \
                         transport file). Unique across Senders on the \
                         Node; a Receiver on the same Node may share the \
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
                         against it when both are set). The path itself will \
                         be consumed by the inner `mxlsink` `domain=` \
                         property when the data path is wired up.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("mxl-flow-id")
                    .nick("MXL flow id")
                    .blurb(
                        "Override for the MXL flow id assigned to this sender. \
                         Defaults to a value derived from `sender-name`.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("label")
                    .nick("Label")
                    .blurb("NMOS label for the sender. Optional.")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("description")
                    .nick("Description")
                    .blurb("NMOS description for the sender. Optional.")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("transport-file")
                    .nick("Transport file")
                    .blurb(
                        "Literal contents of the IS-05 transport file: MXL flow_def JSON \
                         today; SDP later. Pass the text, not a path. Convenient for \
                         programmatic callers; from gst-launch use `transport-file-path` \
                         instead. Mutually exclusive with `transport-file-path`. \
                         When unset and `caps` is supplied the element synthesises a \
                         flow_def from the essence caps.",
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
                        "Essence caps used to synthesise the MXL `flow_def` JSON when \
                         `transport-file` / `transport-file-path` are unset. Supported \
                         shapes match `mxlsink`'s pad template: `video/x-raw,format=v210,…`, \
                         `audio/x-raw,format=F32LE,…`, and `meta/x-st-2038,framerate=…`. \
                         Requires `mxl-flow-id` to be set. Ignored when `transport-file*` \
                         is also set.",
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
            "transport" => settings.transport.to_value(),
            "sender-name" => settings.sender_name.to_value(),
            "mxl-domain-id" => settings.mxl_domain_id.to_value(),
            "mxl-domain-path" => settings.mxl_domain_path.to_value(),
            "mxl-flow-id" => settings.mxl_flow_id.to_value(),
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
        let outcome =
            crate::session::validate_and_open(&CAT, "nmossink", &snapshot.into(), &self.session)?;
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
        if let InnerConfig::Mxl { domain_path, flow_id, .. } = outcome {
            let mxlsink = inner::build_mxlsink(domain_path, flow_id)?;
            self.swap_inner(bin, &mxlsink)?;
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
            transport: s.transport,
            side: crate::session::Side::Sender,
            name: s.sender_name,
            mxl_domain_id: s.mxl_domain_id,
            mxl_domain_path: s.mxl_domain_path,
            mxl_flow_id: s.mxl_flow_id,
            // mxlsink has a single flow-id slot so the receiver-only
            // `mxl-flow-format` property doesn't exist on this side.
            mxl_flow_format: FlowFormat::Unspecified,
            transport_file: s.transport_file,
            transport_file_path: s.transport_file_path,
            label: s.label,
            description: s.description,
            caps: s.caps,
        }
    }
}
