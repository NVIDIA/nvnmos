// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nmossink` impl: GstBin subclass that opens a session against
//! `nvnmosd` at NULL→READY and closes it at READY→NULL. The inner
//! data path is a *real* transport chain (`mxlsink`, `udpsink` with
//! RTP payloader, or `nvdsudpsink`) when configuration is complete
//! for the chosen `transport`; otherwise the bin keeps a *fake* chain
//! (`capsfilter ! fakesink` when essence caps are known, else bare
//! `fakesink`) so the element looks valid in the pipeline until an
//! IS-05 activation (or a later configuration update) supplies the
//! missing pieces.
//!
//! Activations arriving on the daemon subscription are dispatched to
//! [`NmosSink::apply_activation`], which marshals the work onto a
//! GStreamer worker thread via `Element::call_async`. The swap itself
//! is gated by the anchor + block-probe pattern in
//! [`crate::inner::rebuild_chain`] (an `IDLE | BLOCK_DOWNSTREAM`
//! probe on the anchor's chain-side pad); doing the gating from
//! inside the `call_async` worker keeps the swap off the streaming
//! thread, so `set_state(Null)` on the old inner safely joins its
//! streaming task cross-thread rather than recursively trying to
//! join the very thread the activation handler is running on.

use std::sync::{Arc, LazyLock, Mutex};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use tokio::sync::oneshot;

use anyhow::anyhow;

use crate::daemon::{ActivationHandler, ActivationOutcome, ActivationRequest, Session};
use crate::inner;
use crate::session::{
    ActivationAck, ActivationPlan, CommonSettings, InnerConfig, NodeSettings, TransportConfig,
};
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
    host_name: String,
    domain: String,
    registration_url: String,
    system_url: String,
    transport: Transport,
    sender_name: String,
    mxl_domain_id: String,
    mxl_domain_path: String,
    mxl_flow_id: String,
    label: String,
    description: String,
    group_hint: String,
    transport_file: String,
    transport_file_path: String,
    caps: Option<gst::Caps>,
    transport_caps: Option<gst::Caps>,
    /// IS-05 sender transport_params `source_ip` — local egress
    /// NIC IP. See [`crate::session::CommonSettings::source_ip`]
    /// for the per-side transport-parameter semantics. Empty
    /// string = unset.
    source_ip: String,
    /// IS-05 sender transport_params `source_port` — local egress
    /// port. 0 = unset.
    source_port: u16,
    /// IS-05 sender transport_params `destination_ip` — remote
    /// destination (unicast peer or multicast group). Empty
    /// string = unset.
    destination_ip: String,
    /// IS-05 sender transport_params `destination_port` — remote
    /// destination port. 0 = unset (falls back to the transport
    /// file's `m=` port or
    /// [`crate::sdp::defaults::RTP_PORT`]).
    destination_port: u16,
    format_bit_rate: u64,
    transport_bit_rate: u64,
    auto_activate: bool,
    transport_properties: Option<gst::Structure>,
    pay_properties: Option<gst::Structure>,
}

impl Default for Settings {
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
            sender_name: String::new(),
            mxl_domain_id: String::new(),
            mxl_domain_path: String::new(),
            mxl_flow_id: String::new(),
            label: String::new(),
            description: String::new(),
            group_hint: String::new(),
            transport_file: String::new(),
            transport_file_path: String::new(),
            caps: None,
            transport_caps: None,
            source_ip: String::new(),
            source_port: 0,
            destination_ip: String::new(),
            destination_port: 0,
            format_bit_rate: 0,
            transport_bit_rate: 0,
            auto_activate: false,
            transport_properties: None,
            pay_properties: None,
        }
    }
}

