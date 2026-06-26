// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Internal `audiomixer` + `audiomixmatrix` subgraph.

use anyhow::{bail, Context};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::channel_mapping::active_map::ActiveMapRoute;
use crate::channel_mapping::matrix::{
    build_input_bus_mix_matrix, build_output_mix_matrix, input_bus_converter_config,
    matrix_to_audiomixmatrix_gvalue,
};
use crate::channel_mapping::types::FrozenTopology;
use super::caps::{caps_with_channel_count, caps_with_channel_mask, sequential_channel_mask};
use super::pad::{NmosAudioChannelMapSinkPad, NmosAudioChannelMapSrcPad};

// Extra wait the live audiomixer allows for jittery inputs before emitting.
// Covers software-receive scheduling jitter at small RTP packet times.
const AUDIOMIXER_LATENCY_NS: u64 = 10_000_000; // 10 ms

// Pre-mixer / pre-mixmatrix buffering to absorb jitter. The default queue
// max size is 200 buffers, which with a ptime of 0.125 ms (6 samples at 48 kHz)
// is only 25 ms, so the buffer/byte limits must also be disabled.
const QUEUE_MAX_SIZE_TIME_NS: u64 = 50_000_000; // 50 ms

pub(crate) struct InternalGraph {
    // Fields are ordered to follow the data flow through the graph:
    // sink_capsfilters -> sink_queues -> mixer -> mixer_capsfilter -> tee
    // -> src_queues -> mixmatrices.
    sink_capsfilters: Vec<gst::Element>,
    sink_queues: Vec<gst::Element>,
    mixer: gst::Element,
    mixer_capsfilter: gst::Element,
    tee: gst::Element,
    src_queues: Vec<gst::Element>,
    mixmatrices: Vec<gst::Element>,
}

