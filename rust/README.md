<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# nvnmos Rust Workspace

Rust components for the new GStreamer NMOS plugin family described in
[`doc/designs/nvnmosd/README.md`](../doc/designs/nvnmosd/README.md).

## Crates

| Crate              | Kind        | Purpose                                                              |
| ------------------ | ----------- | -------------------------------------------------------------------- |
| `nvnmos-sys`       | library     | `bindgen`-generated FFI to the C `libnvnmos` API in `../src/nvnmos.h`. |
| `nvnmos`           | library     | Safe Rust wrapper over `nvnmos-sys`: RAII `NodeServer`, `Result` errors, deterministic id accessors. |
| `nvnmos-rpc`       | library     | gRPC protocol crate (`nvnmosd.proto` + `tonic`-generated stubs).     |
| `nvnmosd`          | binary      | The NMOS daemon. Wraps `nvnmos-sys`, serves `nvnmos-rpc`.            |
| `nvnmosd-example`  | binary      | Example/regression client modelled on the C `nvnmos-example`.        |
| `nvnmosd-bench`    | binary      | Scale smoke / benchmark client for `nvnmosd`.                        |
| `gst-nmos-rs`      | GStreamer plugin (cdylib) | `nmos` plugin (`nmossrc` / `nmossink`); session lifecycle, inner `mxlsink`/`mxlsrc`, IS-05 activation handling, deferred `nmossink` AddSender (peer-query at READY→PAUSED), `nmossrc` essence-caps advertisement, and property-override-vs-cross-check semantics are all wired. See [`gst-nmos-rs/README.md`](gst-nmos-rs/README.md). |

See `gst-nmos-rs`'s own
[`gst-nmos-rs/README.md`](gst-nmos-rs/README.md) for the per-element
property matrix, load instructions, and status.

## Container Image

For a combined image with `nvnmosd` + `gst-nmos-rs` + plugins for `transport=mxl` and
`transport=udp`/`udp2`, suitable for `gst-launch-1.0` in Kubernetes or Docker, see
[`docker/gst-nmos-rs/README.md`](../docker/gst-nmos-rs/README.md).

## Building

The workspace **MSRV** is **Rust 1.85** (`rust-version` in [`Cargo.toml`](Cargo.toml)). Development, CI, and container builds pin **1.92** in [`rust-toolchain.toml`](rust-toolchain.toml) — the minimum needed for gst-plugins-rs `0.15.2` in [`docker/gst-nmos-rs`](../docker/gst-nmos-rs/).

`nvnmos-sys` links against a pre-built `libnvnmos.so`. The Rust crate does **not** build the C library itself (today); use the existing CMake/Conan workflow under `../src/` to produce `libnvnmos.so` first, then point the Rust build at it:

```sh
# 1. Build libnvnmos.so via the existing CMake/Conan flow (one of the build*/ trees).
# 2. Then:
export NVNMOS_LIB_DIR=/absolute/path/to/build/tree     # contains libnvnmos.so
cargo build --workspace
```

`NVNMOS_LIB_DIR` is optional — when unset, the linker searches the standard system paths (`/usr/local/lib`, `/usr/lib`, etc.), so an installed `libnvnmos` works without the env var.

`NVNMOS_INCLUDE_DIR` defaults to `../src/` (where `nvnmos.h` lives in-tree). Override only if you want to bindgen against a different header location.

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

```sh
# Terminal 1: daemon
cargo run --bin nvnmosd

# Terminal 2: example client
cargo run --bin nvnmosd-example
```

The example exercises every RPC the daemon currently implements: both Node lifetimes (session-refcounted and persistent), session attachment/refcounting, resource registration with `name` ↔ `x-nvnmos-name` mismatch detection, the activations stream (`SubscribeActivations` opened for the resource phase with a background auto-ack task for `AckActivation`), out-of-band `SyncResourceState` (activate with an updated transport file + deactivate), an in-band IS-05 PATCH activate/deactivate round-trip against libnvnmos's HTTP server, resource removal, and session-close-time resource cleanup. The resource phase deliberately registers a Sender and a Receiver under the **same** `name` to exercise the side-disambiguated namespace: the daemon's `by_name` index is keyed on `(node_seed, side, name)` and `ActivationEvent.side` plus the safe wrapper's `Side` enum let the client tell apart activations that share a `name`. Successful output is visible in both terminals via the `tracing` log.

By default the example autodetects a local interface IP via the routing table. Override with `--interface-ip <ip>` if the autodetect picks the wrong one (or fails on a sandboxed network).

The example sets `NodeConfig.http_port` to a fixed value (8010 by default, configurable via `--http-port`); libnvnmos collapses every HTTP API (Node, Connection, ...) onto that single port, so the in-band Connection API PATCH round-trip also targets it: `http://<interface-ip>:8010/x-nmos/connection/v1.1/single/senders/<sender_resource_id>/staged`. Pass `--skip-connection` to disable the round-trip step when libnvnmos's HTTP server isn't reachable from the example.

To poke libnvnmos manually after the automated flow finishes, run with `--hold-secs N` (e.g. `--hold-secs 30`); the example will hold the resource-phase session open for N seconds with the receiver still registered and the activations stream still attached, so a `curl -X PATCH` against the receiver's IS-05 endpoint will round-trip through `SubscribeActivations` / `AckActivation` exactly like the automated step did for the sender.
