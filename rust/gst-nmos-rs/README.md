<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# gst-nmos-rs Usage Guide

GStreamer plugin (`nmos`) providing the `nmossrc`, `nmossink`, and
`nmosaudiochannelmap` elements. They talk to the `nvnmosd` NMOS daemon over
gRPC. See
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

## Configuration Choices

Activation policy and the source of the configuring transport file are
independent choices.

### Activation Policy

| Policy | Setting | Intended use |
| --- | --- | --- |
| Controller-managed | Leave `auto-activate=false` (the default) | Production systems where an IS-05 Controller decides when the data plane becomes active |
| Self-starting | Set `auto-activate=true` | Development and fixed pipelines that should begin processing without a Controller |

### Configuring Transport File Source

| Source | Set initially | Intended use |
| --- | --- | --- |
| Supplied file | `transport-file-path` or `transport-file` | Use an existing SDP or MXL flow definition with [NvNmos extensions](https://nvidia.github.io/nvnmos/transport-files.html#nvnmos-extensions-to-the-transport-file) |
| Synthesised from properties | `caps` plus the relevant RTP/UDP endpoints or MXL identifiers | Build the configuring SDP or MXL flow definition from element configuration |
| Upstream caps (`nmossink` only) | Omit `caps` and `transport-file*` | Defer synthesis until upstream caps arrive during preroll |

`transport` defaults to `udp`. Set `transport=mxl`, `udp2`, or `nvdsudp`
explicitly when selecting another transport implementation.

`transport-file` and `transport-file-path` are mutually exclusive. Explicit
element properties override corresponding values in a supplied transport file;
essence `caps` are cross-checked rather than substituted silently. See
[Property Interaction With Transport Files](#property-interaction-with-transport-files)
for the complete rules.

## Property Groups

Use the plugin and element reference or `gst-inspect-1.0` for exact property
details:

- [`nmossink`](https://nvidia.github.io/nvnmos/gstreamer/nmos/nmossink.html)
- [`nmossrc`](https://nvidia.github.io/nvnmos/gstreamer/nmos/nmossrc.html)
- [`nmosaudiochannelmap`](https://nvidia.github.io/nvnmos/gstreamer/nmos/nmosaudiochannelmap.html)

Those references list properties alphabetically. Use these groups to decide
which properties matter for a task:

| Group | Properties | Purpose |
| --- | --- | --- |
| Essential | `node-seed`, `sender-name` / `receiver-name`, `transport`, `caps` or `transport-file*`, `receiver-caps-mode`, `auto-activate` | Identify the Node and Sender or Receiver, choose the data plane, describe the essence and Receiver capabilities, and choose Controller-managed or self-starting activation |
| RTP/UDP | Sender: `source-ip`, `source-port`, `destination-ip`, `destination-port`; Receiver: `source-ip`, `interface-ip`, `multicast-ip`, `destination-port`; both: `transport-caps`, `format-bit-rate`, `transport-bit-rate` | Configure SDP and IS-05 endpoint values for `udp`, `udp2`, and `nvdsudp`; the bit-rate properties apply to JPEG XS on `udp` / `udp2` |
| MXL | `mxl-domain-path`, `mxl-domain-id`, `mxl-flow-id` | Select the local MXL Domain and flow for `transport=mxl` |
| Human-readable metadata | `label`, `description`, `group-hint` | Set human-readable labels, descriptions, and grouping metadata for NMOS resources |
| Node and session | `daemon-uri`, `http-port`, `host-name`, `domain`, `registration-url`, `system-url` | Connect to `nvnmosd` and configure the NMOS Node; Node properties are taken from the first session that creates a shared `node-seed` |
| Inner-element overrides | `transport-properties`, `pay-properties`, `depay-properties` | Pass advanced properties to generated inner elements; payloader and depayloader overrides apply only to `udp` / `udp2` |

The Sender and Receiver meanings of `source-ip` and `destination-port` differ.
For a Sender, `source-ip` is the local egress address and
`destination-port` is remote. For a Receiver, `source-ip` is an optional remote
source-specific multicast filter and `destination-port` is the local listen port.

### Supported Caps Essence Shapes

When `transport-file*` is unset, `caps` drives synthesis of the configuring
transport file:

- `transport=mxl` produces an MXL flow definition and also requires
  `mxl-flow-id`.
- `transport=udp`, `udp2`, or `nvdsudp` produces SDP and uses the relevant
  endpoint properties.

On `nmossrc`, `receiver-caps-mode` controls whether the synthesised
configuration advertises constrained BCP-004-01 Receiver Caps. With
`transport=mxl`, the `caps` media type also selects the corresponding
`mxlsrc` video, audio, or data flow.

| Media | Caps shape | Transports | Notes |
| --- | --- | --- | --- |
| Video (raw) | `video/x-raw,format=…,width=…,height=…,framerate=…[,interlace-mode=…]` | all | MXL: `v210`. RTP/UDP: RFC 4175 8-bit `UYVY` and 10-bit `UYVP`. |
| Video (JPEG XS) | `image/x-jxsc,…` or `video/x-jxsv,…` | `udp` / `udp2` only | `width`, `height`, and `framerate` are required. Bit rates use `format-bit-rate` and `transport-bit-rate`, not caps fields. |
| Audio | `audio/x-raw,format=…,rate=…,channels=…` | all | MXL: `F32LE`. RTP/UDP: ST 2110-30 `S24BE` (L24) and `S16BE` (L16). |
| Data (ANC) | `meta/x-st-2038,framerate=…` | all | `framerate` is required; add it with a capsfilter if necessary. |

### Property Interaction With Transport Files

When a `transport-file` (literal or path) and an overlapping property
are both set, the resulting transport file handed to the daemon is
built with these rules:

| Group         | Properties | Rule when both set |
| ------------- | ---------- | ------------------ |
| Identity | `sender-name` / `receiver-name`, `mxl-flow-id`, `mxl-domain-id` | **Property overrides file.** The element rewrites the file's matching field/tag before the daemon sees it. |
| Human-readable metadata | `label`, `description`, `group-hint` | **Property overrides file.** The element rewrites the file's matching field/tag before the daemon sees it. |
| Receiver capabilities | `receiver-caps-mode` | **Property overrides file.** The element rewrites the file's Receiver Caps marker before the daemon sees it. |
| Essence shape | `caps`, `transport-caps` | **Cross-check.** Property must agree with the file's shape (today: `caps` first structure name vs `format`). Mismatch is a hard error at NULL→READY. |
| Bit rates | `format-bit-rate`, `transport-bit-rate` | **Cross-check when both declare a rate; splice when only the property is set.** Values are kilobits per second (matching NMOS `bit_rate`, SDP `b=AS:`, and fmtp `x-nvnmos-*-bit-rate` per AMWA BCP-006-01 / RFC 9134 / ST 2110-22). When the supplied SDP omits bit rates, non-zero properties are written into the configuring SDP before the daemon sees it. |
| Activation gate | `auto-activate` | Does not appear in the transport file. Controls whether the data plane starts when configuration is resolved or waits for an IS-05 activation. Independent of the configuration source. |
| No interaction | `daemon-uri`, `node-seed`, `http-port`, `host-name`, `domain`, `registration-url`, `system-url`, `transport`, `mxl-domain-path`, `transport-properties`, `pay-properties`, `depay-properties` | These don't appear in the transport file at all. Node-identity properties (`host-name`, `domain`, `registration-url`, `system-url`, `http-port`) are forwarded to `OpenSession` as `node_config` and honoured only when that session creates the Node (first opener for a given `node-seed`). `transport-properties` / `pay-properties` / `depay-properties` tune the inner GStreamer elements at chain-build time instead. |

`mxl-domain-id` is in the override group for the file tag, but is
still **cross-checked** against `<mxl-domain-path>/domain_def.json`
because that file describes which Domain identity belongs to this
local mount — a different ID would be a host-level misconfiguration,
not a labelling choice.

At IS-05 activation time the daemon's transport file is authoritative
for the override groups (an IS-05 PATCH legitimately replaces the
configured-at-startup flow id); the essence-shape cross-check
still applies, so an activation that asks an `nmossrc` configured for
v210 video to receive an audio flow is ack-failed.

## Lifecycle, Activation, and Property Changes

| Transition or event | User-visible effect |
| --- | --- |
| NULL→READY | Connect to `nvnmosd` and add the NMOS Sender or Receiver when its configuring transport file can be resolved |
| READY→PAUSED | For a deferred `nmossink`, derive configuration from upstream caps and add the Sender |
| IS-05 activation | Build or replace the inner transport elements using the active transport file |
| READY→NULL | Remove the Sender or Receiver and close the daemon session |

When neither `transport-file*` nor `caps` is set, `nmossink` defers
configuration until it can query upstream caps at READY→PAUSED. `nmossrc`
has no deferred mode; set `caps` or `transport-file*` before READY.

The element separates NMOS resource visibility from an active data plane. With
the default `auto-activate=false`, the Sender or Receiver is visible but waits
for an IS-05 activation before starting its data plane. With
`auto-activate=true`, it starts the data plane as soon as configuration is
available. This property does not change the GStreamer pipeline state.

Set configuration properties while the element is in NULL unless the element
reference marks them as changeable in READY. A property that can be set in
READY is not necessarily applied immediately: the element reads it at the next
relevant lifecycle action. In particular, `mxl-domain-path`,
`transport-properties`, `pay-properties`, and `depay-properties` apply when the
inner data plane is next built.

## Audio Channel Mapping

`nmosaudiochannelmap` exposes AMWA IS-08 routing as an audio matrix in the
pipeline. Request one `sink_%u` pad for each input stream and one `src_%u` pad
for each output stream. Every sink pad becomes an IS-08 Input; every source pad
becomes an IS-08 Output.

Request and configure every pad before the element reaches READY. Channel
counts come from negotiated audio caps by default, or from the pad's `channels`
property when it is set. The important pad properties are:

- Sink pads: `input-id`, `receiver-name`, `label`, and `description`.
- Source pads: `output-id`, `sender-name`, `label`, `description`, and the
  optional initial `active-map`.

`receiver-name` associates an Input with the corresponding NMOS Receiver.
`sender-name` associates an Output with the Source belonging to the
corresponding NMOS Sender. The pad `label` is published as IS-08
`/properties/name`; it is not the caller-chosen Sender or Receiver name.

All elements that use the same `node-seed` contribute to one NMOS Node and one
shared IS-08 Channel Mapping API. This includes multiple
`nmosaudiochannelmap` elements. Each element's required
`channelmapping-name` identifies the subset of Inputs and Outputs that it owns;
it does not create a separate IS-08 API. The name must therefore be unique
within the Node.

By default an Output may advertise unrestricted routable Inputs. Set
`restrict-routable-inputs=true` to limit each Output to the Inputs owned by
that element. The element starts with an identity map where its channel
geometry permits, then applies routing changes requested by an IS-08
Controller.

See the
[`nmosaudiochannelmap` reference](https://nvidia.github.io/nvnmos/gstreamer/nmos/nmosaudiochannelmap.html)
for all element and pad properties and a complete pipeline example.

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
  the [supported essence shapes](#supported-caps-essence-shapes).
- **Node does not appear in the Registry:** check `domain`, `registration-url`,
  and the DNS-SD service used by the host.
- **MXL or DeepStream transport setup fails:** check the MXL environment in the
  [workspace quick start](https://github.com/NVIDIA/nvnmos/blob/main/rust/README.md)
  or the [DeepStream/Rivermax prerequisites](#transportnvdsudp-deepstream-rivermax).

## Interactive Demo

For an end-to-end demo — three NMOS Nodes (producer, consumer,
processor) with an interactive menu for IS-05 enable / disable /
rewire — run
[`scripts/gst-nmos-rs-demo.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/gst-nmos-rs-demo.sh).
Video essence is matched across transports at **1080p25 10-bit**
(`v210` on MXL, `UYVP` on UDP). Pick the transport family with `DEMO_TRANSPORT`:

```sh
# MXL shared-memory (default)
./scripts/gst-nmos-rs-demo.sh

# ST 2110 over RTP/UDP (gst-plugins-good)
DEMO_TRANSPORT=udp ./scripts/gst-nmos-rs-demo.sh

# ST 2110 over RTP/UDP (prefer gst-plugins-rs udpsrc2 / *pay2 / *depay2)
DEMO_TRANSPORT=udp2 ./scripts/gst-nmos-rs-demo.sh
```

On WSL with WSLg, if `autoaudiosink` is silent, export the WSLg Pulse
socket before launching: `export PULSE_SERVER=unix:/mnt/wslg/PulseServer`

On WSL or headless hosts, there is also the option to use `fakesink`:

```sh
DEMO_AUDIO_SINK=fakesink DEMO_VIDEO_SINK=fakesink DEMO_TRANSPORT=udp ./scripts/gst-nmos-rs-demo.sh
```

The script builds `nvnmosd` + the plugin, spawns the daemon and several
gst-launch pipelines, then drops into a menu that PATCHes the
IS-05 endpoints so you can exercise activation paths against a
live pipeline.

### Three-Node Pipeline Diagrams

Each Node runs its own `gst-launch-1.0` process (Node 3 uses two:
one for video, one for audio). The diagrams below were exported from
a running demo via the interactive menu.

**Node 1 — producer** (audiotestsrc + videotestsrc → two `nmossink` senders):
video sender is enabled, audio sender is disabled

![Node 1 producer pipeline](https://raw.githubusercontent.com/NVIDIA/nvnmos/main/rust/gst-nmos-rs/images/producer.png)

**Node 2 — consumer** (two `nmossrc` receivers → queues → sinks):
video receiver is enabled, audio receiver is disabled

![Node 2 consumer pipeline](https://raw.githubusercontent.com/NVIDIA/nvnmos/main/rust/gst-nmos-rs/images/consumer.png)

**Node 3 — processor** (receive flows, process, re-transmit):

Video (`nmossrc` → `videoflip` → `nmossink`): receiver and sender enabled

![Node 3 video processor pipeline](https://raw.githubusercontent.com/NVIDIA/nvnmos/main/rust/gst-nmos-rs/images/processor-video.png)

Audio (`nmossrc` → `volume` → `nmossink`): receiver and sender disabled

![Node 3 audio processor pipeline](https://raw.githubusercontent.com/NVIDIA/nvnmos/main/rust/gst-nmos-rs/images/processor-audio.png)

## Pipeline Examples

Copy-paste `gst-launch-1.0` recipes are in
[`pipeline-examples.md`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/pipeline-examples.md).

Static scripts (no interactive menu):
[`scripts/example-pipelines/`](https://github.com/NVIDIA/nvnmos/tree/main/rust/gst-nmos-rs/scripts/example-pipelines).

## `transport=nvdsudp` (DeepStream Rivermax)

Uses a bare inner element — no external `rtp*pay` / `rtp*depay`:

- `nmossink` → `nvdsudpsink` (Mode 3: essence frames in, built-in packetization)
- `nmossrc` → `nvdsudpsrc` (Mode 3: built-in ST 2110-20/30/40 depacketization)

The element auto-calculates `payload-size`, `packets-per-line` (video), and
`payload-multiple` (audio) from essence caps. Override inner `nvdsudpsink` /
`nvdsudpsrc` properties via `transport-properties` when needed (any number of
fields in one structure, e.g. `properties,gpu-id=0,sync=false`).

SDP synthesis from `caps` emits `TP=2110TPN` (narrow traffic profile). Use
`video/x-raw(memory:NVMM),…` in `caps` for GPU Direct; set `gpu-id` via
`transport-properties`.

**Prerequisites:** Install [DeepStream 9.0](https://docs.nvidia.com/metropolis/deepstream/dev-guide/text/DS_Installation.html)
and the [Rivermax SDK](https://developer.nvidia.com/networking/rivermax)
following their respective installation guides. You also need a ConnectX-5 or
newer NIC for real network traffic, and `CAP_NET_RAW` on the host binary
(`sudo setcap CAP_NET_RAW=ep $(which gst-launch-1.0)`).

The DeepStream deb ships `nvdsudpsrc` / `nvdsudpsink` as
`libnvdsgst_udp.so` under
`/opt/nvidia/deepstream/deepstream-9.0/lib/gst-plugins`. Add that directory
to `GST_PLUGIN_PATH` and `/opt/nvidia/deepstream/deepstream-9.0/lib` to
`LD_LIBRARY_PATH` if plugins fail to load (the DeepStream installation guide
covers the usual setup).

**ST 2022-7 (dual-leg):** supported on `transport=nvdsudp` when configuring
SDP has two same-essence `m=` lines (separate destination addresses).
Inactive legs (`rtp_enabled: false` → `a=inactive`) are gated
at activation; `nvdsudpsrc` uses comma-separated `st2022-7-streams`,
`local-iface-ip`, and `source-address`. Dual-leg transport files on
`udp` / `udp2` are rejected. Caps-only synthesis still emits one `m=`.
See
[`doc/designs/gst-nmos-rs-st2022-7-dual-leg-plan.md`](https://github.com/NVIDIA/nvnmos/blob/main/doc/designs/gst-nmos-rs-st2022-7-dual-leg-plan.md).

**Not yet supported on `nvdsudp`:** JPEG XS (`image/x-jxsc` / `video/x-jxsv`) —
available on `udp` / `udp2` only.

Design notes:
[`doc/designs/gst-nmos-rs-nvdsudp-plan.md`](https://github.com/NVIDIA/nvnmos/blob/main/doc/designs/gst-nmos-rs-nvdsudp-plan.md).

## Integration Testing

See the
[integration testing guide](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/tests/README.md)
for end-to-end sync test setup and commands.
