// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, LazyLock, Mutex};

use super::internals::InternalGraph;
use super::pad::{
    NmosAudioChannelMapSinkPad, NmosAudioChannelMapSrcPad, sink_pad_templates, src_pad_templates,
};
use crate::channel_mapping::active_map::{
    active_map_entries_for_src, default_identity_routes, parse_active_map_structure,
};
use crate::channel_mapping::matrix::routes_from_activation_request;
use crate::channel_mapping::request::build_add_channel_mapping_request;
use crate::channel_mapping::types::{FrozenTopology, SinkPadSnapshot, SrcPadSnapshot};
use crate::channel_mapping_session::{
    ChannelMappingActivationHandler, ChannelMappingActivationOutcome,
    ChannelMappingActivationRequest, ChannelMappingSession,
};
use crate::session::channel_mapping::{
    CHANNELMAPPING_NAME_BLURB, ChannelMappingSettings, RESTRICT_ROUTABLE_INPUTS_BLURB,
    close as close_session, validate_and_open,
};
use crate::types::DEFAULT_DAEMON_URI;
use anyhow::{Context, bail};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "nmosaudiochannelmap",
        gst::DebugColorFlags::empty(),
        Some("NMOS IS-08 channel mapping element"),
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
    channelmapping_name: String,
    restrict_routable_inputs: bool,
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
            channelmapping_name: String::new(),
            restrict_routable_inputs: false,
        }
    }
}

impl From<&Settings> for ChannelMappingSettings {
    fn from(s: &Settings) -> Self {
        use crate::session::NodeSettings;
        ChannelMappingSettings {
            daemon_uri: s.daemon_uri.clone(),
            node: NodeSettings {
                node_seed: s.node_seed.clone(),
                http_port: s.http_port,
                host_name: s.host_name.clone(),
                domain: s.domain.clone(),
                registration_url: s.registration_url.clone(),
                system_url: s.system_url.clone(),
            },
            channelmapping_name: s.channelmapping_name.clone(),
        }
    }
}

#[derive(Default)]
pub struct NmosAudioChannelMap {
    settings: Mutex<Settings>,
    session: Mutex<Option<ChannelMappingSession>>,
    internal: Mutex<Option<InternalGraph>>,
    topology: Mutex<Option<FrozenTopology>>,
    fixated: Mutex<bool>,
    next_sink_index: Mutex<u32>,
    next_src_index: Mutex<u32>,
}

#[glib::object_subclass]
impl ObjectSubclass for NmosAudioChannelMap {
    const NAME: &'static str = "GstNmosAudioChannelMap";
    type Type = super::NmosAudioChannelMap;
    type ParentType = gst::Bin;
    type Interfaces = (gst::ChildProxy,);
}

