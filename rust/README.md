# nvnmos Rust workspace

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

The `gst-nmos-rs` GStreamer plugin will join the workspace once the daemon's gRPC surface is stable enough to consume from a real GStreamer element.

## Building

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

## Running the smoke test

```sh
# Terminal 1: daemon
cargo run --bin nvnmosd

# Terminal 2: example client
cargo run --bin nvnmosd-example
```

The example exercises every RPC the daemon currently implements: both Node lifetimes (session-refcounted and persistent), session attachment/refcounting, resource registration with `internal_id` ↔ `x-nvnmos-id` mismatch detection, the activations stream (`SubscribeActivations` opened for the resource phase with a background auto-ack task for `AckActivation`), out-of-band `SyncResourceState` (activate with an updated transport file + deactivate), resource removal, and session-close-time resource cleanup. Successful output is visible in both terminals via the `tracing` log.

To drive a real IS-05 activation through the SubscribeActivations / AckActivation round-trip, run the example with `--hold-secs N` (e.g. `--hold-secs 30`), then `curl -X PATCH` libnvnmos's IS-05 endpoint at `http://<host>:3212/x-nmos/connection/v1.1/single/{senders,receivers}/<resource_id>/staged` with `{"activation":{"mode":"activate_immediate"}}` while the example is holding. The auto-ack task will log the received `ActivationEvent` and the matching `AckActivation`, and the daemon will log the corresponding routing decision.
