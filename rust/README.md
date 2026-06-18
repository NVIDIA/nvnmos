<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# nvnmos Rust Workspace

Rust components for the NMOS daemon and GStreamer plugin family.

| Doc | Contents |
| --- | --- |
| **This file — Quick start** | Build, run `nvnmosd`, try two example GStreamer pipelines |
| [`nvnmosd/README.md`](nvnmosd/README.md) | Daemon operator reference (env vars, gRPC contract) |
| [`gst-nmos-rs/README.md`](gst-nmos-rs/README.md) | `nmossrc` / `nmossink` property reference |
| [`gst-nmos-rs/pipeline-examples.md`](gst-nmos-rs/pipeline-examples.md) | Full pipeline catalog (MXL, flipper, demo script, …) |
| [`doc/designs/nvnmosd/README.md`](../doc/designs/nvnmosd/README.md) | Architecture and design history |

## Quick Start

Three terminals on Linux: build once, start the daemon, then run a sender and
receiver. The example scripts use RTP/UDP (`transport=udp`, ST 2110-style
multicast) with `auto-activate=true`, so video starts without an NMOS
Controller or IS-05 PATCH step.

Tested on **Ubuntu 24.04** (same as CI).

### Prerequisites

**System packages** — C++ compiler, CMake, Python, libclang (`bindgen` for
`nvnmos-sys`), and GStreamer headers plus runtime plugins for the example
pipelines:

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends -y \
  build-essential cmake python3 python3-venv \
  clang \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good
```

CMake must be **3.17+**. Ubuntu 24.04's `cmake` package is sufficient; on older
distros use `pip install cmake~=3.17` inside the venv below (see
[`README.md`](../README.md#cmake)).

**Conan** — resolves and builds C++ dependencies for `libnvnmos`. Use a
Python venv at the **repository root** so Conan stays off the system Python
([`README.md`](../README.md#python-virtual-environment)):

```sh
# repository root
python3 -m venv .venv
source .venv/bin/activate
pip install --upgrade pip
pip install conan~=2.2
conan profile detect    # creates ~/.conan2/profiles/default
```

Keep the venv activated while running `conan install` in the next section.

**Rust** — install [rustup](https://rustup.rs/) if `cargo` is not on your
`PATH`. Building inside `rust/` picks up [`rust-toolchain.toml`](rust-toolchain.toml).

### Build `libnvnmos.so`

From the **repository root** (full detail in the top-level
[`README.md`](../README.md#building-the-nvnmos-library)):

```sh
conan install src \
  --settings:all build_type=Release \
  --build=missing \
  --output-folder=src/conan \
  --lockfile=src/conan.lock

cmake -B build \
  -DCMAKE_TOOLCHAIN_FILE=conan/conan_toolchain.cmake \
  -DCMAKE_BUILD_TYPE=Release \
  src

cmake --build build --parallel
```

Point the Rust build at the shared library:

```sh
export NVNMOS_LIB_DIR=$PWD/build
export LD_LIBRARY_PATH=$NVNMOS_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
```

### Build `nvnmosd` and the GStreamer plugin

```sh
cd rust
cargo build -p nvnmosd -p gst-nmos-rs
export GST_PLUGIN_PATH=$PWD/target/debug
```

### Start the daemon (terminal 1)

```sh
export NVNMOS_LIB_DIR=/path/to/nvnmos/build
export LD_LIBRARY_PATH=$NVNMOS_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}

cd rust
cargo run --bin nvnmosd
```

### Run the sender pipeline (terminal 2)

After the daemon is running, set the same library and plugin paths as above, then start the sender:

```sh
export NVNMOS_LIB_DIR=/path/to/nvnmos/build
export LD_LIBRARY_PATH=$NVNMOS_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
export GST_PLUGIN_PATH=/path/to/nvnmos/rust/target/debug

cd rust/gst-nmos-rs
# Optional: pick the NIC that carries multicast (defaults to first non-loopback).
# export DEMO_NIC_IP=203.0.113.1
./scripts/example-pipelines/1080p25-sender-udp.sh
```

This runs `videotestsrc` → `nmossink` (with `transport=udp`), registers an NMOS Sender, and publishes
1080p25 test video to the default multicast group (see [`scripts/env.sh`](gst-nmos-rs/scripts/env.sh)).

### Run the receiver pipeline (terminal 3)

Start the sender first, then (same env vars as terminal 2):

```sh
export NVNMOS_LIB_DIR=/path/to/nvnmos/build
export LD_LIBRARY_PATH=$NVNMOS_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
export GST_PLUGIN_PATH=/path/to/nvnmos/rust/target/debug