impl ObjectImpl for NmosAudioChannelMap {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
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
                    .maximum(65535)
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
                glib::ParamSpecString::builder("channelmapping-name")
                    .nick("Channel mapping name")
                    .blurb(CHANNELMAPPING_NAME_BLURB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("restrict-routable-inputs")
                    .nick("Restrict routable inputs")
                    .blurb(RESTRICT_ROUTABLE_INPUTS_BLURB)
                    .default_value(false)
                    .mutable_ready()
                    .build(),
            ]
        });
        PROPS.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        if *self.fixated.lock().unwrap() {
            gst::error!(CAT, "element properties are not writable after fixation");
            return;
        }
        match pspec.name() {
            "daemon-uri" => settings.daemon_uri = value.get().expect("type checked"),
            "node-seed" => settings.node_seed = value.get().expect("type checked"),
            "http-port" => settings.http_port = value.get::<u32>().expect("type checked") as u16,
            "host-name" => settings.host_name = value.get().expect("type checked"),
            "domain" => settings.domain = value.get().expect("type checked"),
            "registration-url" => settings.registration_url = value.get().expect("type checked"),
            "system-url" => settings.system_url = value.get().expect("type checked"),
            "channelmapping-name" => {
                settings.channelmapping_name = value.get().expect("type checked")
            }
            "restrict-routable-inputs" => {
                settings.restrict_routable_inputs = value.get().expect("type checked")
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "daemon-uri" => settings.daemon_uri.to_value(),
            "node-seed" => settings.node_seed.to_value(),
            "http-port" => (settings.http_port as u32).to_value(),
            "host-name" => settings.host_name.to_value(),
            "domain" => settings.domain.to_value(),
            "registration-url" => settings.registration_url.to_value(),
            "system-url" => settings.system_url.to_value(),
            "channelmapping-name" => settings.channelmapping_name.to_value(),
            "restrict-routable-inputs" => settings.restrict_routable_inputs.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for NmosAudioChannelMap {}

impl ChildProxyImpl for NmosAudioChannelMap {
    fn child_by_index(&self, index: u32) -> Option<glib::Object> {
        let element = self.obj();
        let pads: Vec<gst::Pad> = element
            .pads()
            .into_iter()
            .filter(|p| p.name().starts_with("sink_") || p.name().starts_with("src_"))
            .collect();
        pads.get(index as usize)
            .map(|p| p.upcast_ref::<glib::Object>().clone())
    }

    fn children_count(&self) -> u32 {
        let element = self.obj();
        element
            .pads()
            .into_iter()
            .filter(|p| p.name().starts_with("sink_") || p.name().starts_with("src_"))
            .count() as u32
    }

    fn child_by_name(&self, name: &str) -> Option<glib::Object> {
        let element = self.obj();
        element
            .pads()
            .into_iter()
            .find(|p| p.name() == name)
            .map(|p| p.upcast_ref::<glib::Object>().clone())
    }
}

impl ElementImpl for NmosAudioChannelMap {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "NMOS IS-08 audio channel map",
                "Filter/Audio/Network",
                "IS-08 channel mapping between NMOS audio streams",
                "NVIDIA",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            sink_pad_templates()
                .iter()
                .chain(src_pad_templates())
                .cloned()
                .collect()
        });
        TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        if *self.fixated.lock().unwrap() {
            gst::error!(CAT, "cannot request new pads after fixation");
            return None;
        }
        let pad_name = match name.filter(|n| !n.is_empty()) {
            Some("sink_%u") | Some("src_%u") | None => {
                if templ.direction() == gst::PadDirection::Sink {
                    let mut n = self.next_sink_index.lock().unwrap();
                    let idx = *n;
                    *n += 1;
                    format!("sink_{idx}")
                } else {
                    let mut n = self.next_src_index.lock().unwrap();
                    let idx = *n;
                    *n += 1;
                    format!("src_{idx}")
                }
            }
            Some(n) => n.to_owned(),
        };
        let pad: gst::Pad = if templ.direction() == gst::PadDirection::Sink {
            gst::PadBuilder::<super::pad::NmosAudioChannelMapSinkPad>::from_template(templ)
                .name(pad_name.as_str())
                .build()
                .upcast()
        } else {
            gst::PadBuilder::<super::pad::NmosAudioChannelMapSrcPad>::from_template(templ)
                .name(pad_name.as_str())
                .build()
                .upcast()
        };
        let element = self.obj().clone();
        if element.add_pad(&pad).is_err() {
            return None;
        }
        // add_pad does not notify ChildProxy; gst-launch defers sink_0::props until child-added.
        element.child_added(&pad, pad_name.as_str());
        Some(pad)
    }

    fn release_pad(&self, pad: &gst::Pad) {
        if *self.fixated.lock().unwrap() {
            gst::error!(CAT, "cannot release pads after fixation");
            return;
        }
        let element = self.obj().clone();
        element.child_removed(pad, pad.name().as_ref());
        let _ = element.remove_pad(pad);
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let element = self.obj();
        match transition {
            gst::StateChange::NullToReady => {
                let settings = self.settings.lock().unwrap().clone();
                let handler = self.activation_handler();
                if let Err(e) = validate_and_open(
                    &CAT,
                    "nmosaudiochannelmap",
                    &(&settings).into(),
                    &self.session,
                    handler,
                ) {
                    element.post_error_message(gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["failed to open channel mapping session: {e:#}"]
                    ));
                    return Err(gst::StateChangeError);
                }
            }
            gst::StateChange::ReadyToNull => {
                self.teardown_internals();
                close_session(&CAT, "nmosaudiochannelmap", &self.session);
                *self.fixated.lock().unwrap() = false;
            }
            _ => {}
        }

        let ret = self.parent_change_state(transition)?;

        // Fixate after the bin has negotiated caps on its ghost pads with peers.
        if transition == gst::StateChange::ReadyToPaused {
            if !*self.fixated.lock().unwrap() {
                if let Err(e) = self.fixate_and_build() {
                    element.post_error_message(gst::error_msg!(
                        gst::StreamError::Failed,
                        ["channel mapping fixation failed: {e:#}"]
                    ));
                    return Err(gst::StateChangeError);
                }
            }
            if let Some(internal) = self.internal.lock().unwrap().as_ref() {
                internal.sync_state_with_parent()?;
            }
        }
        Ok(ret)
    }
}