impl InternalGraph {
    pub(crate) fn build(
        bin: &gst::Bin,
        topology: &FrozenTopology,
        sink_pads: &[gst::Pad],
        src_pads: &[gst::Pad],
        output_routes: &[Vec<ActiveMapRoute>],
    ) -> Result<Self, anyhow::Error> {
        if sink_pads.is_empty() || src_pads.is_empty() {
            bail!("at least one sink and one src pad are required");
        }
        if output_routes.len() != src_pads.len() {
            bail!("output_routes length must match src pad count");
        }

        let mixer = gst::ElementFactory::make("audiomixer")
            .name("mixer")
            .build()
            .context("creating audiomixer")?;
        if mixer.has_property("ignore-inactive-pads") {
            mixer.set_property("ignore-inactive-pads", true);
        }
        // Live nmossrc fake chains block in appsrc::create() until IS-05 activation.
        // Default start-time-selection=zero makes the mixer EOS when every sink pad
        // is still inactive (even with ignore-inactive-pads=true).
        if mixer.has_property("start-time-selection") {
            mixer.set_property_from_str("start-time-selection", "first");
        }
        if mixer.has_property("latency") {
            mixer.set_property("latency", AUDIOMIXER_LATENCY_NS);
        }

        let tee = gst::ElementFactory::make("tee")
            .name("mixer-tee")
            .build()
            .context("creating tee")?;

        // Pin the mixer output bus to its channel count so the downstream
        // per-output audiomixmatrix in-channels dimension matches. The
        // channel-mask is not required to negotiate, but a sequential mask
        // gives the bus well-defined channel positions and avoids
        // "invalid channel positions" warnings in the log.
        let mixer_capsfilter = gst::ElementFactory::make("capsfilter")
            .name("mixer-capsfilter")
            .property("caps", caps_with_channel_mask(topology.input_bus_channels))
            .build()
            .context("creating mixer capsfilter")?;

        bin.add_many([&mixer, &mixer_capsfilter, &tee])?;

        let mixer_src = mixer
            .static_pad("src")
            .ok_or_else(|| anyhow::anyhow!("audiomixer has no src pad"))?;
        mixer_src.link(&mixer_capsfilter.static_pad("sink").unwrap())?;
        mixer_capsfilter.link(&tee)?;

        let mut sink_capsfilters = Vec::new();
        let mut sink_queues = Vec::new();
        for (idx, sink_pad) in sink_pads.iter().enumerate() {
            let mixer_sink = mixer
                .request_pad_simple("sink_%u")
                .ok_or_else(|| anyhow::anyhow!("audiomixer request sink pad failed"))?;
            let input_ch = topology.input_channels[idx];
            let bus_offset = topology.input_bus_offsets[idx];
            let matrix = build_input_bus_mix_matrix(
                topology.input_bus_channels,
                input_ch,
                bus_offset,
            );
            mixer_sink.set_property("converter-config", input_bus_converter_config(&matrix));

            // Pin each input leg to exactly its channel count: the converter-config
            // mix-matrix (set above) is built for `input_ch -> bus slot`, so the leg
            // must negotiate `input_ch` channels for the matrix dimensions to match.
            // Count only, no channel-mask — the matrix remaps by index, so channel
            // positions on the input are irrelevant and we leave the mask open for
            // upstream.
            let sink_capsfilter = gst::ElementFactory::make("capsfilter")
                .name(format!("sink-capsfilter-{idx}"))
                .property("caps", caps_with_channel_count(input_ch))
                .build()
                .context("creating sink capsfilter")?;
            // A per-leg queue decouples each input from the live mixer's
            // aggregation thread so RTP jitter at small packet times does not
            // starve it (see QUEUE_MAX_SIZE_TIME_NS).
            let sink_queue = gst::ElementFactory::make("queue")
                .name(format!("sink-queue-{idx}"))
                .property("max-size-time", QUEUE_MAX_SIZE_TIME_NS)
                .property("max-size-buffers", 0u32)
                .property("max-size-bytes", 0u32)
                .build()
                .context("creating sink queue")?;
            bin.add_many([&sink_capsfilter, &sink_queue])?;
            sink_capsfilter
                .link(&sink_queue)
                .context("linking sink capsfilter to sink queue")?;
            sink_queue
                .static_pad("src")
                .unwrap()
                .link(&mixer_sink)
                .context("linking sink queue to audiomixer")?;

            let ghost: NmosAudioChannelMapSinkPad = sink_pad
                .clone()
                .downcast()
                .map_err(|_| anyhow::anyhow!("pad `{}` is not a sink ghost pad", sink_pad.name()))?;
            ghost.set_target(sink_capsfilter.static_pad("sink").as_ref())?;
            ghost.set_active(true)?;
            sink_capsfilters.push(sink_capsfilter);
            sink_queues.push(sink_queue);
        }

        let mut mixmatrices = Vec::new();
        let mut src_queues = Vec::new();
        for (src_idx, src_pad) in src_pads.iter().enumerate() {
            let src_queue = gst::ElementFactory::make("queue")
                .name(format!("src-queue-{src_idx}"))
                .property("max-size-time", QUEUE_MAX_SIZE_TIME_NS)
                .property("max-size-buffers", 0u32)
                .property("max-size-bytes", 0u32)
                .build()?;
            let output_ch = topology.output_channels[src_idx];
            let mixmatrix = gst::ElementFactory::make("audiomixmatrix")
                .name(format!("mixmatrix-{src_idx}"))
                // Default mode is `manual` (0); omit — Rust bindings reject raw gint here.
                .property("in-channels", topology.input_bus_channels)
                .property("out-channels", output_ch)
                .build()
                .context("creating audiomixmatrix")?;
            // Give >2-channel outputs a sequential channel-mask so the matrix src
            // caps carry defined positions (keeps "invalid channel positions"
            // warnings out of the log). Mono/stereo are left at the property
            // default (0 = unpositioned): their positions are implied, and a
            // sequential mask for 1 channel would wrongly read as front-left
            // rather than mono. (-1 is not usable here: this audiomixmatrix emits
            // it verbatim as an all-bits mask rather than the canonical layout.)
            if output_ch > 2 && mixmatrix.has_property("channel-mask") {
                if let Some(mask) = sequential_channel_mask(output_ch) {
                    mixmatrix.set_property("channel-mask", mask.0);
                }
            }

            let routes = &output_routes[src_idx];
            let mix = build_output_mix_matrix(topology, output_ch, routes)?;
            mixmatrix.set_property("matrix", matrix_to_audiomixmatrix_gvalue(&mix));

            bin.add_many([&src_queue, &mixmatrix])?;
            tee.link_pads(Some("src_%u"), &src_queue, Some("sink"))?;
            src_queue.link(&mixmatrix)?;

            let matrix_src = mixmatrix
                .static_pad("src")
                .ok_or_else(|| anyhow::anyhow!("audiomixmatrix has no src pad"))?;
            let ghost: NmosAudioChannelMapSrcPad = src_pad
                .clone()
                .downcast()
                .map_err(|_| anyhow::anyhow!("pad `{}` is not a src ghost pad", src_pad.name()))?;
            ghost.set_target(Some(&matrix_src))?;
            ghost.set_active(true)?;

            src_queues.push(src_queue);
            mixmatrices.push(mixmatrix);
        }

        Ok(Self {
            sink_capsfilters,
            sink_queues,
            mixer,
            mixer_capsfilter,
            tee,
            src_queues,
            mixmatrices,
        })
    }

