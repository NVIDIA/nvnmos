// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! NMOS audio channel mapping element (`nmosaudiochannelmap`).

/**
 * SECTION:element-nmosaudiochannelmap
 * @see_also: nmossrc, nmossink
 *
 * `nmosaudiochannelmap` exposes AMWA IS-08 audio channel mapping as a
 * GStreamer element. Request `sink_%u` and `src_%u` audio pads and it
 * creates matching IS-08 Inputs and Outputs on the Node; a controller then
 * decides which input channels feed which output channels and the element
 * re-orders the audio channels accordingly.
 *
 * Element and pad properties are listed alphabetically below. The
 * [Audio Channel Mapping Guide](https://nvidia.github.io/nvnmos/gstreamer/audio-channel-mapping.html)
 * explains how channel mappings contribute to the Node's shared IS-08 API.
 *
 * ## Example
 *
 * Two-input, two-output matrix from the interactive demo
 * (`scripts/gst-nmos-rs-demo.sh`, Node 3 audio processor). Each `sink_%u` /
 * `src_%u` pad becomes an IS-08 Input or Output; channel counts come from the
 * negotiated caps on that branch. Request every pad before the element reaches
 * READY.
 *
 * |[
 * gst-launch-1.0 -e \
 *   nmosaudiochannelmap name=map \
 *     node-seed=demo-node3 \
 *     channelmapping-name=demo-map \
 *     sink_0::input-id=input0 \
 *     sink_0::receiver-name=audio-in0 \
 *     sink_0::channels=2 \
 *     sink_1::input-id=input1 \
 *     sink_1::receiver-name=audio-in1 \
 *     sink_1::channels=8 \
 *     src_0::output-id=output0 \
 *     src_0::sender-name=audio-out0 \
 *     src_0::channels=2 \
 *     src_1::output-id=output1 \
 *     src_1::sender-name=audio-out1 \
 *     src_1::channels=8 \
 *   nmossrc \
 *     transport=mxl \
 *     node-seed=demo-node3 \
 *     receiver-name=audio-in0 \
 *     mxl-domain-path=/dev/shm/gst-nmos-rs-examples \
 *     caps="audio/x-raw,format=F32LE,rate=48000,channels=2" ! map.sink_0 \
 *   nmossrc \
 *     transport=mxl \
 *     node-seed=demo-node3 \
 *     receiver-name=audio-in1 \
 *     receiver-caps-mode=unconstrained \
 *     mxl-domain-path=/dev/shm/gst-nmos-rs-examples \
 *     caps="audio/x-raw,format=F32LE,rate=48000,channels=8" ! map.sink_1 \
 *   map.src_0 ! volume volume=0.3 ! nmossink \
 *     transport=mxl \
 *     node-seed=demo-node3 \
 *     sender-name=audio-out0 \
 *     mxl-domain-path=/dev/shm/gst-nmos-rs-examples \
 *     caps="audio/x-raw,format=F32LE,rate=48000,channels=2" \
 *   map.src_1 ! volume volume=0.1 ! nmossink \
 *     transport=mxl \
 *     node-seed=demo-node3 \
 *     sender-name=audio-out1 \
 *     mxl-domain-path=/dev/shm/gst-nmos-rs-examples \
 *     caps="audio/x-raw,format=F32LE,rate=48000,channels=8"
 * ]|
 *
 * An IS-08 controller PATCHes `/map/active` to re-route channels at runtime;
 * until then the element applies an identity map wherever the geometry allows.
 *
 * ## Daemon connection
 *
 * This element is a front end for the `nvnmosd` NMOS daemon and holds no NMOS
 * state of its own. At NULL→READY it opens a gRPC session to the daemon
 * (`daemon-uri`) and closes it again at READY→NULL, so `nvnmosd` must already
 * be running when the pipeline starts. The daemon serves the AMWA IS-04, IS-05
 * and IS-08 HTTP APIs; the element only drives the session and its inner
 * GStreamer data path.
 *
 * ## Node properties
 *
 * Sessions that share a `node-seed` contribute to the same NMOS Node, so one
 * host can present many Senders, Receivers and channel maps under a single
 * Node simply by reusing the seed. The Node identity properties — `host-name`,
 * `domain`, `registration-url`, `system-url` and `http-port` — are taken from
 * whichever session first creates the Node and ignored by later sessions that
 * attach to it.
 *
 * ## Inputs and outputs
 *
 * Request a `sink_%u` pad for each audio stream coming in and a `src_%u` pad
 * for each stream going out; every pad becomes an IS-08 Input or Output whose
 * channel count is taken from its negotiated caps. The set of pads is frozen
 * when the element first reaches READY, so request every pad you need before
 * starting the pipeline. `channelmapping-name` names the Input/Output bundle on
 * the Node, and `restrict-routable-inputs` limits each Output to this element's
 * own Inputs.
 *
 * ## Activation
 *
 * The element starts with an identity map wherever the channel geometry allows
 * it; an IS-08 controller then PATCHes the active map to re-route channels at
 * runtime and the element applies the new routing to the audio passing through
 * it.
 */
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

mod caps;
mod imp;
mod internals;
mod pad;

glib::wrapper! {
    pub struct NmosAudioChannelMap(ObjectSubclass<imp::NmosAudioChannelMap>)
        @extends gst::Bin, gst::Element, gst::Object,
        @implements gst::ChildProxy;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    pad::register_types()?;
    gst::Element::register(
        Some(plugin),
        "nmosaudiochannelmap",
        gst::Rank::NONE,
        NmosAudioChannelMap::static_type(),
    )
}