#[derive(Default)]
pub struct NmosSink {
    settings: Mutex<Settings>,
    session: Mutex<Option<Session>>,
    /// Ghost pad that hides the current inner chain behind the bin.
    /// Created at `constructed`; the chain behind it swaps between
    /// the fake chain and a real inner transport chain as configuration
    /// / activations land.
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
                    .build(),
                glib::ParamSpecString::builder("node-seed")
                    .nick("Node seed")
                    .blurb(crate::session::NODE_SEED_BLURB)
                    .build(),
                glib::ParamSpecUInt::builder("http-port")
                    .nick("HTTP port")
                    .blurb(crate::session::HTTP_PORT_BLURB)
                    .minimum(0)
                    .maximum(65535)
                    .default_value(0)
                    .build(),
                glib::ParamSpecString::builder("host-name")
                    .nick("Host name")
                    .blurb(crate::session::HOST_NAME_BLURB)
                    .build(),
                glib::ParamSpecString::builder("domain")
                    .nick("NMOS DNS domain")
                    .blurb(crate::session::DOMAIN_BLURB)
                    .build(),
                glib::ParamSpecString::builder("registration-url")
                    .nick("Registration URL")
                    .blurb(crate::session::REGISTRATION_URL_BLURB)
                    .build(),
                glib::ParamSpecString::builder("system-url")
                    .nick("System URL")
                    .blurb(crate::session::SYSTEM_URL_BLURB)
                    .build(),
                glib::ParamSpecEnum::builder_with_default("transport", Transport::Mxl)
                    .nick("Transport")
                    .blurb(crate::session::TRANSPORT_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("sender-name")
                    .nick("NMOS sender name")
                    .blurb(crate::session::SENDER_NAME_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("mxl-domain-id")
                    .nick("MXL domain id")
                    .blurb(crate::session::MXL_DOMAIN_ID_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("mxl-domain-path")
                    .nick("MXL domain path")
                    .blurb(
                        "Local filesystem path identifying the MXL Domain on \
                         this host. If the directory contains a \
                         `domain_def.json` (AMWA BCP-007-03 WIP) its `id` is \
                         used to populate `mxl-domain-id` (or cross-checked \
                         against it when both are set). Without \
                         `domain_def.json`, an unset `mxl-domain-id` leaves \
                         the NMOS tag application-resolved while the data plane \
                         still uses this path. The path itself will be consumed \
                         by the inner `mxlsink` `domain=` property when the \
                         data path is wired up.",
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
                    .blurb(crate::session::LABEL_BLURB_SENDER)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("description")
                    .nick("Description")
                    .blurb(crate::session::DESCRIPTION_BLURB_SENDER)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("group-hint")
                    .nick("Group hint")
                    .blurb(crate::session::GROUP_HINT_BLURB_SENDER)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("transport-file")
                    .nick("Transport file")
                    .blurb(crate::session::TRANSPORT_FILE_BLURB_SENDER)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("transport-file-path")
                    .nick("Transport file path")
                    .blurb(crate::session::TRANSPORT_FILE_PATH_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("caps")
                    .nick("Essence caps")
                    .blurb(crate::session::CAPS_BLURB_SENDER)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("transport-caps")
                    .nick("Transport caps")
                    .blurb(crate::session::TRANSPORT_CAPS_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("transport-properties")
                    .nick("Transport sink properties")
                    .blurb(crate::session::TRANSPORT_PROPERTIES_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("pay-properties")
                    .nick("Payloader properties")
                    .blurb(crate::session::PAY_PROPERTIES_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("source-ip")
                    .nick("Source IP")
                    .blurb(
                        "IS-05 sender transport_params `source_ip`: \
                         local egress NIC IP. Drives both the SDP \
                         `a=source-filter:` include-source (RFC 4607 \
                         SSM convention) and the `a=x-nvnmos-iface-ip:` \
                         attribute, and `udpsink.bind-address` on the \
                         RTP transports (`udp`, `udp2`, `nvdsudp`). \
                         Empty = unset (leave the daemon / SDP / \
                         IS-05 `auto` resolver to fill at activation \
                         time). Honoured only on the RTP transports; \
                         ignored on `mxl`.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("source-port")
                    .nick("Source port")
                    .blurb(
                        "IS-05 sender transport_params `source_port`: \
                         local egress port. Drives `udpsink.bind-port` \
                         and the SDP `a=x-nvnmos-src-port:` attribute \
                         on the RTP transports. 0 (the default) = \
                         unset; the OS picks an ephemeral port. \
                         Honoured only on the RTP transports; ignored \
                         on `mxl`.",
                    )
                    .minimum(0)
                    .maximum(65535)
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("destination-ip")
                    .nick("Destination IP")
                    .blurb(
                        "IS-05 sender transport_params `destination_ip`: \
                         remote destination (unicast peer or multicast \
                         group). Becomes the configuring SDP `c=` line \
                         address and `udpsink.host` on the RTP \
                         transports. Empty = unset (use the transport \
                         file's `c=` line if present; otherwise the \
                         daemon fills the IS-05 `auto` sentinel). \
                         Honoured only on the RTP transports; ignored \
                         on `mxl`.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("destination-port")
                    .nick("Destination port")
                    .blurb(
                        "IS-05 sender transport_params \
                         `destination_port`: remote destination port. \
                         Becomes the configuring SDP `m=` line port \
                         and `udpsink.port` on the RTP transports. 0 \
                         (the default) = unset; falls back to the \
                         transport file's `m=` port if present, else \
                         to the canonical RTP default 5004 \
                         (`nmos-cpp` `auto_rtp_port`). Honoured only \
                         on the RTP transports; ignored on `mxl`.",
                    )
                    .minimum(0)
                    .maximum(65535)
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("format-bit-rate")
                    .nick("Format bit rate")
                    .blurb(crate::session::FORMAT_BIT_RATE_BLURB)
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("transport-bit-rate")
                    .nick("Transport bit rate")
                    .blurb(crate::session::TRANSPORT_BIT_RATE_BLURB)
                    .default_value(0)
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
            "host-name" => {
                settings.host_name = string_or_empty(value);
            }
            "domain" => {
                settings.domain = string_or_empty(value);
            }
            "registration-url" => {
                settings.registration_url = string_or_empty(value);
            }
            "system-url" => {
                settings.system_url = string_or_empty(value);
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
            "group-hint" => {
                settings.group_hint = string_or_empty(value);
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
            "transport-properties" => {
                settings.transport_properties = value.get().expect("type checked upstream");
            }
            "pay-properties" => {
                settings.pay_properties = value.get().expect("type checked upstream");
            }
            "source-ip" => {
                settings.source_ip = string_or_empty(value);
            }
            "source-port" => {
                let v: u32 = value.get().expect("type checked upstream");
                settings.source_port = u16::try_from(v).expect("range checked by ParamSpec");
            }
            "destination-ip" => {
                settings.destination_ip = string_or_empty(value);
            }
            "destination-port" => {
                let v: u32 = value.get().expect("type checked upstream");
                settings.destination_port = u16::try_from(v).expect("range checked by ParamSpec");
            }
            "format-bit-rate" => {
                settings.format_bit_rate = value.get().expect("type checked upstream");
            }
            "transport-bit-rate" => {
                settings.transport_bit_rate = value.get().expect("type checked upstream");
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
            "host-name" => settings.host_name.to_value(),
            "domain" => settings.domain.to_value(),
            "registration-url" => settings.registration_url.to_value(),
            "system-url" => settings.system_url.to_value(),
            "transport" => settings.transport.to_value(),
            "sender-name" => settings.sender_name.to_value(),
            "mxl-domain-id" => settings.mxl_domain_id.to_value(),
            "mxl-domain-path" => settings.mxl_domain_path.to_value(),
            "mxl-flow-id" => settings.mxl_flow_id.to_value(),
            "auto-activate" => settings.auto_activate.to_value(),
            "label" => settings.label.to_value(),
            "description" => settings.description.to_value(),
            "group-hint" => settings.group_hint.to_value(),
            "transport-file" => settings.transport_file.to_value(),
            "transport-file-path" => settings.transport_file_path.to_value(),
            "caps" => settings.caps.to_value(),
            "transport-caps" => settings.transport_caps.to_value(),
            "transport-properties" => settings.transport_properties.to_value(),
            "pay-properties" => settings.pay_properties.to_value(),
            "source-ip" => settings.source_ip.to_value(),
            "source-port" => u32::from(settings.source_port).to_value(),
            "destination-ip" => settings.destination_ip.to_value(),
            "destination-port" => u32::from(settings.destination_port).to_value(),
            "format-bit-rate" => settings.format_bit_rate.to_value(),
            "transport-bit-rate" => settings.transport_bit_rate.to_value(),
            _ => unimplemented!("unknown property {}", pspec.name()),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        let bin = self.obj();
        let bin_ref: &gst::Bin = bin.upcast_ref();
        match install_initial_fake_chain(bin_ref) {
            Ok(ghost) => {
                *self.ghost.lock().unwrap() = Some(ghost);
            }
            Err(e) => gst::error!(CAT, "failed to build nmossink fake chain: {e}"),
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
                // Deferred senders: pin the fake chain from upstream
                // peer caps *before* child negotiation so sticky caps
                // match what AddSender will register. Early-known caps
                // were pinned at NULL→READY; this path is skipped then.
                let bin = self.obj();
                let bin_ref: &gst::Bin = bin.upcast_ref();
                if let Err(e) = self.prepare_deferred_fake_chain(bin_ref) {
                    gst::element_imp_error!(
                        self,
                        gst::ResourceError::OpenWrite,
                        ["nmossink READY\u{2192}PAUSED deferred fake-chain pin failed: {e:#}"]
                    );
                    return Err(gst::StateChangeError);
                }
                let res = self.parent_change_state(transition)?;
                if let Err(e) = self.maybe_add_deferred_sender() {
                    gst::element_imp_error!(
                        self,
                        gst::ResourceError::OpenWrite,
                        ["nmossink READY\u{2192}PAUSED deferred AddSender failed: {e:#}"]
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
            // Close the daemon session and restore the fake chain so
            // the bin is left as if NULL→READY had never been
            // attempted.
            self.close_session();
            return Err(e);
        }
        Ok(())
    }

    fn activate_inner(&self, bin: &gst::Bin, outcome: &InnerConfig) -> Result<(), anyhow::Error> {
        match outcome {
            InnerConfig::Real(transport) => {
                let settings = self.settings.lock().unwrap();
                let new_inner = build_real_sink(transport, &settings)?;
                self.swap_inner(bin, &new_inner)?;
                // Reaching the `Real` branch at NULL→READY / READY→PAUSED
                // implies `auto-activate=true` (the `validate_and_open` and
                // `add_deferred_sender` gates downgrade to a fake chain
                // otherwise). Tell the daemon to bring the resource's
                // IS-04/IS-05 view up to match the live data path so
                // external state stays consistent without an external
                // IS-05 PATCH.
                if let Err(e) = crate::session::sync_active(
                    &CAT,
                    "nmossink",
                    &self.session,
                    transport.transport_file(),
                ) {
                    // Inner is up and pushing already; don't tear it down
                    // for a SyncResourceState glitch, but surface the
                    // failure as a warning so it shows up in logs.
                    gst::warning!(CAT, "nmossink auto-activate sync failed: {e:#}");
                }
            }
            InnerConfig::Fake { .. } => {
                self.upgrade_fake_chain_from_settings(bin)?;
            }
        }
        Ok(())
    }

    fn close_session(&self) {
        crate::session::close(&CAT, "nmossink", &self.session);
        let bin = self.obj();
        let bin_ref: &gst::Bin = bin.upcast_ref();
        let snapshot = self.settings.lock().unwrap().clone();
        match build_fake_sink_for_settings(&snapshot) {
            Ok(fake) => {
                if let Err(e) = self.swap_inner(bin_ref, &fake) {
                    gst::warning!(CAT, "restoring nmossink fake chain: {e:#}");
                }
            }
            Err(e) => gst::warning!(CAT, "rebuilding nmossink fake chain: {e:#}"),
        }
    }

    /// Replace the bare constructed-time fake chain with a caps-pinned
    /// one when `caps` or `transport-file*` supplies essence caps.
    fn upgrade_fake_chain_from_settings(&self, bin: &gst::Bin) -> Result<(), anyhow::Error> {
        let snapshot = self.settings.lock().unwrap().clone();
        let caps = fake_caps_from_settings(&snapshot)?;
        if let Some(caps) = caps {
            gst::info!(CAT, "nmossink pinning fake chain to `{caps}`");
            let fake = inner::build_fake_sink(Some(&caps))?;
            self.swap_inner(bin, &fake)?;
        } else {
            gst::debug!(
                CAT,
                "nmossink fake chain caps unknown at NULL→READY; \
                 deferred mode will pin from peer caps at READY→PAUSED",
            );
        }
        Ok(())
    }

    /// Deferred AddSender path: query upstream peer caps and pin the
    /// fake chain before child negotiation. No-op when essence caps
    /// were already known at NULL→READY.
    fn prepare_deferred_fake_chain(&self, bin: &gst::Bin) -> Result<(), anyhow::Error> {
        let snapshot = self.settings.lock().unwrap().clone();
        if !snapshot.transport_file.is_empty()
            || !snapshot.transport_file_path.is_empty()
            || snapshot.caps.is_some()
        {
            return Ok(());
        }
        let session_open = self.session.lock().unwrap().is_some();
        let resource_added = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.resource_id().map(|_| ()))
            .is_some();
        if !session_open || resource_added {
            return Ok(());
        }

        let ghost = self
            .ghost
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("nmossink ghost pad missing"))?;
        let peer_caps = ghost.peer_query_caps(None);
        gst::debug!(
            CAT,
            imp = self,
            "deferred fake-chain peer_query_caps -> {peer_caps}"
        );
        let fixated = crate::session::prepare_deferred_peer_caps("nmossink", peer_caps)?;
        gst::info!(CAT, "nmossink pinning deferred fake chain to `{fixated}`");
        let fake = inner::build_fake_sink(Some(&fixated))?;
        self.swap_inner(bin, &fake)?;
        Ok(())
    }

    fn swap_inner(&self, bin: &gst::Bin, new_inner: &gst::Element) -> Result<(), anyhow::Error> {
        let ghost_guard = self.ghost.lock().unwrap();
        let ghost = ghost_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("nmossink ghost pad missing"))?;
        inner::rebuild_chain(&CAT, bin, ghost, new_inner, "sink")
    }

    /// True iff the bin's current inner chain is a real transport
    /// chain — not the fake one (`fakesink`). Used by
    /// [`execute_activation_plan`] to insert a fake hop into
    /// real → real re-activations.
    fn current_chain_is_real(&self) -> bool {
        self.ghost
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(inner::current_chain_is_real)
    }

    /// Drive a deferred `AddSender` from inside
    /// `change_state(ReadyToPaused)`. Only attempts AddSender when
    /// the session is open without a resource and neither
    /// `transport-file*` nor `caps` were supplied at NULL→READY. The
    /// ghost sink pad is queried for the upstream peer's caps, which
    /// are then fed to the shared caps-driven transport-file builder; on
    /// success the inner element is swapped to the real transport chain.
    ///
    /// Returns `Ok(())` both when deferred mode is not applicable and
    /// when AddSender succeeds. Errors are propagated only on real
    /// failures (ANY/EMPTY caps, builder rejection, AddSender RPC
    /// failure) so that change_state surfaces a clear,
    /// pipeline-visible error.
    fn maybe_add_deferred_sender(&self) -> Result<(), anyhow::Error> {
        let snapshot = self.settings.lock().unwrap().clone();
        let static_inputs_set = !snapshot.transport_file.is_empty()
            || !snapshot.transport_file_path.is_empty()
            || snapshot.caps.is_some();
        let resource_added = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.resource_id().map(|_| ()))
            .is_some();
        let session_open = self.session.lock().unwrap().is_some();
        if !session_open || resource_added || static_inputs_set {
            return Ok(());
        }

        let ghost = self
            .ghost
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("nmossink ghost pad missing"))?;
        let peer_caps = ghost.peer_query_caps(None);
        gst::debug!(
            CAT,
            imp = self,
            "deferred mode peer_query_caps -> {peer_caps}"
        );

        let common: crate::session::CommonSettings = snapshot.into();
        let outcome = crate::session::add_deferred_sender(
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
        Arc::new(
            move |req: ActivationRequest, tx: oneshot::Sender<ActivationOutcome>| {
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
            },
        )
    }

    /// Apply an activation. Runs on a GStreamer worker thread (via
    /// `call_async` from the daemon subscription task). The swap
    /// itself is gated by the anchor + block-probe pattern in
    /// [`crate::inner::rebuild_chain`], which installs an
    /// `IDLE | BLOCK_DOWNSTREAM` probe on the anchor's chain-side
    /// pad and waits for the pad to drain before mutating the bin's
    /// child set. Doing the gating from the `call_async` worker
    /// (rather than from a probe on the bin's external ghost pad)
    /// keeps the swap off the streaming thread, so
    /// `set_state(Null)` on the old inner can cleanly join its
    /// streaming task cross-thread.
    fn apply_activation(&self, req: ActivationRequest, tx: oneshot::Sender<ActivationOutcome>) {
        let snapshot = self.settings.lock().unwrap().clone();
        let plan = crate::session::make_activation_plan(&CAT, "nmossink", &snapshot.into(), &req);
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

        let outcome = self.execute_activation_plan(&plan);
        let _ = tx.send(outcome);
    }

    /// Perform the inner swap and translate the result into an
    /// [`ActivationOutcome`]. Always called on a `call_async`
    /// worker thread, so [`crate::inner::rebuild_chain`]'s
    /// `set_state(Null)` on the old chain joins the streaming task
    /// cross-thread. On swap failure the element is left on the
    /// fake chain and the outcome is `Failed`.
    ///
    /// For real → real re-activations (idempotent re-enable or
    /// flow-id change) we drop to the fake chain between the two
    /// real instances so any transport-side per-process state (e.g.
    /// libmxl's `FlowWriter`) is fully released before the new
    /// instance tries to allocate it. A direct real → real swap
    /// intermittently fails the new chain's start-up for reasons
    /// internal to the transport; the intermediate fake hop is one
    /// extra `rebuild_chain` cycle and reliably avoids the failure.
    fn execute_activation_plan(&self, plan: &ActivationPlan) -> ActivationOutcome {
        let bin = self.obj();
        let bin_ref: &gst::Bin = bin.upcast_ref();
        let settings = self.settings.lock().unwrap().clone();

        if matches!(plan.inner, InnerConfig::Real(_)) && self.current_chain_is_real() {
            gst::debug!(
                CAT,
                "nmossink activation is real\u{2192}real; inserting fake hop \
                 to fully release the old transport state before re-allocating",
            );
            match intermediate_fake_sink_caps(plan, &settings)
                .and_then(|caps| inner::build_fake_sink(caps.as_ref()))
            {
                Ok(p) => {
                    if let Err(e) = self.swap_inner(bin_ref, &p) {
                        return ActivationOutcome::Failed {
                            reason: format!("nmossink: intermediate fake-chain swap failed: {e:#}"),
                        };
                    }
                }
                Err(e) => {
                    return ActivationOutcome::Failed {
                        reason: format!("nmossink: building intermediate fake chain: {e:#}"),
                    };
                }
            }
        }

        let new_inner = match &plan.inner {
            InnerConfig::Real(transport) => match build_real_sink(transport, &settings) {
                Ok(bin) => bin,
                Err(e) => {
                    return ActivationOutcome::Failed {
                        reason: format!("nmossink: building inner transport chain: {e:#}"),
                    };
                }
            },
            InnerConfig::Fake { .. } => {
                // Deactivation, side mismatch, missing config, etc.
                // The bin may be in PLAYING when this happens, so
                // we still need caps on the fake chain so upstream
                // stays negotiated across the swap.
                match fake_caps_from_settings(&settings)
                    .and_then(|caps| inner::build_fake_sink(caps.as_ref()))
                {
                    Ok(e) => e,
                    Err(e) => {
                        return ActivationOutcome::Failed {
                            reason: format!("nmossink: building fake chain: {e:#}"),
                        };
                    }
                }
            }
        };
        if let Err(e) = self.swap_inner(bin_ref, &new_inner) {
            // Loud-log the swap failure so the cause shows up in
            // the producer log, not just stuffed into the daemon's
            // (currently-discarded) `AckActivation` reason.
            gst::warning!(
                CAT,
                "nmossink activation swap failed; restoring fake chain: {e:#}",
            );
            // Try one more time with a fresh fake chain so the bin
            // is left in a known state even on the failure path.
            if let Ok(p) = build_fake_sink_for_settings(&settings) {
                if let Err(e2) = self.swap_inner(bin_ref, &p) {
                    gst::warning!(CAT, "nmossink fake-chain restore also failed: {e2:#}",);
                }
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
}

/// Build the real transport inner element for `nmossink` — counterpart to
/// [`inner::build_fake_sink`]. Applies `transport-properties` /
/// `pay-properties`, then pins the transport sink for mid-stream swap.
fn build_real_sink(
    transport: &TransportConfig,
    settings: &Settings,
) -> Result<gst::Element, anyhow::Error> {
    let (bin, transport_sink) = match transport {
        TransportConfig::Mxl {
            domain_path,
            flow_id,
            ..
        } => {
            let chain = inner::build_mxlsink(domain_path, flow_id)?;
            inner::apply_mxl_sink_inner_properties(
                &CAT,
                "nmossink",
                &chain,
                settings.transport_properties.as_ref(),
                settings.pay_properties.as_ref(),
            );
            (chain.bin, chain.transport)
        }
        TransportConfig::Udp { variant, media, .. } => {
            let chain = inner::build_udpsink(media, *variant)?;
            let property = crate::sdp::bit_rates_from_properties(
                settings.format_bit_rate,
                settings.transport_bit_rate,
            );
            let bit_rates = crate::sdp::effective_bit_rates(property, media.bit_rates);
            inner::apply_format_bit_rate_to_jxsv_payloader(
                media,
                &chain.pay,
                bit_rates.format_bit_rate,
                settings.pay_properties.as_ref(),
            );
            inner::apply_udp_sink_inner_properties(
                &CAT,
                "nmossink",
                &chain,
                settings.transport_properties.as_ref(),
                settings.pay_properties.as_ref(),
            );
            (chain.bin, chain.transport)
        }
        TransportConfig::NvDsUdp {
            media,
            transport_file,
            ..
        } => {
            let sdp = transport_file.as_deref().unwrap_or("");
            let chain = inner::build_nvdsudpsink(media, sdp)?;
            inner::apply_nvdsudp_sink_inner_properties(
                &CAT,
                "nmossink",
                &chain,
                media,
                settings.transport_properties.as_ref(),
                settings.pay_properties.as_ref(),
            );
            (chain.bin, chain.transport)
        }
    };
    // GstBaseSink defaults to async=true, which makes READY→PAUSED wait for
    // preroll — fine at pipeline start, but a deadlock when rebuild_chain
    // swaps a sink behind the anchor block probe. Pin after
    // transport-properties so users cannot override back to async=true.
    if transport_sink.has_property("async") {
        transport_sink.set_property("async", false);
    }
    Ok(bin)
}

fn install_initial_fake_chain(bin: &gst::Bin) -> Result<gst::GhostPad, glib::BoolError> {
    let fake = inner::build_fake_sink(None).map_err(|e| glib::bool_error!("{e}"))?;
    let ghost = inner::build_initial(bin, fake, "sink", gst::PadDirection::Sink)?;
    bin.add_pad(&ghost)
        .map_err(|e| glib::bool_error!("adding ghost pad to nmossink: {e}"))?;
    Ok(ghost)
}

fn build_fake_sink_for_settings(settings: &Settings) -> Result<gst::Element, anyhow::Error> {
    let caps = fake_caps_from_settings(settings).ok().flatten();
    inner::build_fake_sink(caps.as_ref())
}

fn fake_caps_from_settings(settings: &Settings) -> Result<Option<gst::Caps>, anyhow::Error> {
    crate::session::fake_caps_from_settings(
        "nmossink",
        settings.transport,
        settings.caps.as_ref(),
        &settings.transport_file,
        &settings.transport_file_path,
    )
}

/// Caps for the intermediate fake chain on real→real activations.
///
/// For [`InnerConfig::Real`] RTP/UDP plans, pin essence caps from the
/// incoming activation [`UdpMedia`] (same shape the real payloader or
/// `nvdsudpsink` Mode 3 expects). MXL senders ignore the activation
/// transport file for the real build — fall back to [`fake_caps_from_settings`].
///
/// Keeps the fake hop aligned with the incoming real chain when element
/// `caps` / `transport-file*` differs from the activation transport.
fn intermediate_fake_sink_caps(
    plan: &ActivationPlan,
    settings: &Settings,
) -> Result<Option<gst::Caps>, anyhow::Error> {
    match &plan.inner {
        InnerConfig::Real(TransportConfig::Udp { media, .. })
        | InnerConfig::Real(TransportConfig::NvDsUdp { media, .. }) => Ok(Some(
            crate::essence_caps::caps_from(&media.caps, Some(&media.rtp_caps)),
        )),
        InnerConfig::Real(TransportConfig::Mxl { .. }) => fake_caps_from_settings(settings),
        _ => fake_caps_from_settings(settings),
    }
}

fn string_or_empty(value: &glib::Value) -> String {
    value
        .get::<Option<String>>()
        .expect("type checked upstream")
        .unwrap_or_default()
}

impl From<Settings> for CommonSettings {
    fn from(s: Settings) -> Self {
        CommonSettings {
            daemon_uri: s.daemon_uri,
            node: NodeSettings {
                node_seed: s.node_seed,
                http_port: s.http_port,
                host_name: s.host_name,
                domain: s.domain,
                registration_url: s.registration_url,
                system_url: s.system_url,
            },
            transport: s.transport,
            side: crate::session::types::Side::Sender,
            name: s.sender_name,
            mxl_domain_id: s.mxl_domain_id,
            mxl_domain_path: s.mxl_domain_path,
            mxl_flow_id: s.mxl_flow_id,
            transport_file: s.transport_file,
            transport_file_path: s.transport_file_path,
            label: s.label,
            description: s.description,
            group_hint: s.group_hint,
            caps: s.caps,
            transport_caps: s.transport_caps,
            caps_mode: crate::types::CapsMode::Auto,
            source_ip: s.source_ip,
            source_port: s.source_port,
            destination_ip: s.destination_ip,
            destination_port: s.destination_port,
            format_bit_rate: s.format_bit_rate,
            transport_bit_rate: s.transport_bit_rate,
            // Receiver-only slots: empty/0 on the Sender side.
            // `nmossrc::From<Settings>` populates these instead.
            interface_ip: String::new(),
            multicast_ip: String::new(),
            auto_activate: s.auto_activate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::CommonSettings;
    use crate::test_support::init_gst;

    /// Pins the IS-05 Sender `transport_params` → `CommonSettings`
    /// mapping. `nmossink` populates the sender-side slots
    /// (`source_ip`, `source_port`, `destination_ip`,
    /// `destination_port`) directly and zeros the receiver-only
    /// slots (`interface_ip`, `multicast_ip`); the SDP splice
    /// helper reads from there.
    #[test]
    fn from_settings_populates_is05_sender_transport_params() {
        let s = Settings {
            source_ip: "192.0.2.10".to_owned(),
            source_port: 5005,
            destination_ip: "239.1.1.1".to_owned(),
            destination_port: 5004,
            ..Settings::default()
        };
        let cs: CommonSettings = s.into();
        assert_eq!(cs.source_ip, "192.0.2.10");
        assert_eq!(cs.source_port, 5005);
        assert_eq!(cs.destination_ip, "239.1.1.1");
        assert_eq!(cs.destination_port, 5004);
        assert_eq!(
            cs.interface_ip, "",
            "receiver-only on the Sender side must be empty"
        );
        assert_eq!(
            cs.multicast_ip, "",
            "receiver-only on the Sender side must be empty"
        );
        assert_eq!(cs.side, crate::session::types::Side::Sender);
    }

    #[test]
    fn from_settings_defaults_leave_route_b_unset() {
        let cs: CommonSettings = Settings::default().into();
        assert_eq!(cs.source_ip, "");
        assert_eq!(cs.source_port, 0);
        assert_eq!(cs.destination_ip, "");
        assert_eq!(cs.destination_port, 0);
        assert_eq!(cs.interface_ip, "");
        assert_eq!(cs.multicast_ip, "");
        assert_eq!(cs.transport_caps, None);
    }

    /// Pins that the `transport-caps` property propagates into
    /// `CommonSettings` so the splice / synth path can read it for
    /// pt / audio clock-rate / ptime overrides + essence
    /// cross-check.
    #[test]
    fn from_settings_forwards_network_services_properties() {
        let s = Settings {
            host_name: "studio-a".to_owned(),
            domain: "local".to_owned(),
            registration_url: "http://reg:3210/x-nmos/registration/v1.3".to_owned(),
            system_url: "http://sys:10641/x-nmos/system/v1.0".to_owned(),
            ..Settings::default()
        };
        let cs: CommonSettings = s.into();
        assert_eq!(cs.node.host_name, "studio-a");
        assert_eq!(cs.node.domain, "local");
        assert_eq!(
            cs.node.registration_url,
            "http://reg:3210/x-nmos/registration/v1.3"
        );
        assert_eq!(cs.node.system_url, "http://sys:10641/x-nmos/system/v1.0");
    }

    #[test]
    fn from_settings_forwards_transport_caps() {
        use std::str::FromStr;
        init_gst();
        let caps = gst::Caps::from_str("application/x-rtp,media=audio,payload=99,clock-rate=48000")
            .expect("valid caps");
        let s = Settings {
            transport_caps: Some(caps.clone()),
            ..Settings::default()
        };
        let cs: CommonSettings = s.into();
        assert_eq!(cs.transport_caps.as_ref(), Some(&caps));
    }

    fn udp_activation_plan(sdp: &str) -> ActivationPlan {
        use crate::session::udp::UdpVariant;
        let media = crate::sdp::parse_sdp(sdp).expect("parse sdp");
        ActivationPlan {
            inner: InnerConfig::Real(TransportConfig::Udp {
                variant: UdpVariant::V1,
                media,
                transport_file: Some(sdp.to_owned()),
            }),
            ack: ActivationAck::Success,
        }
    }

    /// Real→real: fake-hop caps follow activation [`UdpMedia`], not stale
    /// element `caps` from an earlier chain.
    #[test]
    fn intermediate_fake_sink_caps_udp_uses_activation_media() {
        use std::str::FromStr;
        init_gst();
        const NARROW_SDP: &str = concat!(
            "v=0\r\n",
            "o=- 1 0 IN IP4 192.0.2.10\r\n",
            "s=Example\r\n",
            "t=0 0\r\n",
            "m=video 5004 RTP/AVP 96\r\n",
            "c=IN IP4 239.2.2.2/64\r\n",
            "a=rtpmap:96 raw/90000\r\n",
            "a=fmtp:96 sampling=YCbCr-4:2:2; width=1920; height=1080; exactframerate=25/1; depth=10\r\n",
        );
        let activation_caps =
            intermediate_fake_sink_caps(&udp_activation_plan(NARROW_SDP), &Settings::default())
                .expect("activation caps")
                .expect("narrow UDP must pin fake-hop caps");
        let structure = activation_caps.structure(0).expect("structure");
        assert_eq!(structure.name(), "video/x-raw");
        assert_eq!(structure.get::<&str>("format").ok(), Some("UYVP"));

        let stale_element_caps =
            gst::Caps::from_str("video/x-raw,format=RGB,width=640,height=480,framerate=30/1")
                .expect("stale caps");
        let settings = Settings {
            caps: Some(stale_element_caps),
            ..Settings::default()
        };
        let hop_caps = intermediate_fake_sink_caps(&udp_activation_plan(NARROW_SDP), &settings)
            .expect("activation caps")
            .expect("narrow UDP must pin fake-hop caps");
        assert_eq!(
            hop_caps
                .structure(0)
                .expect("structure")
                .get::<&str>("format")
                .ok(),
            Some("UYVP"),
            "fake-hop caps must follow activation media, not element property",
        );
    }

    #[test]
    fn intermediate_fake_sink_caps_mxl_uses_element_settings() {
        use std::str::FromStr;
        init_gst();
        let caps =
            gst::Caps::from_str("video/x-raw,format=v210,width=1920,height=1080,framerate=25/1")
                .expect("caps");
        let settings = Settings {
            caps: Some(caps.clone()),
            ..Settings::default()
        };
        let plan = ActivationPlan {
            inner: InnerConfig::Real(TransportConfig::Mxl {
                domain_path: "/dev/shm/test".to_owned(),
                flow_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                format: crate::types::FlowFormat::Video,
                transport_file: None,
            }),
            ack: ActivationAck::Success,
        };
        assert_eq!(
            intermediate_fake_sink_caps(&plan, &settings).expect("mxl hop"),
            Some(caps),
        );
    }
}