impl BinImpl for NmosAudioChannelMap {}

impl NmosAudioChannelMap {
    fn activation_handler(&self) -> ChannelMappingActivationHandler {
        let weak = self.obj().downgrade();
        Arc::new(move |req, tx| {
            let Some(element) = weak.upgrade() else {
                let _ = tx.send(ChannelMappingActivationOutcome::Failed {
                    reason: "element dropped before activation".into(),
                });
                return;
            };
            element.call_async(move |map| {
                let imp = map.imp();
                let outcome = imp.apply_activation(req);
                let _ = tx.send(outcome);
            });
        })
    }

    fn apply_activation(
        &self,
        req: ChannelMappingActivationRequest,
    ) -> ChannelMappingActivationOutcome {
        gst::info!(
            CAT,
            "applying channel mapping activation (channelmapping_handle={}, \
             activation_handle={}, output_id={}, active_map_entries={})",
            req.channelmapping_handle,
            req.activation_handle,
            req.output_id,
            req.active_map.len(),
        );
        let topology = match self.topology.lock().unwrap().clone() {
            Some(t) => t,
            None => {
                return ChannelMappingActivationOutcome::Failed {
                    reason: "activation before fixation".into(),
                };
            }
        };
        let routes = match routes_from_activation_request(&topology, &req) {
            Ok(r) => r,
            Err(e) => {
                return ChannelMappingActivationOutcome::Failed {
                    reason: format!("invalid activation active map: {e}"),
                };
            }
        };
        let src_index = match topology
            .output_ids
            .iter()
            .position(|id| id == &req.output_id)
        {
            Some(i) => i,
            None => {
                return ChannelMappingActivationOutcome::Failed {
                    reason: format!("unknown output_id `{}`", req.output_id),
                };
            }
        };
        let internal = self.internal.lock().unwrap();
        let Some(graph) = internal.as_ref() else {
            return ChannelMappingActivationOutcome::Failed {
                reason: "internal graph not built".into(),
            };
        };
        if let Err(e) = graph.set_output_matrix(&topology, src_index, &routes) {
            return ChannelMappingActivationOutcome::Failed {
                reason: format!("matrix update failed: {e:#}"),
            };
        }
        ChannelMappingActivationOutcome::Applied
    }

    fn teardown_internals(&self) {
        if let Some(graph) = self.internal.lock().unwrap().take() {
            let element = self.obj().clone();
            let bin: &gst::Bin = element.upcast_ref();
            graph.teardown(bin);
        }
        *self.topology.lock().unwrap() = None;
    }