    pub(crate) fn set_output_matrix(
        &self,
        topology: &FrozenTopology,
        src_index: usize,
        routes: &[ActiveMapRoute],
    ) -> Result<(), anyhow::Error> {
        let mixmatrix = self
            .mixmatrices
            .get(src_index)
            .ok_or_else(|| anyhow::anyhow!("no matrix for src index {src_index}"))?;
        let output_channels = topology
            .output_channels
            .get(src_index)
            .copied()
            .unwrap_or(0);
        let mix = build_output_mix_matrix(topology, output_channels, routes)?;
        mixmatrix.set_property("matrix", matrix_to_audiomixmatrix_gvalue(&mix));
        Ok(())
    }

    pub(crate) fn sync_state_with_parent(&self) -> Result<(), gst::StateChangeError> {
        for cf in &self.sink_capsfilters {
            cf.sync_state_with_parent()
                .map_err(|_| gst::StateChangeError)?;
        }
        for q in &self.sink_queues {
            q.sync_state_with_parent()
                .map_err(|_| gst::StateChangeError)?;
        }
        self.mixer
            .sync_state_with_parent()
            .map_err(|_| gst::StateChangeError)?;
        self.mixer_capsfilter
            .sync_state_with_parent()
            .map_err(|_| gst::StateChangeError)?;
        self.tee
            .sync_state_with_parent()
            .map_err(|_| gst::StateChangeError)?;
        for q in &self.src_queues {
            q.sync_state_with_parent()
                .map_err(|_| gst::StateChangeError)?;
        }
        for m in &self.mixmatrices {
            m.sync_state_with_parent()
                .map_err(|_| gst::StateChangeError)?;
        }
        Ok(())
    }

