<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Usage Guide

GStreamer plugin (`nmos`) providing the `nmossrc`, `nmossink`, and
`nmosaudiochannelmap` elements. They talk to the `nvnmosd` NMOS daemon over
gRPC. See the
[Daemon design record](https://github.com/NVIDIA/nvnmos/blob/main/doc/designs/nvnmosd/README.md)
and the
[Rust workspace quick start](https://github.com/NVIDIA/nvnmos/blob/main/rust/README.md).

The [Core NvNmos Concepts](https://nvidia.github.io/nvnmos/concepts.html)
guide explains the transport file, activation direction, and identity model
shared by the GStreamer elements, C API, and daemon.

## Quick Start

The [workspace quick start](https://github.com/NVIDIA/nvnmos/blob/main/rust/README.md#quick-start)
covers prerequisites and builds `libnvnmos`, `nvnmosd`, and this plugin. It
then runs an RTP/UDP Sender and Receiver in three terminals:

```sh
# Terminal 1, from rust/
cargo run --bin nvnmosd

# Terminal 2, from rust/gst-nmos-rs/
./scripts/example-pipelines/1080p25-sender-udp.sh

# Terminal 3, from rust/gst-nmos-rs/
./scripts/example-pipelines/1080p25-receiver-udp.sh
```

The example uses `transport=udp`, so its transport path uses
`gst-plugins-good`, and `auto-activate=true`, so video starts without an NMOS
Controller. The full quick start shows the required `NVNMOS_LIB_DIR`,
`LD_LIBRARY_PATH`, and `GST_PLUGIN_PATH` settings.

## Building

```sh
cd /path/to/nvnmos/rust
cargo build -p gst-nmos-rs
```

Build output is `target/debug/libgstnmos.so` (or `target/release/...`).

## Loading the Plugin

```sh
export GST_PLUGIN_PATH=/path/to/nvnmos/rust/target/debug
gst-inspect-1.0 nmos
```

`gst-inspect-1.0 nmos` prints the plugin metadata;
`gst-inspect-1.0 nmossink`, `gst-inspect-1.0 nmossrc`, and
`gst-inspect-1.0 nmosaudiochannelmap` list the exact property reference.

## Pipeline Examples

The [pipeline examples](https://nvidia.github.io/nvnmos/gstreamer/pipeline-examples.html)
provide copy-paste
`gst-launch-1.0` recipes for MXL, RTP/UDP, and DeepStream Rivermax, plus an
interactive three-Node IS-05 demo. Static scripts are under
[`scripts/example-pipelines/`](https://github.com/NVIDIA/nvnmos/tree/main/rust/gst-nmos-rs/scripts/example-pipelines).

## Troubleshooting

- **Plugin not found:** confirm `libgstnmos.so` is on `GST_PLUGIN_PATH` and
  `libnvnmos.so` is on `LD_LIBRARY_PATH`, then run `gst-inspect-1.0 nmos`.
- **Cannot connect to the daemon:** start `nvnmosd` and check that `daemon-uri`
  names the same Unix socket.
- **Node has no Sender:** an `nmossink` without `caps` or `transport-file*`
  waits until READY→PAUSED to query upstream caps. Move the pipeline to
  PAUSED or PLAYING and ensure upstream offers fixed, supported caps.
- **Node has no Receiver:** `nmossrc` does not defer configuration; set `caps`
  or `transport-file*` before READY.
- **Sender or Receiver is visible but media is not flowing:** either activate
  it through IS-05 or set `auto-activate=true` for a self-starting pipeline.
- **Caps negotiation fails:** compare the configured or negotiated caps with
  the [supported essence shapes](https://nvidia.github.io/nvnmos/gstreamer/configuration.html#supported-caps-essence-shapes).
- **Node does not appear in the Registry:** check `domain`, `registration-url`,
  and the DNS-SD service used by the host.
- **MXL or DeepStream transport setup fails:** check the MXL environment in the
  [workspace quick start](https://github.com/NVIDIA/nvnmos/blob/main/rust/README.md)
  or the
  [DeepStream/Rivermax prerequisites](https://nvidia.github.io/nvnmos/gstreamer/pipeline-examples.html#deepstream-rivermax).

## Further Guidance

- [Configuration](https://nvidia.github.io/nvnmos/gstreamer/configuration.html)
  — activation policy, transport-file sources, property groups, caps, and
  lifecycle timing.
- [Audio Channel Mapping](https://nvidia.github.io/nvnmos/gstreamer/audio-channel-mapping.html)
  — configure `nmosaudiochannelmap` for IS-08 routing.
- [Pipeline Examples](https://nvidia.github.io/nvnmos/gstreamer/pipeline-examples.html)
  — complete transport-specific recipes and the interactive demo.
- [Element reference](https://nvidia.github.io/nvnmos/gstreamer/nmos/) — exact
  element and pad properties.
- [Integration testing](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/tests/README.md)
  — end-to-end sync-test setup and commands.
