<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Pipeline Examples

Copy-paste `gst-launch-1.0` recipes for `nmossrc` / `nmossink`, with matched
essence across `transport=mxl`, `transport={udp,udp2}`, and `transport=nvdsudp`.

Static scripts and the interactive demo share defaults from
[`scripts/env.sh`](scripts/env.sh) (flow IDs, RTP/UDP multicast, caps).
The demo — [`scripts/gst-nmos-rs-demo.sh`](scripts/gst-nmos-rs-demo.sh) —
sources that file and overrides demo-only knobs (domain path, node seeds).

Property reference and activation semantics: [`README.md`](README.md).

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

**Daemon** (terminal 1 — or use the demo script, which starts its own):

```sh
/path/to/nvnmos/rust/target/debug/nvnmosd --uds /tmp/nvnmosd.sock
```

Pipelines below use `daemon-uri=$DEMO_DAEMON_URI` (default
`unix:/tmp/nvnmosd.sock`) unless noted.

**MXL domain** — example scripts call `bootstrap_mxl_domain` (from
[`env.sh`](scripts/env.sh)), which creates `domain_def.json` on first use
or reuses the `id` already stored under `DEMO_MXL_DOMAIN_PATH`. Override the
path or export `DEMO_MXL_DOMAIN_ID` before the first run; to start fresh,
remove the domain directory.

```sh
export DEMO_MXL_DOMAIN_PATH=/dev/shm/gst-nmos-rs-examples
# optional on first run: export DEMO_MXL_DOMAIN_ID=$(uuidgen)
```

The example scripts read `DEMO_MXL_*` flow IDs and `DEMO_UDP_*` multicast
destinations from the environment (see [`scripts/env.sh`](scripts/env.sh)).
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

[`env.sh`](scripts/env.sh) exports user-tunable values with the `DEMO_`
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
| Audio | `DEMO_MXL_AUDIO_CAPS` — 2 ch `F32LE` | `DEMO_UDP_AUDIO_CAPS` — 2 ch `S24BE` |
| Video (alt) | `DEMO_MXL_VIDEO_CAPS_ALT` — 1080p29.97 `v210` | `DEMO_UDP_VIDEO_CAPS_ALT` — 1080p29.97 `UYVP` |
| Audio (alt) | `DEMO_MXL_AUDIO_CAPS_ALT` — 8 ch `F32LE` | `DEMO_UDP_AUDIO_CAPS_ALT` — 8 ch `S24BE`; `DEMO_UDP_AUDIO_TRANSPORT_CAPS_ALT` — `ptime=0.125` ms |

Primary caps match the table in [`env.sh`](scripts/env.sh).
Alternate caps are used by demo **Node 4** for wide-receiver caps
renegotiation tests (not covered by the static example scripts).

## Scenarios

| Scenario | MXL script | UDP script (`udp` / `udp2` / `nvdsudp`) | Demo script node |
|----------|------------|-------------------------------------------|------------------|
| 1080p25 sender | [`1080p25-sender-mxl.sh`](scripts/example-pipelines/1080p25-sender-mxl.sh) | [`1080p25-sender-udp.sh`](scripts/example-pipelines/1080p25-sender-udp.sh) | Node 1 video |
| 1080p25 receiver | [`1080p25-receiver-mxl.sh`](scripts/example-pipelines/1080p25-receiver-mxl.sh) | [`1080p25-receiver-udp.sh`](scripts/example-pipelines/1080p25-receiver-udp.sh) | Node 2 video |
| Flip / process | [`1080p25-flipper-mxl.sh`](scripts/example-pipelines/1080p25-flipper-mxl.sh) | [`1080p25-flipper-udp.sh`](scripts/example-pipelines/1080p25-flipper-udp.sh) | Node 3 video |
| Deferred sender | [`1080p25-deferred-sender-mxl.sh`](scripts/example-pipelines/1080p25-deferred-sender-mxl.sh) | [`1080p25-deferred-sender-udp.sh`](scripts/example-pipelines/1080p25-deferred-sender-udp.sh) | — |
| Multi-flow sender | [`multi-flow-sender-mxl.sh`](scripts/example-pipelines/multi-flow-sender-mxl.sh) | [`multi-flow-sender-udp.sh`](scripts/example-pipelines/multi-flow-sender-udp.sh) | Node 1 (video + audio) |
| Full lab + IS-05 | — | — | [`gst-nmos-rs-demo.sh`](scripts/gst-nmos-rs-demo.sh) |

### 1080p25 Sender

[`1080p25-sender-mxl.sh`](scripts/example-pipelines/1080p25-sender-mxl.sh) ·
[`1080p25-sender-udp.sh`](scripts/example-pipelines/1080p25-sender-udp.sh)

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