    pub(crate) fn teardown(self, bin: &gst::Bin) {
        // Reverse of the graph data flow: downstream elements first.
        for el in self
            .mixmatrices
            .into_iter()
            .chain(self.src_queues)
            .chain([self.tee, self.mixer_capsfilter, self.mixer])
            .chain(self.sink_queues)
            .chain(self.sink_capsfilters)
        {
            let _ = el.set_state(gst::State::Null);
            let _ = bin.remove(&el);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nmosaudiochannelmap::caps::{caps_with_channel_count, caps_with_channel_mask};

    // Exercises the real InternalGraph::build() against a bin with the element's
    // ghost pads (no nvnmosd needed), so the production subgraph — sink_queues,
    // start-time-selection/latency, the bus capsfilter and per-output mixmatrices
    // — is the thing under test rather than a hand-rolled lookalike. The IS-08
    // integration test covers routing correctness; this covers "build() negotiates
    // and reaches PLAYING".
    #[test]
    fn internal_graph_build_reaches_playing() {
        use crate::channel_mapping::active_map::ActiveMapRoute;
        use crate::channel_mapping::types::FrozenTopology;
        use gst::glib;

        let _ = gst::init();
        let input_ch = 2u32;
        let topology = FrozenTopology {
            input_bus_channels: 4,
            input_channels: vec![input_ch, input_ch],
            input_bus_offsets: vec![0, 2],
            output_channels: vec![input_ch, input_ch],
            input_ids: vec!["input0".into(), "input1".into()],
            output_ids: vec!["output0".into(), "output1".into()],
        };
        let identity_routes = |input_id: &str| {
            vec![
                ActiveMapRoute {
                    output_channel: 0,
                    input_id: input_id.into(),
                    input_channel: 0,
                },
                ActiveMapRoute {
                    output_channel: 1,
                    input_id: input_id.into(),
                    input_channel: 1,
                },
            ]
        };
        let output_routes = vec![identity_routes("input0"), identity_routes("input1")];

        let bin = gst::Bin::new();
        let mut sink_pads = Vec::new();
        for i in 0..2 {
            let pad = glib::Object::builder::<NmosAudioChannelMapSinkPad>()
                .property("name", format!("sink_{i}"))
                .property("direction", gst::PadDirection::Sink)
                .build()
                .upcast::<gst::Pad>();
            bin.add_pad(&pad).unwrap();
            sink_pads.push(pad);
        }
        let mut src_pads = Vec::new();
        for i in 0..2 {
            let pad = glib::Object::builder::<NmosAudioChannelMapSrcPad>()
                .property("name", format!("src_{i}"))
                .property("direction", gst::PadDirection::Src)
                .build()
                .upcast::<gst::Pad>();
            bin.add_pad(&pad).unwrap();
            src_pads.push(pad);
        }

        InternalGraph::build(&bin, &topology, &sink_pads, &src_pads, &output_routes)
            .expect("build internal graph");

        // Drive the bin's external ghost pads exactly as a host pipeline would.
        let pipe = gst::Pipeline::new();
        pipe.add(&bin).unwrap();
        for (idx, sink) in sink_pads.iter().enumerate() {
            let tone = gst::ElementFactory::make("audiotestsrc")
                .property("freq", 440.0 * f64::from(idx as u32 + 1))
                .property("num-buffers", 48i32)
                .build()
                .unwrap();
            let cf = gst::ElementFactory::make("capsfilter")
                .property(
                    "caps",
                    gst::Caps::builder("audio/x-raw")
                        .field("format", "F32LE")
                        .field("rate", 48_000i32)
                        .field("layout", "interleaved")
                        .field("channels", input_ch as i32)
                        .build(),
                )
                .build()
                .unwrap();
            pipe.add_many([&tone, &cf]).unwrap();
            tone.link(&cf).unwrap();
            cf.static_pad("src").unwrap().link(sink).unwrap();
        }
        for src in &src_pads {
            let fakesink = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .build()
                .unwrap();
            pipe.add(&fakesink).unwrap();
            src.link(&fakesink.static_pad("sink").unwrap()).unwrap();
        }

        pipe.set_state(gst::State::Playing).unwrap();
        let (_ret, state, pending) = pipe.state(gst::ClockTime::from_seconds(5));
        assert_eq!(state, gst::State::Playing);
        assert_eq!(pending, gst::State::VoidPending);
        let _ = pipe.set_state(gst::State::Null);
    }

    // Load-bearing assumption: audiomixer's sink pads advertise the open template
    // (any channel count, no channel-mask) upstream, regardless of the channel-mask
    // pinned on the bus src. That is what keeps the bus mask off the external
    // audioconvert. Verified independent of per-sink converter-config, which affects
    // streaming-time remixing but not the sink caps query.
    #[test]
    fn audiomixer_sink_does_not_propagate_pinned_bus_src_caps() {
        let _ = gst::init();
        let pipe = gst::Pipeline::new();
        let mixer = gst::ElementFactory::make("audiomixer").build().unwrap();
        // Pin the bus src to 10ch + sequential mask, as production does.
        let bus_cf = gst::ElementFactory::make("capsfilter")
            .property("caps", caps_with_channel_mask(10))
            .build()
            .unwrap();
        let fakesink = gst::ElementFactory::make("fakesink").build().unwrap();
        pipe.add_many([&mixer, &bus_cf, &fakesink]).unwrap();
        mixer.link(&bus_cf).unwrap();
        bus_cf.link(&fakesink).unwrap();

        let msink = mixer.request_pad_simple("sink_%u").unwrap();
        let _ = pipe.set_state(gst::State::Ready);
        let unconstrained = msink.query_caps(None).to_string();
        let constrained = msink
            .query_caps(Some(&caps_with_channel_count(8)))
            .to_string();
        let _ = pipe.set_state(gst::State::Null);

        assert!(
            !unconstrained.contains("channel-mask"),
            "audiomixer.sink leaked bus mask upstream (unconstrained): {unconstrained}"
        );
        assert!(
            !unconstrained.contains("channels=(int)10"),
            "audiomixer.sink forced bus width upstream (unconstrained): {unconstrained}"
        );
        assert!(
            !constrained.contains("channel-mask"),
            "audiomixer.sink leaked bus mask upstream (channels=8 filter): {constrained}"
        );
    }
}
