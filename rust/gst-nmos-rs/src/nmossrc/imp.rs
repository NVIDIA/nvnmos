// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nmossrc` impl: GstBin subclass that opens a session against
//! `nvnmosd` at NULL→READY and closes it at READY→NULL. The inner
//! data path is a *real* chain — today a `mxlsrc` — when the
//! resolved configuration pins a Domain path + Flow id + a
//! recognised essence shape (from `caps` or the transport file's
//! `format`); otherwise the bin keeps a *fake* chain so the element
//! looks valid in the pipeline until an IS-05 activation (or a
//! later configuration update) supplies the missing pieces. The
//! fake chain is an `appsrc` configured with the best-available
//! essence caps (user `caps` property, synthesised from
//! `transport-file`*); if no caps source is yet available, the
//! appsrc is built without caps and downstream negotiation will
//! fail until one is supplied. See [`inner`] for the details.
//!
//! Activations arriving on the daemon subscription are dispatched to
//! [`NmosSrc::apply_activation`], which marshals the work onto a
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

use crate::daemon::{ActivationHandler, ActivationOutcome, ActivationRequest, Session};
use crate::inner;
use crate::session::{ActivationAck, ActivationPlan, InnerConfig, TransportConfig};
use crate::types::{CapsMode, DEFAULT_DAEMON_URI, Transport};

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
    http_port: u16,
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
    receiver_caps_mode: CapsMode,
    /// IS-05 receiver transport_params `source_ip` — SSM
    /// include-source (the remote sender's IP). See
    /// [`crate::session::CommonSettings::source_ip`]; empty
    /// string = unset.
    source_ip: String,
    /// IS-05 receiver transport_params `interface_ip` — local
    /// NIC IP used for the IGMP join. Resolved to an interface
    /// name via [`crate::iface::iface_name_for_ip`] and fed into
    /// `udpsrc.multicast-iface`. Empty string = unset.
    interface_ip: String,
    /// IS-05 receiver transport_params `multicast_ip` — multicast
    /// group to join. Becomes `udpsrc.address` and the SDP `c=`
    /// line address. Empty string = unset (unicast reception).
    multicast_ip: String,
    /// IS-05 receiver transport_params `destination_port` — local
    /// listen port (becomes `udpsrc.port` and the SDP `m=` port
    /// slot). 0 = unset (falls back to the transport file's `m=`
    /// port or [`crate::sdp::defaults::RTP_PORT`]).
    destination_port: u16,
    auto_activate: bool,
    transport_properties: Option<gst::Structure>,
    depay_properties: Option<gst::Structure>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            daemon_uri: DEFAULT_DAEMON_URI.to_owned(),
            node_seed: String::new(),
            http_port: 0,
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
            receiver_caps_mode: CapsMode::Auto,
            source_ip: String::new(),
            interface_ip: String::new(),
            multicast_ip: String::new(),
            destination_port: 0,
            auto_activate: false,
            transport_properties: None,
            depay_properties: None,
        }
    }
}

