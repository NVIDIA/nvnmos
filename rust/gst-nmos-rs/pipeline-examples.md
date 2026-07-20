<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Pipeline Examples

Copy-paste `gst-launch-1.0` recipes for `nmossrc` / `nmossink`, with matched
essence across `transport=mxl`, `transport={udp,udp2}`, and `transport=nvdsudp`.

Static scripts and the interactive demo share defaults from
[`scripts/env.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/env.sh) (flow IDs, RTP/UDP multicast, caps).
The demo — [`scripts/gst-nmos-rs-demo.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/gst-nmos-rs-demo.sh) —
sources that file and overrides demo-only knobs (domain path, node seeds).

See the
[configuration guide](https://nvidia.github.io/nvnmos/gstreamer/configuration.html)
for property groups and activation semantics.

## Prerequisites

**Build and load** (from the nvnmos `rust/` directory):

`cargo build` here produces `nvnmosd` and `libgstnmos.so` only. It does
**not** build the MXL runtime (`libmxl.so`) or the `gst-mxl-rs` plugin
(`libgstmxl.so`) — those live in the [mxl](https://github.com/dmf-mxl/mxl)
repo and are required only for `transport=mxl`.

```sh
# libnvnmos.so — build via the nvnmos CMake/Conan flow first (see rust/README.md)
export NVNMOS_LIB_DIR=/path/to/nvnmos/build

cd /path/to/nvnmos/rust
cargo build -p nvnmosd -p gst-nmos-rs
export GST_PLUGIN_PATH=/path/to/nvnmos/rust/target/debug
export LD_LIBRARY_PATH=$NVNMOS_LIB_DIR
```

For **`transport=mxl`**, also build MXL and extend the plugin / library paths
(sibling checkout layout assumed by the demo script):

```sh
MXL_REPO=/path/to/mxl
cargo build --manifest-path "$MXL_REPO/rust/Cargo.toml" -p gst-mxl-rs
export GST_PLUGIN_PATH=$GST_PLUGIN_PATH:$MXL_REPO/rust/target/debug
export LD_LIBRARY_PATH=$LD_LIBRARY_PATH:$MXL_REPO/build/Linux-Clang-Debug/lib
```

`transport={udp,udp2}` needs only the nvnmos build above plus system
GStreamer plugins (gst-plugins-good; gst-plugins-rs for `udp2`).

JPEG XS examples (`*-udp-jxsv.sh`) also need gst-plugins-rs `rsrtp`
(`rtpjxsvpay` / `rtpjxsvdepay`) and a JPEG XS codec
(`svtjpegxsenc` / `svtjpegxsdec` from gst-plugins-bad). JPEG XS
is not supported on `transport=nvdsudp`.

**Daemon** (terminal 1 — or use the demo script, which starts its own):

```sh
/path/to/nvnmos/rust/target/debug/nvnmosd --uds /tmp/nvnmosd.sock
```

Pipelines below use `daemon-uri=$DEMO_DAEMON_URI` (default
`unix:/tmp/nvnmosd.sock`) unless noted.

**MXL domain** — example scripts call `bootstrap_mxl_domain` (from
[`env.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/env.sh)), which creates `domain_def.json` on first use
or reuses the `id` already stored under `DEMO_MXL_DOMAIN_PATH`. Override the
path or export `DEMO_MXL_DOMAIN_ID` before the first run; to start fresh,
remove the domain directory.

```sh
export DEMO_MXL_DOMAIN_PATH=/dev/shm/gst-nmos-rs-examples
# optional on first run: export DEMO_MXL_DOMAIN_ID=$(uuidgen)
```

The example scripts read `DEMO_MXL_*` flow IDs and `DEMO_UDP_*` multicast
destinations from the environment (see [`scripts/env.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/env.sh)).
Override any `DEMO_MXL_{VIDEO,AUDIO}_FLOW_ID{1-4}` or
`DEMO_UDP_{VIDEO,AUDIO}_MCAST_{IP,PORT}{1-4}` before running.

**UDP multicast NIC** — set the interface that can carry multicast:

```sh
export DEMO_NIC_IP=203.0.113.1   # your high-bandwidth network interface
# 203.0.113.1 is from RFC 5737 TEST-NET-3 (reserved for documentation).
```

High-rate ST 2110-20 video (~1 Gbps at 1080p25 UYVP) needs a large
`udpsrc` / `udpsink` socket buffer on **`transport=udp` / `udp2`**.
All UDP video examples below set:

```text
transport-properties="properties,buffer-size=16777216"
```

(16 MiB — override with `DEMO_UDP_VIDEO_BUFFER_SIZE` in the example
scripts.) If packets are dropped, raise `net.core.rmem_max` / `wmem_max`:

```sh
sudo sysctl -w net.core.rmem_max=16777216 net.core.wmem_max=16777216
```

Pick **`transport=udp`** (gst-plugins-good), **`transport=udp2`**
(gst-plugins-rs), or **`transport=nvdsudp`** (DeepStream Rivermax).
Example UDP scripts use `DEMO_UDP_TRANSPORT` for the `transport=` property
(default `udp`).

## Environment variables

[`env.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/env.sh) exports user-tunable values with the `DEMO_`
prefix. Example scripts and the interactive demo both source this file.

| Variable | Role |
|----------|------|
| `DEMO_DAEMON_SOCK` | gRPC UDS path for `nvnmosd` (default `/tmp/nvnmosd.sock`) |
| `DEMO_DAEMON_URI` | `unix:` URI derived from `DEMO_DAEMON_SOCK` (used as `daemon-uri=`) |
| `DEMO_NIC_IP` | Local NIC for UDP IS-05 endpoint props and SDP source-filter |
| `DEMO_UDP_TRANSPORT` | `transport=` on example pipelines: `udp`, `udp2`, or `nvdsudp` |
| `DEMO_MXL_DOMAIN_ID` / `DEMO_MXL_DOMAIN_PATH` | MXL shared-memory domain |
| `DEMO_MXL_{VIDEO,AUDIO}_FLOW_ID{1-4}` | MXL flow identity (paired per index) |
| `DEMO_UDP_{VIDEO,AUDIO}_MCAST_{IP,PORT}{1-4}` | UDP multicast groups (paired per index) |
| `DEMO_{MXL,UDP}_{VIDEO,AUDIO}_CAPS` | Primary essence caps |
| `DEMO_{MXL,UDP}_{VIDEO,AUDIO}_LABEL` | Primary essence name for NMOS `label=` (keep in sync with `*_CAPS`) |
| `DEMO_{MXL,UDP}_{VIDEO,AUDIO}_CAPS_ALT` | Alternate essence (demo Node 4) |
| `DEMO_{MXL,UDP}_{VIDEO,AUDIO}_LABEL_ALT` | Alternate essence name for NMOS `label=` (keep in sync with `*_CAPS_ALT`) |
| `DEMO_UDP_VIDEO_JXSV_{CAPS,RAW_CAPS,BIT_RATE,LABEL}` | JPEG XS essence caps (`image/x-jxsc`), encoder input (`video/x-raw`), format bit rate (kbit/s), and NMOS `label=` |
| `DEMO_UDP_AUDIO_TRANSPORT_CAPS_ALT` | Alt UDP audio `transport-caps` (`a-ptime=0.125` ms; demo Node 4) |
| `DEMO_UDP_VIDEO_BUFFER_SIZE` | `udpsrc`/`udpsink` socket buffer (`udp`/`udp2` only) |
| `DEMO_VIDEO_QUEUE_MAX_BUFFERS` | Queue after video `nmossrc` on receiver / flipper paths |
| `DEMO_AUDIO_QUEUE_MAX_TIME_MS` | Queue after audio `nmossrc` on receiver / flipper paths |
| `DEMO_{AUDIO,VIDEO}_SINK` | Playback sink elements (e.g. `fakesink`, `autovideosink`; scripts add `videoconvert`/`audioconvert` upstream) |

The interactive demo adds `DEMO_TRANSPORT` (`mxl` \| `udp` \| `udp2` \|
`nvdsudp`) to select the transport family for the whole topology. Example
scripts set `transport=mxl` explicitly on MXL recipes and use
`DEMO_UDP_TRANSPORT` on UDP recipes.

## Shared Essence

By default, essence formats are aligned across transports at
**1920×1080 @ 25 fps 10-bit 4:2:2** video plus **2 ch @ 48 kHz** audio.

| Flow | MXL caps | UDP caps |
|------|----------|----------|
| Video | `DEMO_MXL_VIDEO_CAPS` — 1080p25 `v210` | `DEMO_UDP_VIDEO_CAPS` — 1080p25 `UYVP` |
| Video (JPEG XS) | — | `DEMO_UDP_VIDEO_JXSV_CAPS` — 1080p25 `image/x-jxsc` |
| Audio | `DEMO_MXL_AUDIO_CAPS` — 2 ch `F32LE` | `DEMO_UDP_AUDIO_CAPS` — 2 ch `S24BE` |
| Video (alt) | `DEMO_MXL_VIDEO_CAPS_ALT` — 1080p29.97 `v210` | `DEMO_UDP_VIDEO_CAPS_ALT` — 1080p29.97 `UYVP` |
| Audio (alt) | `DEMO_MXL_AUDIO_CAPS_ALT` — 8 ch `F32LE` | `DEMO_UDP_AUDIO_CAPS_ALT` — 8 ch `S24BE`; `DEMO_UDP_AUDIO_TRANSPORT_CAPS_ALT` — `ptime=0.125` ms |

Primary caps match the table in [`env.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/env.sh).
Alternate caps are used by demo **Node 4** for unconstrained-receiver reroute tests
(menu action 4: switch Node 2 between Node 1 and Node 4 senders).

## Scenarios

| Scenario | MXL script | UDP script (`udp` / `udp2` / `nvdsudp`) | Demo script node |
|----------|------------|-------------------------------------------|------------------|
| 1080p25 sender | [`1080p25-sender-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-sender-mxl.sh) | [`1080p25-sender-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-sender-udp.sh) · [`1080p25-sender-udp-jxsv.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-sender-udp-jxsv.sh) | Node 1 video |
| 1080p25 receiver | [`1080p25-receiver-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-receiver-mxl.sh) | [`1080p25-receiver-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-receiver-udp.sh) · [`1080p25-receiver-udp-jxsv.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-receiver-udp-jxsv.sh) | Node 2 video |
| Flip / process | [`1080p25-flipper-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-flipper-mxl.sh) | [`1080p25-flipper-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-flipper-udp.sh) | Node 3 video |
| Deferred sender | [`1080p25-deferred-sender-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-deferred-sender-mxl.sh) | [`1080p25-deferred-sender-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-deferred-sender-udp.sh) | — |
| Multi-flow sender | [`multi-flow-sender-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/multi-flow-sender-mxl.sh) | [`multi-flow-sender-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/multi-flow-sender-udp.sh) | Node 1 (video + audio) |
| Minimal sender (properties) | [`minimal-prop-sender-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-prop-sender-mxl.sh) | [`minimal-prop-sender-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-prop-sender-udp.sh) | — |
| Minimal receiver (properties) | [`minimal-prop-receiver-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-prop-receiver-mxl.sh) | [`minimal-prop-receiver-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-prop-receiver-udp.sh) | — |
| Minimal sender (transport file) | [`minimal-file-sender-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-file-sender-mxl.sh) | [`minimal-file-sender-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-file-sender-udp.sh) | — |
| Minimal receiver (transport file) | [`minimal-file-receiver-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-file-receiver-mxl.sh) | [`minimal-file-receiver-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-file-receiver-udp.sh) | — |
| Minimal sender (properties, JPEG XS) | — | [`minimal-prop-sender-udp-jxsv.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-prop-sender-udp-jxsv.sh) | — |
| Minimal receiver (properties, JPEG XS) | — | [`minimal-prop-receiver-udp-jxsv.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-prop-receiver-udp-jxsv.sh) | — |
| Minimal sender (transport file, JPEG XS) | — | [`minimal-file-sender-udp-jxsv.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-file-sender-udp-jxsv.sh) | — |
| Minimal receiver (transport file, JPEG XS) | — | [`minimal-file-receiver-udp-jxsv.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-file-receiver-udp-jxsv.sh) | — |
| Full lab + IS-05 | — | — | [`gst-nmos-rs-demo.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/gst-nmos-rs-demo.sh) |

### 1080p25 Sender

[`1080p25-sender-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-sender-mxl.sh) ·
[`1080p25-sender-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-sender-udp.sh) ·
[`1080p25-sender-udp-jxsv.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-sender-udp-jxsv.sh)

**MXL** — caps + `mxl-flow-id` synthesis, eager activation:

```sh
./scripts/example-pipelines/1080p25-sender-mxl.sh
```

Equivalent one-liner:

```sh
gst-launch-1.0 -e \
  videotestsrc pattern=smpte is-live=true ! \
  video/x-raw,format=v210,width=1920,height=1080,framerate=25/1,interlace-mode=progressive ! \
  nmossink transport=mxl daemon-uri=$DEMO_DAEMON_URI \
    node-seed=example sender-name=video1 \
    mxl-domain-id=$DEMO_MXL_DOMAIN_ID \
    mxl-domain-path=$DEMO_MXL_DOMAIN_PATH \
    mxl-flow-id=$DEMO_MXL_VIDEO_FLOW_ID1 \
    caps="video/x-raw,format=v210,width=1920,height=1080,framerate=25/1,interlace-mode=progressive" \
    auto-activate=true
```

**UDP** — caps + IS-05 endpoint props synthesise configuring SDP:

```sh
export DEMO_NIC_IP=203.0.113.1   # your high-bandwidth network interface
./scripts/example-pipelines/1080p25-sender-udp.sh
# or
./scripts/example-pipelines/1080p25-sender-udp-jxsv.sh
```

Equivalent one-liner (`transport=udp2` also works):

```sh
gst-launch-1.0 -e \
  videotestsrc pattern=smpte is-live=true ! \
  video/x-raw,format=UYVP,width=1920,height=1080,framerate=25/1,interlace-mode=progressive ! \
  nmossink transport=udp daemon-uri=$DEMO_DAEMON_URI \
    node-seed=example sender-name=video1 \
    destination-ip=$DEMO_UDP_VIDEO_MCAST_IP1 destination-port=$DEMO_UDP_VIDEO_MCAST_PORT1 source-ip=$DEMO_NIC_IP \
    transport-properties="properties,buffer-size=16777216" \
    caps="video/x-raw,format=UYVP,width=1920,height=1080,framerate=25/1,interlace-mode=progressive" \
    auto-activate=true
```

### 1080p25 Receiver

[`1080p25-receiver-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-receiver-mxl.sh) ·
[`1080p25-receiver-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-receiver-udp.sh) ·
[`1080p25-receiver-udp-jxsv.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-receiver-udp-jxsv.sh)

Start a matching sender first (same flow identity / multicast group).

**MXL:**

```sh
./scripts/example-pipelines/1080p25-receiver-mxl.sh
```

**UDP:**

```sh
./scripts/example-pipelines/1080p25-receiver-udp.sh
# or
./scripts/example-pipelines/1080p25-receiver-udp-jxsv.sh
```

Use `DEMO_VIDEO_SINK=fakesink` on headless hosts (element name only;
scripts add `videoconvert` upstream). Set `DEMO_NIC_IP` to your
high-bandwidth network interface.

### Flipper (Receive → Process → Re-Transmit)

[`1080p25-flipper-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-flipper-mxl.sh) ·
[`1080p25-flipper-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-flipper-udp.sh)

One `node-seed`, two resources: `nmossrc` consumes the inbound flow,
`nmossink` publishes the processed flow (`DEMO_MXL_VIDEO_FLOW_ID3` / outbound
multicast group).

For MXL, start [`1080p25-sender-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-sender-mxl.sh)
in another terminal first (inbound flow `DEMO_MXL_VIDEO_FLOW_ID1`). For UDP, start
[`1080p25-sender-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-sender-udp.sh)
before the flipper script.

```sh
# MXL flipper (after sender is running)
./scripts/example-pipelines/1080p25-flipper-mxl.sh

# UDP flipper (after sender is running)
./scripts/example-pipelines/1080p25-flipper-udp.sh
```

For IS-05-gated activation (resources visible on IS-04 before the data
path goes live), omit `auto-activate=true` and PATCH
`/single/{senders,receivers}/{id}/staged` — demo **Node 3** exercises
this; example scripts use `auto-activate=true` for simplicity.

### Deferred AddSender

[`1080p25-deferred-sender-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-deferred-sender-mxl.sh) ·
[`1080p25-deferred-sender-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/1080p25-deferred-sender-udp.sh)

When neither `transport-file*` nor `caps=` is set on `nmossink`, AddSender
runs at **READY→PAUSED** from upstream peer caps. Transport-specific identity
props must still be set (MXL domain + `mxl-flow-id`, or UDP destination
multicast + `source-ip`).

**MXL:**

```sh
./scripts/example-pipelines/1080p25-deferred-sender-mxl.sh
```

**UDP:**

```sh
./scripts/example-pipelines/1080p25-deferred-sender-udp.sh
```

Expected log sequence: `no resource added` at NULL→READY, then
`deferred AddSender complete` at READY→PAUSED (MXL synthesises `flow_def`
JSON; UDP synthesises configuring SDP).

### Minimal (controller-driven IS-05)

[`minimal-prop-sender-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-prop-sender-mxl.sh) ·
[`minimal-prop-sender-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-prop-sender-udp.sh) ·
[`minimal-prop-receiver-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-prop-receiver-mxl.sh) ·
[`minimal-prop-receiver-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-prop-receiver-udp.sh)

[`minimal-file-sender-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-file-sender-mxl.sh) ·
[`minimal-file-sender-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-file-sender-udp.sh) ·
[`minimal-file-receiver-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-file-receiver-mxl.sh) ·
[`minimal-file-receiver-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-file-receiver-udp.sh)

[`minimal-prop-sender-udp-jxsv.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-prop-sender-udp-jxsv.sh) ·
[`minimal-prop-receiver-udp-jxsv.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-prop-receiver-udp-jxsv.sh) ·
[`minimal-file-sender-udp-jxsv.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-file-sender-udp-jxsv.sh) ·
[`minimal-file-receiver-udp-jxsv.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/minimal-file-receiver-udp-jxsv.sh)

Smallest NULL→READY AddSender / AddReceiver paths with
`auto-activate=false`. Resources become visible through IS-04 at READY; the
controller PATCHes `/single/{senders,receivers}/{id}/staged` to bring the data
plane live.

**Properties-driven (`minimal-prop-*`)** — `sender-name` / `receiver-name`
plus `caps` (and `source-ip` / `interface-ip` for RTP/UDP, or
`mxl-domain-*` for MXL) synthesise the configuring transport at NULL→READY.

**MXL:**

```sh
./scripts/example-pipelines/minimal-prop-sender-mxl.sh
./scripts/example-pipelines/minimal-prop-receiver-mxl.sh
```

**UDP:**

```sh
./scripts/example-pipelines/minimal-prop-sender-udp.sh
./scripts/example-pipelines/minimal-prop-receiver-udp.sh
# or
./scripts/example-pipelines/minimal-prop-sender-udp-jxsv.sh
./scripts/example-pipelines/minimal-prop-receiver-udp-jxsv.sh
```

**Transport-file-driven (`minimal-file-*`)** — configuring SDP / flow_def
from [`fixtures/minimal-video.sdp.in`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/fixtures/minimal-video.sdp.in),
[`fixtures/minimal-video-jxsv.sdp.in`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/fixtures/minimal-video-jxsv.sdp.in),
and
[`fixtures/minimal-video.mxl.json.in`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/fixtures/minimal-video.mxl.json.in)
via `transport-file-path`. Resource name comes from the file
(`a=x-nvnmos-name` or `urn:x-nvnmos:tag:name`); NMOS label from SDP `s=`
or flow_def `label`. [`render_transport_fixture`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/env.sh) substitutes
`@NIC_IP@`, `@MXL_DOMAIN_ID@`, and `@LABEL@`. Upstream caps must still
match the file essence. File-driven MXL scripts need only `mxl-domain-path`
(`bootstrap_mxl_domain`); they omit `mxl-domain-id`, `caps`, and name
properties. UDP fixtures set `a=x-nvnmos-iface-ip` only — destination
addresses via IS-05 PATCH.

**MXL:**

```sh
./scripts/example-pipelines/minimal-file-sender-mxl.sh
./scripts/example-pipelines/minimal-file-receiver-mxl.sh
```

**UDP:**

```sh
./scripts/example-pipelines/minimal-file-sender-udp.sh
./scripts/example-pipelines/minimal-file-receiver-udp.sh
# or
./scripts/example-pipelines/minimal-file-sender-udp-jxsv.sh
./scripts/example-pipelines/minimal-file-receiver-udp-jxsv.sh
```

### Multi-Flow (Video + Audio on One Node)

[`multi-flow-sender-mxl.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/multi-flow-sender-mxl.sh) ·
[`multi-flow-sender-udp.sh`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/scripts/example-pipelines/multi-flow-sender-udp.sh)

Two `nmossink` elements sharing `node-seed`, distinct
`sender-name` / flow identity:

```sh
./scripts/example-pipelines/multi-flow-sender-mxl.sh
# or
./scripts/example-pipelines/multi-flow-sender-udp.sh
```

## DeepStream Rivermax

Set `transport=nvdsudp` to select the DeepStream Rivermax-based transport
plugin. The NMOS elements then use a bare inner element — no additional
`rtp*pay` / `rtp*depay`:

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
See the
[ST 2022-7 design notes](https://github.com/NVIDIA/nvnmos/blob/main/doc/designs/gst-nmos-rs-st2022-7-dual-leg-plan.md).

**Not yet supported on `nvdsudp`:** JPEG XS (`image/x-jxsc` / `video/x-jxsv`) —
available on `udp` / `udp2` only.

```sh
export DEMO_NIC_IP=203.0.113.1
DEMO_UDP_TRANSPORT=nvdsudp ./scripts/example-pipelines/1080p25-sender-udp.sh
```

Interactive demo:

```sh
export DEMO_NIC_IP=203.0.113.1
DEMO_TRANSPORT=nvdsudp ./scripts/gst-nmos-rs-demo.sh
```

See the
[DeepStream Rivermax design notes](https://github.com/NVIDIA/nvnmos/blob/main/doc/designs/gst-nmos-rs-nvdsudp-plan.md)
for implementation details.

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

![Node 1 producer pipeline](images/producer.png)

**Node 2 — consumer** (two `nmossrc` receivers → queues → sinks):
video receiver is enabled, audio receiver is disabled

![Node 2 consumer pipeline](images/consumer.png)

**Node 3 — processor** (receive flows, process, re-transmit):

Video (`nmossrc` → `videoflip` → `nmossink`): receiver and sender enabled

![Node 3 video processor pipeline](images/processor-video.png)

Audio (`nmossrc` → `volume` → `nmossink`): receiver and sender disabled

![Node 3 audio processor pipeline](images/processor-audio.png)