[`1080p25-receiver-mxl.sh`](scripts/example-pipelines/1080p25-receiver-mxl.sh) ·
[`1080p25-receiver-udp.sh`](scripts/example-pipelines/1080p25-receiver-udp.sh)

Start a matching sender first (same flow identity / multicast group).

**MXL:**

```sh
./scripts/example-pipelines/1080p25-receiver-mxl.sh
```

**UDP:**

```sh
./scripts/example-pipelines/1080p25-receiver-udp.sh
```

Use `DEMO_VIDEO_SINK=fakesink` on headless hosts (element name only;
scripts add `videoconvert` upstream). Set `DEMO_NIC_IP` to your
high-bandwidth network interface.

### Flipper (Receive → Process → Re-Transmit)

[`1080p25-flipper-mxl.sh`](scripts/example-pipelines/1080p25-flipper-mxl.sh) ·
[`1080p25-flipper-udp.sh`](scripts/example-pipelines/1080p25-flipper-udp.sh)

One `node-seed`, two resources: `nmossrc` consumes the inbound flow,
`nmossink` publishes the processed flow (`DEMO_MXL_VIDEO_FLOW_ID3` / outbound
multicast group).

For MXL, start [`1080p25-sender-mxl.sh`](scripts/example-pipelines/1080p25-sender-mxl.sh)
in another terminal first (inbound flow `DEMO_MXL_VIDEO_FLOW_ID1`). For UDP, start
[`1080p25-sender-udp.sh`](scripts/example-pipelines/1080p25-sender-udp.sh)
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

[`1080p25-deferred-sender-mxl.sh`](scripts/example-pipelines/1080p25-deferred-sender-mxl.sh) ·
[`1080p25-deferred-sender-udp.sh`](scripts/example-pipelines/1080p25-deferred-sender-udp.sh)

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

### Multi-Flow (Video + Audio on One Node)

[`multi-flow-sender-mxl.sh`](scripts/example-pipelines/multi-flow-sender-mxl.sh) ·
[`multi-flow-sender-udp.sh`](scripts/example-pipelines/multi-flow-sender-udp.sh)

Two `nmossink` elements sharing `node-seed`, distinct
`sender-name` / flow identity:

```sh
./scripts/example-pipelines/multi-flow-sender-mxl.sh
# or
./scripts/example-pipelines/multi-flow-sender-udp.sh
```

The rigorous **video + ANC** (`meta/x-st-2038`) integration test with
real buffer validation is [`tests/multi_flow_video_data.rs`](tests/multi_flow_video_data.rs)
(`#[ignore]` — needs full MXL toolchain). Opt-in:

```sh
cargo test -p gst-nmos-rs --test multi_flow_video_data -- --ignored --test-threads=1
```

## `transport=nvdsudp` (DeepStream Rivermax)

Same IS-05 endpoint properties and `DEMO_UDP_VIDEO_CAPS` essence as `udp` /
`udp2`; inner chain is `nvdsudpsrc` / `nvdsudpsink` (no `buffer-size` — use
`transport-properties` for `gpu-id`, thread cores, etc.).
Prerequisites: Install [DeepStream 9.0](https://docs.nvidia.com/metropolis/deepstream/dev-guide/text/DS_Installation.html)
and [Rivermax](https://developer.nvidia.com/networking/rivermax) — see
[`README.md`](README.md#transportnvdsudp-deepstream-rivermax).

```sh
export DEMO_NIC_IP=203.0.113.1
DEMO_UDP_TRANSPORT=nvdsudp ./scripts/example-pipelines/1080p25-sender-udp.sh
```

Interactive demo:

```sh
export DEMO_NIC_IP=203.0.113.1
DEMO_TRANSPORT=nvdsudp ./scripts/gst-nmos-rs-demo.sh
```

## Interactive Demo

[`gst-nmos-rs-demo.sh`](scripts/gst-nmos-rs-demo.sh)

```sh
# MXL (default)
./scripts/gst-nmos-rs-demo.sh

# UDP — same topology, matched 1080p25 essence
export DEMO_NIC_IP=203.0.113.1   # your high-bandwidth network interface
DEMO_TRANSPORT=udp ./scripts/gst-nmos-rs-demo.sh
```

WSL with WSLg audio: `export PULSE_SERVER=unix:/mnt/wslg/PulseServer` before launch.
Or for headless mode: `DEMO_AUDIO_SINK=fakesink DEMO_VIDEO_SINK=fakesink DEMO_TRANSPORT=udp ./scripts/gst-nmos-rs-demo.sh`