#[derive(Default)]
pub struct NmosSrc {
    settings: Mutex<Settings>,
    session: Mutex<Option<Session>>,
    /// Ghost pad that hides the current inner chain behind the bin.
    /// Created at `constructed`; the chain behind it swaps between
    /// the fake chain and a real `mxlsrc` sub-bin as configuration
    /// / activations land.
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
                glib::ParamSpecString::builder("receiver-name")
                    .nick("NMOS receiver name")
                    .blurb(
                        "Name for this Receiver within the Node (becomes the \
                         `x-nvnmos-name` SDP attribute or the \
                         `urn:x-nvnmos:tag:name` flow-def tag in the \
                         transport file). Unique across Receivers on the \
                         Node; a Sender on the same Node may share the \
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
                         convenience: combined with `auto-activate=true` \
                         (plus `caps` and `mxl-domain-path`) the receiver \
                         starts up pre-bound to a known flow without an \
                         external controller. Overrides the transport \
                         file's top-level `id` when both are supplied.",
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
                        "NMOS label for the Receiver. Optional. Overrides \
                         the transport file's top-level `label` when both \
                         are supplied.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("description")
                    .nick("Description")
                    .blurb(
                        "NMOS description for the Receiver. Optional. \
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
                         `transport-file-path`. Required unless `caps` is provided.",
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
                        "Essence-shaped pad caps. Required if `transport-file` is not \
                         provided: the media-type structure name (`video/x-raw` / \
                         `audio/x-raw` / `meta/x-st-2038`) decides which `mxlsrc` flow-id \
                         slot receives `mxl-flow-id`. Cross-checked against the \
                         transport file's `format` field when both are supplied — \
                         mismatch is a hard error (the caps and the flow's essence shape \
                         must describe the same thing).",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("transport-caps")
                    .nick("Transport caps")
                    .blurb(crate::session::TRANSPORT_CAPS_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("transport-properties")
                    .nick("Transport source properties")
                    .blurb(crate::session::TRANSPORT_PROPERTIES_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("depay-properties")
                    .nick("Depayloader properties")
                    .blurb(crate::session::DEPAY_PROPERTIES_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder::<CapsMode>("receiver-caps-mode")
                    .nick("Receiver caps mode")
                    .blurb(
                        "Selects whether the published NMOS Receiver advertises narrow \
                         or wide Receiver Caps in IS-04, via the presence of the \
                         `urn:x-nvnmos:tag:caps` tag on the flow_def. `auto` (default) \
                         leaves the tag untouched in the spliced transport file: the \
                         result is narrow when the transport file is present and the \
                         tag is absent (or no transport file is in play), and wide \
                         when the tag is already there. `narrow` strips the tag from \
                         the transport file if present. `wide` ensures the tag is \
                         present with a non-empty marker (libnvnmos's rule for wide \
                         is \"present + non-empty\").",
                    )
                    .default_value(CapsMode::Auto)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("source-ip")
                    .nick("Source IP")
                    .blurb(
                        "IS-05 receiver transport_params `source_ip`: \
                         SSM include-source — the remote sender's IP. \
                         Drives the configuring SDP \
                         `a=source-filter:` include-source. On the \
                         `udp2` (gst-plugins-rs `udpsrc2`) variant \
                         this translates to `source-filter`; on the \
                         `udp` (gst-plugins-good `udpsrc`) variant it \
                         translates to `multicast-source`. Empty = \
                         unset (any-source multicast / unicast). \
                         Honoured only on the RTP transports; ignored \
                         on `mxl`.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("interface-ip")
                    .nick("Interface IP")
                    .blurb(
                        "IS-05 receiver transport_params \
                         `interface_ip`: local NIC IP used for the \
                         IGMP join. Resolved to an interface name and \
                         fed into `udpsrc.multicast-iface` on the RTP \
                         transports. Also emitted in the configuring \
                         SDP as `a=x-nvnmos-iface-ip:`. Empty = unset \
                         (let the kernel pick). Honoured only on the \
                         RTP transports; ignored on `mxl`.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("multicast-ip")
                    .nick("Multicast IP")
                    .blurb(
                        "IS-05 receiver transport_params \
                         `multicast_ip`: multicast group to join. \
                         Becomes `udpsrc.address` and the SDP `c=` \
                         line address on the RTP transports. Empty = \
                         unset (unicast reception). Honoured only on \
                         the RTP transports; ignored on `mxl`.",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("destination-port")
                    .nick("Destination port")
                    .blurb(
                        "IS-05 receiver transport_params \
                         `destination_port`: local listen port \
                         (becomes `udpsrc.port` and the SDP `m=` port \
                         slot). 0 (the default) = unset; falls back \
                         to the transport file's `m=` port if \
                         present, else to the canonical RTP default \
                         5004 (`nmos-cpp` `auto_rtp_port`). Honoured \
                         only on the RTP transports; ignored on `mxl`.",
                    )
                    .minimum(0)
                    .maximum(65535)
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
                settings.http_port = u16::try_from(v).expect("range checked by ParamSpec");
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
            "transport-properties" => {
                settings.transport_properties = value.get().expect("type checked upstream");
            }
            "depay-properties" => {
                settings.depay_properties = value.get().expect("type checked upstream");
            }
            "receiver-caps-mode" => {
                settings.receiver_caps_mode = value.get().expect("type checked upstream");
            }
            "source-ip" => {
                settings.source_ip = string_or_empty(value);
            }
            "interface-ip" => {
                settings.interface_ip = string_or_empty(value);
            }
            "multicast-ip" => {
                settings.multicast_ip = string_or_empty(value);
            }
            "destination-port" => {
                let v: u32 = value.get().expect("type checked upstream");
                settings.destination_port = u16::try_from(v).expect("range checked by ParamSpec");
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
            "receiver-name" => settings.receiver_name.to_value(),
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
            "transport-properties" => settings.transport_properties.to_value(),
            "depay-properties" => settings.depay_properties.to_value(),
            "receiver-caps-mode" => settings.receiver_caps_mode.to_value(),
            "source-ip" => settings.source_ip.to_value(),
            "interface-ip" => settings.interface_ip.to_value(),
            "multicast-ip" => settings.multicast_ip.to_value(),
            "destination-port" => u32::from(settings.destination_port).to_value(),
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
            Err(e) => gst::error!(CAT, "failed to build nmossrc fake chain: {e}"),
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
            // Close the daemon session and restore the fake chain so
            // the bin is left as if NULL→READY had never been
            // attempted.
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
        match outcome {
            InnerConfig::Real(transport) => {
                let new_inner = match transport {
                    TransportConfig::Mxl {
                        domain_path,
                        flow_id,
                        format,
                        transport_file,
                    } => {
                        let advertise_caps = derive_advertise_caps(transport_file.as_deref())?;
                        {
                            let chain = inner::build_mxlsrc(
                                domain_path,
                                flow_id,
                                *format,
                                advertise_caps.as_ref(),
                            )?;
                            let settings = self.settings.lock().unwrap();
                            inner::apply_mxl_src_inner_properties(
                                &CAT,
                                "nmossrc",
                                &chain,
                                settings.transport_properties.as_ref(),
                                settings.depay_properties.as_ref(),
                            );
                            chain.bin
                        }
                    }
                    TransportConfig::Udp { variant, media, .. } => {
                        // Receiver-side advertise_caps for UDP is the
                        // essence shape carried by `media.raw_caps`
                        // (derived from the SDP transport-file at
                        // resolution time). Pinned via the bare
                        // factory call here; the factory itself wraps
                        // the depayloader + a trailing capsfilter so
                        // downstream caps queries see the concrete
                        // shape the flow will carry, mirroring the
                        // `mxlsrc ! capssetter` sub-bin pattern.
                        {
                            let chain = inner::build_udpsrc(
                                media,
                                *variant,
                                Some(&media.raw_caps),
                            )?;
                            let settings = self.settings.lock().unwrap();
                            inner::apply_udp_src_inner_properties(
                                &CAT,
                                "nmossrc",
                                &chain,
                                settings.transport_properties.as_ref(),
                                settings.depay_properties.as_ref(),
                            );
                            chain.bin
                        }
                    }
                };
                self.swap_inner(bin, &new_inner)?;
                // Reaching the `Real` branch at NULL→READY implies
                // `auto-activate=true` (the `validate_and_open` gate
                // downgrades to a fake chain otherwise). Tell the
                // daemon to bring the resource's IS-04/IS-05 view up
                // to match the live data path so the
                // `/single/receivers/{id}/active` endpoint reflects
                // `master_enable: true` without an external IS-05
                // PATCH.
                if let Err(e) = crate::session::sync_active(
                    &CAT,
                    "nmossrc",
                    &self.session,
                    transport.transport_file(),
                ) {
                    gst::warning!(CAT, "nmossrc auto-activate sync failed: {e:#}");
                }
            }
            InnerConfig::Fake { .. } => {
                // The constructed-time fake chain is a bare `appsrc`
                // with no caps (no settings were available yet).
                // Whenever we have caps to advertise — from the
                // user-set `caps` property or synthesised from
                // `transport-file*` — swap it for a caps-aware
                // `appsrc` so downstream negotiation can complete
                // and the pipeline can reach PLAYING while the bin
                // waits for an IS-05 activation. Without a caps
                // source we leave the bare `appsrc` in place; the
                // pipeline will then fail caps negotiation on its
                // way to PLAYING and the user must supply `caps`
                // (or `transport-file*`).
                let snapshot = self.settings.lock().unwrap().clone();
                let caps = fake_caps_from_settings(&snapshot)?;
                if let Some(caps) = caps {
                    let fake = inner::build_fake_src(Some(&caps))?;
                    self.swap_inner(bin, &fake)?;
                } else {
                    gst::warning!(
                        CAT,
                        "nmossrc fake chain has no caps to advertise; \
                         downstream caps negotiation will fail until \
                         `caps` or `transport-file*` is set",
                    );
                }
            }
        }
        Ok(())
    }

    fn close_session(&self) {
        crate::session::close(&CAT, "nmossrc", &self.session);
        let bin = self.obj();
        let bin_ref: &gst::Bin = bin.upcast_ref();
        // The bin is heading back to NULL, so caps aren't strictly
        // required to flow data, but we still hand the best-known
        // caps to the fake chain so a subsequent NULL→READY (or
        // simple state-query) doesn't see a regression to ANY.
        let snapshot = self.settings.lock().unwrap().clone();
        let caps = fake_caps_from_settings(&snapshot).ok().flatten();
        match inner::build_fake_src(caps.as_ref()) {
            Ok(fake) => {
                if let Err(e) = self.swap_inner(bin_ref, &fake) {
                    gst::warning!(CAT, "restoring nmossrc fake chain: {e:#}");
                }
            }
            Err(e) => gst::warning!(CAT, "rebuilding nmossrc fake chain: {e:#}"),
        }
    }

    fn swap_inner(&self, bin: &gst::Bin, new_inner: &gst::Element) -> Result<(), anyhow::Error> {
        let ghost_guard = self.ghost.lock().unwrap();
        let ghost = ghost_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("nmossrc ghost pad missing"))?;
        inner::rebuild_chain(&CAT, bin, ghost, new_inner, "src")
    }

    /// True iff the bin's current inner chain is a real chain
    /// (today only the `mxlsrc` sub-bin) — not the fake `appsrc`.
    /// Used by [`execute_activation_plan`] to insert a fake hop on
    /// real → real re-activations.
    fn current_chain_is_real(&self) -> bool {
        self.ghost
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(inner::current_chain_is_real)
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
        let plan = crate::session::make_activation_plan(&CAT, "nmossrc", &snapshot.into(), &req);
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
    /// libmxl's `FlowReader`) is fully released before the new
    /// instance tries to attach. A direct real → real swap
    /// intermittently fails the new chain's start-up for reasons
    /// internal to the transport; the intermediate fake hop is one
    /// extra `rebuild_chain` cycle and reliably avoids the failure.
    fn execute_activation_plan(&self, plan: &ActivationPlan) -> ActivationOutcome {
        let bin = self.obj();
        let bin_ref: &gst::Bin = bin.upcast_ref();

        if matches!(plan.inner, InnerConfig::Real(_))
            && self.current_chain_is_real()
        {
            gst::debug!(
                CAT,
                "nmossrc activation is real\u{2192}real; inserting fake hop \
                 to fully release the old transport state before re-attaching",
            );
            let snapshot = self.settings.lock().unwrap().clone();
            let caps = fake_caps_from_settings(&snapshot).ok().flatten();
            match inner::build_fake_src(caps.as_ref()) {
                Ok(p) => {
                    if let Err(e) = self.swap_inner(bin_ref, &p) {
                        gst::warning!(
                            CAT,
                            "nmossrc intermediate fake-chain swap failed: {e:#}",
                        );
                    }
                }
                Err(e) => gst::warning!(
                    CAT,
                    "nmossrc: building intermediate fake chain: {e:#}",
                ),
            }
        }

        let new_inner = match &plan.inner {
            InnerConfig::Real(TransportConfig::Mxl {
                domain_path, flow_id, format, transport_file,
            }) => {
                let advertise_caps = match derive_advertise_caps(transport_file.as_deref()) {
                    Ok(c) => c,
                    Err(e) => {
                        return ActivationOutcome::Failed {
                            reason: format!(
                                "nmossrc: deriving caps from activation transport file: {e:#}"
                            ),
                        };
                    }
                };
                let settings = self.settings.lock().unwrap();
                match inner::build_mxlsrc(domain_path, flow_id, *format, advertise_caps.as_ref()) {
                    Ok(chain) => {
                        inner::apply_mxl_src_inner_properties(
                            &CAT,
                            "nmossrc",
                            &chain,
                            settings.transport_properties.as_ref(),
                            settings.depay_properties.as_ref(),
                        );
                        chain.bin
                    }
                    Err(e) => {
                        return ActivationOutcome::Failed {
                            reason: format!("nmossrc: building inner mxlsrc: {e:#}"),
                        };
                    }
                }
            }
            InnerConfig::Real(TransportConfig::Udp { variant, media, .. }) => {
                let settings = self.settings.lock().unwrap();
                match inner::build_udpsrc(media, *variant, Some(&media.raw_caps)) {
                    Ok(chain) => {
                        inner::apply_udp_src_inner_properties(
                            &CAT,
                            "nmossrc",
                            &chain,
                            settings.transport_properties.as_ref(),
                            settings.depay_properties.as_ref(),
                        );
                        chain.bin
                    }
                    Err(e) => {
                        return ActivationOutcome::Failed {
                            reason: format!("nmossrc: building inner udpsrc: {e:#}"),
                        };
                    }
                }
            }
            InnerConfig::Fake { .. } => {
                // Deactivation, side mismatch, missing config, etc.
                // The bin may be in PLAYING when this happens, so
                // we still need caps on the fake chain so
                // downstream stays negotiated across the swap.
                let snapshot = self.settings.lock().unwrap().clone();
                let caps = match fake_caps_from_settings(&snapshot) {
                    Ok(c) => c,
                    Err(e) => {
                        return ActivationOutcome::Failed {
                            reason: format!(
                                "nmossrc: resolving fake-chain caps for activation: {e:#}"
                            ),
                        };
                    }
                };
                match inner::build_fake_src(caps.as_ref()) {
                    Ok(e) => e,
                    Err(e) => {
                        return ActivationOutcome::Failed {
                            reason: format!("nmossrc: building fake chain: {e:#}"),
                        };
                    }
                }
            }
        };
        if let Err(e) = self.swap_inner(bin_ref, &new_inner) {
            // Loud-log the swap failure so the cause shows up in
            // the consumer log, not just stuffed into the daemon's
            // (currently-discarded) `AckActivation` reason.
            gst::warning!(
                CAT,
                "nmossrc activation swap failed; restoring fake chain: {e:#}",
            );
            // Best-effort: try to restore a caps-aware fake chain
            // so the bin doesn't end up with a bare appsrc that
            // can't negotiate. If caps resolution fails here we
            // fall back to the bare fake chain rather than failing
            // twice.
            let snapshot = self.settings.lock().unwrap().clone();
            let fallback_caps = fake_caps_from_settings(&snapshot).ok().flatten();
            if let Ok(p) = inner::build_fake_src(fallback_caps.as_ref()) {
                if let Err(e2) = self.swap_inner(bin_ref, &p) {
                    gst::warning!(
                        CAT,
                        "nmossrc fake-chain restore also failed: {e2:#}",
                    );
                }
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
}

fn install_initial_fake_chain(bin: &gst::Bin) -> Result<gst::GhostPad, glib::BoolError> {
    // Constructed-time fake chain: no properties have been set yet,
    // so we have no caps source. Build a bare `appsrc` without caps;
    // it'll be replaced at NULL→READY (`activate_inner`) once `caps`
    // / `transport-file*` are known.
    let fake = inner::build_fake_src(None)
        .map_err(|e| glib::bool_error!("{e}"))?;
    let ghost = inner::build_initial(bin, fake, "src", gst::PadDirection::Src)?;
    bin.add_pad(&ghost)
        .map_err(|e| glib::bool_error!("adding ghost pad to nmossrc: {e}"))?;
    Ok(ghost)
}

/// Reverse-map a resolved transport file into essence caps that
/// the bin should advertise on its ghost src pad. `None` is
/// returned when no transport file is in play (the fake chain is
/// in use until an IS-05 activation supplies one); the caller then
/// builds a bare `mxlsrc` whose broad pad template propagates.
fn derive_advertise_caps(
    transport_file: Option<&str>,
) -> Result<Option<gst::Caps>, anyhow::Error> {
    let Some(text) = transport_file.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let caps = crate::flow_def::caps_from(text).map_err(|e| {
        anyhow::anyhow!(
            "deriving essence caps from transport file for ghost-pad advertisement: {e}",
        )
    })?;
    gst::info!(CAT, "nmossrc: advertising caps `{caps}` from transport file");
    Ok(Some(caps))
}

/// Best-available caps for the bin's fake chain, resolved from
/// current `Settings` in priority order:
///   1. `caps` property (user-supplied; authoritative).
///   2. Caps synthesised from the literal `transport-file` JSON.
///   3. Caps synthesised from the JSON loaded from
///      `transport-file-path`.
///
/// Returns `Ok(None)` only when none of the three sources is
/// available (e.g. neither `caps` nor `transport-file*` has been
/// set yet); callers then build the fake-chain appsrc without
/// caps and the pipeline will fail caps negotiation if it tries
/// to reach PLAYING in that state. Filesystem / parse errors when
/// a source is set are propagated as `Err`.
fn fake_caps_from_settings(
    settings: &Settings,
) -> Result<Option<gst::Caps>, anyhow::Error> {
    if let Some(caps) = settings.caps.as_ref() {
        return Ok(Some(caps.clone()));
    }
    if !settings.transport_file.is_empty() {
        return derive_advertise_caps(Some(&settings.transport_file));
    }
    if !settings.transport_file_path.is_empty() {
        let text = std::fs::read_to_string(&settings.transport_file_path).map_err(|e| {
            anyhow::anyhow!(
                "nmossrc: re-reading `transport-file-path` = `{}` for fake-chain caps: {e}",
                settings.transport_file_path
            )
        })?;
        return derive_advertise_caps(Some(&text));
    }
    Ok(None)
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
            transport_caps: s.transport_caps,
            caps_mode: s.receiver_caps_mode,
            source_ip: s.source_ip,
            interface_ip: s.interface_ip,
            multicast_ip: s.multicast_ip,
            destination_port: s.destination_port,
            // Sender-only slots: empty/0 on the Receiver side.
            // `nmossink::From<Settings>` populates these instead.
            source_port: 0,
            destination_ip: String::new(),
            auto_activate: s.auto_activate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer::prelude::StaticType;
    use crate::session::CommonSettings;

    /// Pins the IS-05 Receiver `transport_params` →
    /// `CommonSettings` mapping. `nmossrc` populates the
    /// receiver-side slots (`source_ip`, `interface_ip`,
    /// `multicast_ip`, `destination_port`) directly and zeros
    /// the sender-only slots (`source_port`, `destination_ip`);
    /// the SDP splice helper reads from there.
    #[test]
    fn from_settings_populates_is05_receiver_transport_params() {
        let s = Settings {
            source_ip: "192.0.2.10".to_owned(),
            interface_ip: "192.0.2.20".to_owned(),
            multicast_ip: "239.1.1.1".to_owned(),
            destination_port: 5004,
            ..Settings::default()
        };
        let cs: CommonSettings = s.into();
        assert_eq!(cs.source_ip, "192.0.2.10");
        assert_eq!(cs.interface_ip, "192.0.2.20");
        assert_eq!(cs.multicast_ip, "239.1.1.1");
        assert_eq!(cs.destination_port, 5004);
        assert_eq!(cs.source_port, 0, "sender-only on the Receiver side must be 0");
        assert_eq!(cs.destination_ip, "", "sender-only on the Receiver side must be empty");
        assert_eq!(cs.side, crate::session::Side::Receiver);
    }

    #[test]
    fn from_settings_defaults_leave_route_b_unset() {
        let cs: CommonSettings = Settings::default().into();
        assert_eq!(cs.source_ip, "");
        assert_eq!(cs.interface_ip, "");
        assert_eq!(cs.multicast_ip, "");
        assert_eq!(cs.destination_port, 0);
        assert_eq!(cs.source_port, 0);
        assert_eq!(cs.destination_ip, "");
        assert_eq!(cs.transport_caps, None);
    }

    /// Pins that the `transport-caps` property propagates into
    /// `CommonSettings` so the splice / synth path can read it for
    /// pt / audio clock-rate / ptime overrides + essence
    /// cross-check.
    #[test]
    fn from_settings_forwards_transport_caps() {
        use std::str::FromStr;
        let _ = gst::init();
        let caps =
            gst::Caps::from_str("application/x-rtp,media=audio,payload=99,clock-rate=48000")
                .expect("valid caps");
        let s = Settings {
            transport_caps: Some(caps.clone()),
            ..Settings::default()
        };
        let cs: CommonSettings = s.into();
        assert_eq!(cs.transport_caps.as_ref(), Some(&caps));
    }

}
