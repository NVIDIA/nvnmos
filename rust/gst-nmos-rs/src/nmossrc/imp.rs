// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nmossrc` impl: GstBin subclass that opens a session against
//! `nvnmosd` at NULL→READY and closes it at READY→NULL. The data path
//! is currently a placeholder `fakesrc`; an `input-selector` driving
//! `mxlsrc` vs a disabled pad lands when MXL is wired up.

use std::sync::{LazyLock, Mutex};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;

use crate::daemon::Session;
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
                        "MXL Domain identifier (UUID; becomes \
                         `urn:x-nvnmos:tag:mxl-domain-id` in the transport_file). \
                         Required when transport=mxl. Translation to the inner \
                         mxlsrc `domain` filesystem path is a stub.",
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
                        "Essence-shaped pad caps used by the property route. Required if \
                         `transport-file` is not provided.",
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
        if let Err(e) = build_placeholder(self.obj().upcast_ref::<gst::Bin>()) {
            gst::error!(CAT, "failed to build nmossrc placeholder data path: {e}");
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
                        ["failed to open session against nvnmosd: {e:#}"]
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
        crate::session::validate_and_open(&CAT, "nmossrc", &snapshot.into(), &self.session)
    }

    fn close_session(&self) {
        crate::session::close(&CAT, "nmossrc", &self.session);
    }
}

fn build_placeholder(bin: &gst::Bin) -> Result<(), glib::BoolError> {
    let fakesrc = gst::ElementFactory::make("fakesrc")
        .name("nmossrc-placeholder")
        .property("is-live", true)
        .property_from_str("num-buffers", "-1")
        .build()
        .map_err(|e| glib::bool_error!("creating fakesrc placeholder: {e}"))?;
    bin.add(&fakesrc)
        .map_err(|e| glib::bool_error!("adding fakesrc to nmossrc: {e}"))?;

    let src_pad = fakesrc
        .static_pad("src")
        .expect("fakesrc always has a src pad");
    let ghost = gst::GhostPad::with_target(&src_pad)
        .map_err(|e| glib::bool_error!("ghosting fakesrc src pad: {e}"))?;
    ghost
        .set_active(true)
        .map_err(|e| glib::bool_error!("activating ghost pad: {e}"))?;
    bin.add_pad(&ghost)
        .map_err(|e| glib::bool_error!("adding ghost pad to nmossrc: {e}"))?;
    Ok(())
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
            transport_file: s.transport_file,
            transport_file_path: s.transport_file_path,
        }
    }
}