    fn fixate_and_build(&self) -> Result<(), anyhow::Error> {
        if *self.fixated.lock().unwrap() {
            return Ok(());
        }
        let element = self.obj().clone();
        let bin: &gst::Bin = element.upcast_ref();

        let (sink_pads, src_pads) = collect_request_pads(bin)?;
        if sink_pads.is_empty() || src_pads.is_empty() {
            bail!("need at least one sink_%u and one src_%u pad");
        }

        let mut sink_snapshots = Vec::new();
        for (idx, pad) in sink_pads.iter().enumerate() {
            let typed: NmosAudioChannelMapSinkPad = pad.clone().downcast().map_err(|_| {
                anyhow::anyhow!("pad `{}` is not a channel map sink pad", pad.name())
            })?;
            let state = typed.imp().snapshot();
            let negotiated = resolve_pad_channels(pad, state.channels)?;
            sink_snapshots.push(SinkPadSnapshot {
                receiver_name: state.receiver_name,
                input_id: state.input_id,
                label: state.label,
                description: state.description,
                negotiated_channels: negotiated,
            });
            if negotiated == 0 {
                bail!("sink pad `{}` has zero channels", pad.name());
            }
            let _ = idx;
        }

        let mut src_snapshots = Vec::new();
        for pad in &src_pads {
            let typed: NmosAudioChannelMapSrcPad = pad.clone().downcast().map_err(|_| {
                anyhow::anyhow!("pad `{}` is not a channel map src pad", pad.name())
            })?;
            let state = typed.imp().snapshot();
            let negotiated = resolve_pad_channels(pad, state.channels)?;
            src_snapshots.push(SrcPadSnapshot {
                sender_name: state.sender_name,
                output_id: state.output_id,
                label: state.label,
                description: state.description,
                negotiated_channels: negotiated,
                active_map: state.active_map,
            });
            if negotiated == 0 {
                bail!("src pad `{}` has zero channels", pad.name());
            }
        }

        let settings = self.settings.lock().unwrap().clone();
        let mut session_guard = self.session.lock().unwrap();
        let session = session_guard
            .as_mut()
            .context("no open channel mapping session")?;

        if session.channelmapping_handle().is_none() {
            let req = build_add_channel_mapping_request(
                &session.session_handle,
                &settings.channelmapping_name,
                &sink_snapshots,
                &src_snapshots,
                settings.restrict_routable_inputs,
            );
            let resp =
                block_on_async(session.add_channel_mapping(req)).context("AddChannelMapping")?;
            for (snap, id) in sink_snapshots.iter_mut().zip(resp.input_ids.iter()) {
                if snap.input_id.is_empty() {
                    snap.input_id.clone_from(id);
                }
            }
            for (snap, id) in src_snapshots.iter_mut().zip(resp.output_ids.iter()) {
                if snap.output_id.is_empty() {
                    snap.output_id.clone_from(id);
                }
            }
        }

        gst::info!(
            CAT,
            "channel mapping pad ids resolved: inputs [{}], outputs [{}]",
            sink_pads
                .iter()
                .zip(sink_snapshots.iter())
                .map(|(pad, s)| format!("{}={}", pad.name(), s.input_id))
                .collect::<Vec<_>>()
                .join(", "),
            src_pads
                .iter()
                .zip(src_snapshots.iter())
                .map(|(pad, s)| format!("{}={}", pad.name(), s.output_id))
                .collect::<Vec<_>>()
                .join(", "),
        );

        let input_ids: Vec<String> = sink_snapshots.iter().map(|s| s.input_id.clone()).collect();
        let output_ids: Vec<String> = src_snapshots.iter().map(|s| s.output_id.clone()).collect();
        let topology = FrozenTopology::from_snapshots(
            &sink_snapshots,
            &src_snapshots,
            &input_ids,
            &output_ids,
        );

        let src_count = src_snapshots.len();
        let mut output_routes = Vec::new();
        let mut sync_entries = Vec::new();
        for (src_idx, src) in src_snapshots.iter().enumerate() {
            let entries =
                active_map_entries_for_src(&topology, src, src_idx, &sink_snapshots, src_count)?;
            let routes = if let Some(structure) = src.active_map.as_ref() {
                parse_active_map_structure(structure)?
            } else {
                default_identity_routes(
                    &topology,
                    src_idx,
                    src.negotiated_channels,
                    &sink_snapshots,
                    src_count,
                )
            };
            output_routes.push(routes);
            sync_entries.push((src.output_id.clone(), entries));
        }

        let graph = InternalGraph::build(bin, &topology, &sink_pads, &src_pads, &output_routes)?;

        for pad in sink_pads.iter().chain(src_pads.iter()) {
            if let Some(sink) = pad.downcast_ref::<NmosAudioChannelMapSinkPad>() {
                sink.imp().freeze();
            } else if let Some(src) = pad.downcast_ref::<NmosAudioChannelMapSrcPad>() {
                src.imp().freeze();
            }
        }
        *self.fixated.lock().unwrap() = true;
        *self.topology.lock().unwrap() = Some(topology);
        *self.internal.lock().unwrap() = Some(graph);

        drop(session_guard);
        let mut session_guard = self.session.lock().unwrap();
        let session = session_guard.as_mut().unwrap();
        for (output_id, entries) in sync_entries {
            block_on_async(session.sync_channel_mapping_state(&output_id, entries))
                .with_context(|| format!("SyncChannelMappingState for output `{output_id}`"))?;
        }

        Ok(())
    }
}

