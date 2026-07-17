// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! NMOS Receiver (`nmossrc`) element.

/**
 * SECTION:element-nmossrc
 * @see_also: nmossink, nmosaudiochannelmap, mxlsrc, udpsrc, nvdsudpsrc
 *
 * `nmossrc` is an NMOS Receiver. It creates an IS-04 Receiver on an NMOS
 * Node hosted by `nvnmosd` and, once the Receiver is activated, produces the
 * received video, audio or ST 2038 ANC essence on its source pad as ordinary
 * GStreamer buffers.
 *
 * ## Examples
 *
 * These pipelines assume `nvnmosd` is listening on `unix:/tmp/nvnmosd.sock`.
 * With the default `auto-activate=false` the Receiver appears on IS-04/IS-05 and
 * an IS-05 controller PATCHes it to supply the subscription identity and start
 * media.
 *
 * Caps-driven MXL receiver (`scripts/example-pipelines/minimal-prop-receiver-mxl.sh`):
 *
 * |[
 * gst-launch-1.0 -e \
 *   nmossrc \
 *     transport=mxl \
 *     node-seed=example-minimal-consumer \
 *     receiver-name=video1 \
 *     mxl-domain-id=92c696c2-66f9-4d86-8a87-13135d847189 \
 *     mxl-domain-path=/dev/shm/gst-nmos-rs-examples \
 *     caps="video/x-raw,format=v210,width=1920,height=1080,framerate=25/1,interlace-mode=progressive" \
 *     label="minimal 1080p25 v210 receiver" ! \
 *   queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 ! \
 *   videoconvert ! autovideosink sync=false
 * ]|
 *
 * Caps-driven RTP/UDP receiver (`scripts/example-pipelines/minimal-prop-receiver-udp.sh`):
 *
 * |[
 * gst-launch-1.0 -e \
 *   nmossrc \
 *     transport=udp \
 *     node-seed=example-minimal-consumer \
 *     receiver-name=video1 \
 *     interface-ip=192.0.2.10 \
 *     caps="video/x-raw,format=UYVP,width=1920,height=1080,framerate=25/1,interlace-mode=progressive" \
 *     label="minimal 1080p25 UYVP receiver" ! \
 *   queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 ! \
 *   videoconvert ! autovideosink sync=false
 * ]|
 *
 * Replace `interface-ip` with the IP address of the local NIC that should
 * join the multicast group.
 *
 * Transport-file variant: write an MXL `flow_def` or SDP to disk and pass
 * `transport-file-path=/path/to/file` instead of `caps` and the identity
 * properties above (`scripts/example-pipelines/minimal-file-receiver-mxl.sh`,
 * `minimal-file-receiver-udp.sh`).
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
 * you, so you never add `mxlsrc`, `udpsrc` and the RTP depayloaders to the
 * pipeline yourself. MXL additionally needs `mxl-domain-path`; the RTP
 * transports use the IS-05 endpoint properties.
 *
 * Inner elements are created when the data path starts and must already be
 * registered: `mxl` uses `mxlsrc` from the gst-mxl-rs plugin (and `libmxl.so`
 * on the dynamic loader path); `udp` uses `udpsrc` plus an essence-specific
 * `rtp*depay` from gst-plugins-good; `udp2` prefers `udpsrc2` and matching
 * `*depay2` elements from gst-plugins-rs where available, falling back to
 * gst-plugins-good per element; `nvdsudp` uses `nvdsudpsrc` from the DeepStream
 * plugin (built-in depayloading). The exact RTP depayloader is chosen from the
 * configuring transport file and caps. See the `transport` property for the
 * full family list; plugin loading and `GST_PLUGIN_PATH` setup are in the
 * Usage Guide.
 *
 * ## Configuring the Receiver
 *
 * Before it can be added to the Node the Receiver needs a *configuring
 * transport file* — an SDP for the RTP transports or an MXL `flow_def` — which
 * also determines whether IS-04 advertises BCP-004-01 Receiver Caps. Provide
 * it in one of two ways:
 *
 * * **From caps.** Set `caps` to the essence you want to receive and the
 *   element synthesises the file; `receiver-caps-mode` chooses whether the
 *   Receiver is advertised as constrained (these specific caps) or unconstrained (accepts
 *   any compatible stream).
 * * **From a transport file.** Set `transport-file-path` (or, for programmatic
 *   callers, `transport-file`) to a ready-made SDP or `flow_def`.
 *
 * With neither set the Receiver is added without a format and waits for an
 * IS-05 activation to supply one. `receiver-name` names the Receiver on the
 * Node; `label` and `description` are optional and override the file.
 *
 * ## Activation
 *
 * Appearing on the Node (visible to IS-04/IS-05 controllers) is separate from
 * the data path going live. By default (`auto-activate=false`) the Receiver is
 * created but stays idle until an IS-05 controller activates it with a
 * PATCH. Set `auto-activate=true` to bring the data path up immediately from
 * the configuring transport file and have the daemon reflect that in the IS-05
 * API — a convenient shortcut for development and controller-less setups.
 */
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

mod imp;

glib::wrapper! {
    pub struct NmosSrc(ObjectSubclass<imp::NmosSrc>) @extends gst::Bin, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "nmossrc",
        gst::Rank::NONE,
        NmosSrc::static_type(),
    )
}
