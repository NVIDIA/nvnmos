---
short-description: NMOS Sender/Receiver and audio channel mapping GStreamer plugin
...

<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# nmos

NMOS Sender, Receiver and IS-08 audio channel-mapping elements that connect
GStreamer pipelines to an NMOS Node hosted by the `nvnmosd` daemon. The elements
create AMWA IS-04 resources and follow IS-05 / IS-08 activations, so
controllers manage connections while the pipeline carries ST 2110 (RTP/UDP) or
MXL essence.

See [Core NvNmos Concepts](https://nvidia.github.io/nvnmos/concepts.html)
for the transport file, activation direction, and identity model shared by all
NvNmos integration layers.

## Prerequisites

`nmossrc` and `nmossink` instantiate an inner data path at runtime. The
`transport` you choose determines which plugins must be installed and on the
GStreamer registry when media starts — gst-mxl-rs (`mxlsrc` / `mxlsink` and
`libmxl.so`) for `transport=mxl`, gst-plugins-good and/or gst-plugins-rs for
ST 2110 RTP/UDP (`transport=udp` or `udp2`), or the DeepStream plugin for
`transport=nvdsudp` (Rivermax). See the Usage Guide for building, loading
`libgstnmos.so` on `GST_PLUGIN_PATH`, and transport-specific setup.