fn collect_request_pads(bin: &gst::Bin) -> Result<(Vec<gst::Pad>, Vec<gst::Pad>), anyhow::Error> {
    let mut sinks = Vec::new();
    let mut srcs = Vec::new();
    for pad in bin.pads() {
        let name = pad.name();
        if name.starts_with("sink_") {
            sinks.push(pad);
        } else if name.starts_with("src_") {
            srcs.push(pad);
        }
    }
    sinks.sort_by_key(|p| pad_index(p.name()));
    srcs.sort_by_key(|p| pad_index(p.name()));
    Ok((sinks, srcs))
}

fn pad_index(name: glib::GString) -> u32 {
    name.rsplit('_')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn channels_from_structure(structure: &gst::StructureRef) -> Option<u32> {
    if !structure.has_name("audio/x-raw") {
        return None;
    }
    if let Ok(channels) = structure.get::<i32>("channels") {
        return (channels > 0).then_some(channels as u32);
    }
    if let Ok(range) = structure.get::<gst::IntRange<i32>>("channels") {
        let min = range.min();
        let max = range.max();
        if min == max && min > 0 {
            return Some(min as u32);
        }
    }
    None
}

fn channels_from_caps(caps: &gst::Caps) -> Option<u32> {
    for i in 0..caps.size() {
        let structure = caps.structure(i)?;
        if let Some(channels) = channels_from_structure(structure) {
            return Some(channels);
        }
    }
    None
}

/// Channel count from caps negotiated on this pad's link (pad/peer intersection).
fn channels_from_link(pad: &gst::Pad) -> Option<u32> {
    let peer = pad.peer()?;
    let ours = pad
        .current_caps()
        .or_else(|| pad.allowed_caps())
        .unwrap_or_else(|| pad.query_caps(None));
    let theirs = peer
        .current_caps()
        .or_else(|| peer.allowed_caps())
        .unwrap_or_else(|| pad.peer_query_caps(None));
    channels_from_caps(&ours.intersect(&theirs))
}

fn resolve_pad_channels(pad: &gst::Pad, declared: u32) -> Result<u32, anyhow::Error> {
    // Fixed current_caps reflect the active link format; otherwise pad/peer intersection.
    let negotiated = pad
        .current_caps()
        .filter(|c| !c.is_empty())
        .and_then(|c| channels_from_caps(&c))
        .or_else(|| channels_from_link(pad));

    if let Some(negotiated) = negotiated {
        if declared > 0 && declared != negotiated {
            bail!(
                "pad `{}`: declared channels={declared} but negotiated caps have {negotiated}",
                pad.name()
            );
        }
        return Ok(if declared > 0 { declared } else { negotiated });
    }

    if declared > 0 {
        return Ok(declared);
    }

    let caps_hint = pad
        .current_caps()
        .or_else(|| pad.allowed_caps())
        .and_then(|c| c.structure(0).map(|s| s.to_string()))
        .unwrap_or_else(|| "none".into());
    bail!(
        "pad `{}`: link peers with fixed channel count in audio/x-raw caps, or set `channels` \
         before READY→PAUSED (caps={caps_hint})",
        pad.name()
    );
}

fn block_on_async<T, E>(future: impl std::future::Future<Output = Result<T, E>>) -> Result<T, E> {
    crate::runtime::SHARED_RUNTIME.block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::init_gst;

    #[test]
    fn channels_from_structure_accepts_fixed_int_and_singleton_range() {
        init_gst();

        let fixed = gst::Structure::builder("audio/x-raw")
            .field("channels", 2i32)
            .build();
        assert_eq!(channels_from_structure(fixed.as_ref()), Some(2));

        let wide = gst::Structure::builder("audio/x-raw")
            .field("channels", gst::IntRange::new(1, 8))
            .build();
        assert_eq!(channels_from_structure(wide.as_ref()), None);
    }
}
