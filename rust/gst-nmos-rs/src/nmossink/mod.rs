// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! NMOS Sender (`nmossink`) element.

/**
 * SECTION:element-nmossink
 * @see_also: nmossrc, nmosaudiochannelmap, mxlsink, udpsink, nvdsudpsink
 *
 * `nmossink` is an NMOS Sender. It creates an IS-04 Sender on an NMOS Node
 * hosted by `nvnmosd` and transmits the video, audio or ST 2038 ANC essence
 * arriving on its sink pad over the configured transport.
 *
 * Properties are listed alphabetically below. The
 * [Configuration Guide](https://nvidia.github.io/nvnmos/gstreamer/configuration.html#property-groups)
 * groups them by task and explains the common configuration choices.
 *
 * ## Examples
 *
 * These pipelines assume `nvnmosd` is listening on `unix:/tmp/nvnmosd.sock`.
 * With the default `auto-activate=false` the Sender appears on IS-04/IS-05 and
 * an IS-05 controller PATCHes it to start media.
 *
 * Caps-driven MXL sender (`scripts/example-pipelines/minimal-prop-sender-mxl.sh`):
 *
 * |[
 * gst-launch-1.0 -e \
 *   videotestsrc pattern=smpte is-live=true ! \
 *   video/x-raw,format=v210,width=1920,height=1080,framerate=25/1,interlace-mode=progressive ! \
 *   nmossink \
 *     transport=mxl \
 *     node-seed=example-minimal-producer \
 *     sender-name=video1 \
 *     mxl-domain-id=92c696c2-66f9-4d86-8a87-13135d847189 \
 *     mxl-domain-path=/dev/shm/gst-nmos-rs-examples \
 *     caps="video/x-raw,format=v210,width=1920,height=1080,framerate=25/1,interlace-mode=progressive" \
 *     label="minimal 1080p25 v210 sender"
 * ]|
 *
 * Caps-driven RTP/UDP sender (`scripts/example-pipelines/minimal-prop-sender-udp.sh`):
 *
 * |[
 * gst-launch-1.0 -e \
 *   videotestsrc pattern=smpte is-live=true ! \
 *   video/x-raw,format=UYVP,width=1920,height=1080,framerate=25/1,interlace-mode=progressive ! \
 *   nmossink \
 *     transport=udp \
 *     node-seed=example-minimal-producer \
 *     sender-name=video1 \
 *     source-ip=192.0.2.10 \
 *     caps="video/x-raw,format=UYVP,width=1920,height=1080,framerate=25/1,interlace-mode=progressive" \
 *     label="minimal 1080p25 UYVP sender"
 * ]|
 *
 * Replace `source-ip` with the IP address of the local NIC to transmit from.
 *
 * Deferred sender: omit `caps` and the element synthesises the configuring
 * transport file from upstream caps at pre-roll — often `videotestsrc ! nmossink`
 * with only `transport`, `node-seed`, and `sender-name` is enough.
 *
 * Transport-file variant: write an MXL `flow_def` or SDP to disk and pass
 * `transport-file-path=/path/to/file` instead of `caps` and the identity
 * properties above (`scripts/example-pipelines/minimal-file-sender-mxl.sh`,
 * `minimal-file-sender-udp.sh`).
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
 * ## Transport
 *
 * `transport` selects the inner data path — `mxl` (MXL shared memory), `udp`
 * or `udp2` (ST 2110 over RTP/UDP), or `nvdsudp` (ST 2110 via DeepStream and
 * Rivermax). The element builds and manages the matching inner elements for
 * you, so you never add `mxlsink`, `udpsink` and the RTP payloaders to the
 * pipeline yourself. MXL additionally needs `mxl-domain-path`; the RTP
 * transports use the IS-05 endpoint properties.
 *
 * Inner elements are created when the data path starts and must already be
 * registered: `mxl` uses `mxlsink` from the gst-mxl-rs plugin (and `libmxl.so`
 * on the dynamic loader path); `udp` uses `udpsink` plus an essence-specific
 * `rtp*pay` from gst-plugins-good; `udp2` prefers matching `*pay2` elements
 * from gst-plugins-rs where available, falling back to gst-plugins-good per
 * element (`udpsink` is always from gst-plugins-good); `nvdsudp` uses
 * `nvdsudpsink` from the DeepStream plugin (built-in payloading). The exact RTP
 * payloader is chosen from the configuring transport file and caps. See the
 * `transport` property for the full family list; plugin loading and
 * `GST_PLUGIN_PATH` setup are in the Usage Guide.
 *
 * ## Configuring the Sender
 *
 * Before it can be added to the Node the Sender needs a *configuring transport
 * file* — an SDP for the RTP transports or an MXL `flow_def`. Provide it in one
 * of two ways:
 *
 * * **From caps.** Set `caps` to the essence you are sending and the element
 *   synthesises the file, together with the transport identity properties
 *   (`mxl-flow-id` on MXL; the IS-05 endpoint properties such as
 *   `destination-ip` on RTP/UDP).
 * * **From a transport file.** Set `transport-file-path` (or, for programmatic
 *   callers, `transport-file`) to a ready-made SDP or `flow_def`.
 *
 * If neither is set the Sender is configured lazily: the element reads the
 * upstream caps as the pipeline pre-rolls and synthesises the file from those,
 * so a plain `… ! nmossink` often works with no `caps` property at all.
 * `sender-name` names the Sender on the Node; `label` and `description` are
 * optional and override the file.
 *
 * ## Activation
 *
 * Appearing on the Node (visible to IS-04/IS-05 controllers) is separate from
 * the data path going live. By default (`auto-activate=false`) the Sender is
 * created but stays idle until an IS-05 controller activates it with a
 * PATCH. Set `auto-activate=true` to bring the data path up immediately from
 * the configured transport file and have the daemon reflect that in the IS-05
 * API — a convenient shortcut for development and controller-less setups.
 */
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

mod imp;

glib::wrapper! {
    pub struct NmosSink(ObjectSubclass<imp::NmosSink>) @extends gst::Bin, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "nmossink",
        gst::Rank::NONE,
        NmosSink::static_type(),
    )
}