cd rust/gst-nmos-rs
./scripts/example-pipelines/1080p25-receiver-udp.sh
```

This runs `nmossrc` (`transport=udp`) → `videoconvert` → `autovideosink`, subscribing to the
same multicast group. You should see SMPTE color bars when both pipelines are playing.

**Troubleshooting:** example scripts call `require_nvnmosd` and exit if
`/tmp/nvnmosd.sock` is not listening (default daemon socket; override with
`--uds` or `NVNMOSD_UDS` — see [`nvnmosd/README.md`](nvnmosd/README.md)).
Use `RUST_LOG=info` on the daemon for verbose logs. Headless hosts: set
`DEMO_VIDEO_SINK=fakesink` before the receiver script.

**Next steps:** The interactive lab with IS-05 menu actions is
[`gst-nmos-rs/scripts/gst-nmos-rs-demo.sh`](gst-nmos-rs/scripts/gst-nmos-rs-demo.sh).
For MXL (`transport=mxl`), build the [MXL SDK](https://github.com/dmf-mxl/mxl)
first, then see [`pipeline-examples.md`](gst-nmos-rs/pipeline-examples.md) for
example pipelines. For compliant ST 2110 (`transport=nvdsudp`)
using DeepStream and Rivermax SDK, see
[`gst-nmos-rs/README.md`](gst-nmos-rs/README.md#transportnvdsudp-deepstream-rivermax).

## Crates

| Crate              | Kind        | Purpose                                                              |
| ------------------ | ----------- | -------------------------------------------------------------------- |
| `nvnmos-sys`       | library     | `bindgen`-generated FFI to the C `libnvnmos` API in `../src/nvnmos.h`. |
| `nvnmos`           | library     | Safe Rust wrapper over `nvnmos-sys`: RAII `NodeServer`, `Result` errors, deterministic id accessors. |
| `nvnmos-rpc`       | library     | gRPC protocol crate (`nvnmosd.proto` + `tonic`-generated stubs).     |
| `nvnmosd`          | binary      | NMOS daemon — see [`nvnmosd/README.md`](nvnmosd/README.md).         |
| `nvnmosd-example`  | binary      | Example/regression client modelled on the C `nvnmos-example`.        |
| `nvnmosd-bench`    | binary      | Scale smoke / benchmark client for `nvnmosd`.                        |
| `gst-nmos-rs`      | GStreamer plugin (cdylib) | `nmos` plugin — `nmossrc` / `nmossink` elements. See [`gst-nmos-rs/README.md`](gst-nmos-rs/README.md). |

## Container Image

For a combined image with `nvnmosd` + `gst-nmos-rs` + plugins for `transport=mxl` and
`transport=udp`/`udp2`, suitable for `gst-launch-1.0` in Kubernetes or Docker, see
[`docker/gst-nmos-rs/README.md`](../docker/gst-nmos-rs/README.md).

## Building

The workspace **MSRV** is **Rust 1.85** (`rust-version` in [`Cargo.toml`](Cargo.toml)). Development, CI, and container builds pin **1.92** in [`rust-toolchain.toml`](rust-toolchain.toml) — the minimum needed for gst-plugins-rs `0.15.2` in [`docker/gst-nmos-rs`](../docker/gst-nmos-rs/).

`nvnmos-sys` links against a pre-built `libnvnmos.so`. The Rust crate does **not** build the C library itself; use the CMake/Conan flow under `../src/` ([Quick start — Build `libnvnmos.so`](#build-libnvnmosso)), then:

```sh
export NVNMOS_LIB_DIR=/absolute/path/to/build     # contains libnvnmos.so
cargo build --workspace
```

`NVNMOS_LIB_DIR` is optional when `libnvnmos.so` is installed on the system linker path.

`NVNMOS_INCLUDE_DIR` defaults to `../src/` (where `nvnmos.h` lives in-tree). Override only to bindgen against a different header location.

`protoc` is vendored via [`protobuf-src`](https://crates.io/crates/protobuf-src) — no system `protoc` is required.

## Scale Benchmark (`nvnmosd-bench`)

Measures daemon memory usage and RPC/HTTP latencies across preset scenarios. See
[`doc/designs/nvnmosd/scale-smoke.md`](../doc/designs/nvnmosd/scale-smoke.md).

```sh
# Build libnvnmos first (../build/libnvnmos.so), then:
export NVNMOS_LIB_DIR=/absolute/path/to/build
./rust/nvnmosd-bench/scripts/run-nvnmosd-scale-smoke.sh              # default: small preset
PRESETS="medium large" ./rust/nvnmosd-bench/scripts/run-nvnmosd-scale-smoke.sh
```

Results land in `rust/nvnmosd-bench/results/` (gitignored).

## Running the Smoke Test

RPC-only regression client — no GStreamer. For a full media walk-through, use
[Quick start](#quick-start) instead.

```sh
# Terminal 1: daemon (with NVNMOS_LIB_DIR / LD_LIBRARY_PATH set)
cargo run --bin nvnmosd

# Terminal 2: example client
cargo run --bin nvnmosd-example
```

The example exercises every RPC the daemon currently implements: both Node lifetimes (session-refcounted and persistent), session attachment/refcounting, resource registration with `name` ↔ `x-nvnmos-name` mismatch detection, the activations stream (`SubscribeActivations` opened for the resource phase with a background auto-ack task for `AckActivation`), out-of-band `SyncResourceState` (activate with an updated transport file + deactivate), an in-band IS-05 PATCH activate/deactivate round-trip against libnvnmos's HTTP server, resource removal, and session-close-time resource cleanup. The resource phase deliberately registers a Sender and a Receiver under the **same** `name` to exercise the side-disambiguated namespace: the daemon's `by_name` index is keyed on `(node_seed, side, name)` and `ActivationEvent.side` plus the safe wrapper's `Side` enum let the client tell apart activations that share a `name`. Successful output is visible in both terminals via the `tracing` log.

By default the example autodetects a local interface IP via the routing table. Override with `--interface-ip <ip>` if the autodetect picks the wrong one (or fails on a sandboxed network).

The example sets `NodeConfig.http_port` to a fixed value (8010 by default, configurable via `--http-port`); libnvnmos collapses every HTTP API (Node, Connection, ...) onto that single port, so the in-band Connection API PATCH round-trip also targets it: `http://<interface-ip>:8010/x-nmos/connection/v1.1/single/senders/<sender_resource_id>/staged`. Pass `--skip-connection` to disable the round-trip step when libnvnmos's HTTP server isn't reachable from the example.

To poke libnvnmos manually after the automated flow finishes, run with `--hold-secs N` (e.g. `--hold-secs 30`); the example will hold the resource-phase session open for N seconds with the receiver still registered and the activations stream still attached, so a `curl -X PATCH` against the receiver's IS-05 endpoint will round-trip through `SubscribeActivations` / `AckActivation` exactly like the automated step did for the sender.
