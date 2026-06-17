<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# nvnmosd

Linux-first NMOS daemon: one process hosts multiple NMOS **Nodes** (by `node_seed`),
**sessions** (gRPC clients), and session-linked NMOS **Senders/Receivers** backed by
[`libnvnmos`](../../src/). Clients talk to it over gRPC (`nvnmos-rpc` /
[`nvnmosd.proto`](../nvnmos-rpc/proto/nvnmosd.proto)).

Architecture, element integration, and historical design rationale live in
[`doc/designs/nvnmosd/README.md`](../../doc/designs/nvnmosd/README.md). This
file describes the **as-built** daemon surface for operators and client authors.

## Build and Run

Requires a pre-built `libnvnmos.so` — see the workspace
[`../README.md`](../README.md#building) for `NVNMOS_LIB_DIR` and the CMake/Conan
flow.

```sh
export NVNMOS_LIB_DIR=/absolute/path/to/build
export LD_LIBRARY_PATH="$NVNMOS_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

cargo build --bin nvnmosd
cargo run --bin nvnmosd -- --uds /tmp/nvnmosd.sock
```

## Command Line

| Flag / env | Default | Meaning |
|------------|---------|---------|
| `--uds` / `NVNMOSD_UDS` | `/tmp/nvnmosd.sock` | Unix socket for gRPC. Fails if another listener owns the path; removes a stale socket file from a crashed process. |

Logging uses `tracing`; set `RUST_LOG` as usual (e.g. `RUST_LOG=info`).

## gRPC API (Summary)

| Area | RPCs |
|------|------|
| Node lifecycle | `AddNode`, `RemoveNode`, `OpenSession`, `CloseSession` |
| Resources | `AddSender`, `AddReceiver`, `RemoveResource` |
| IS-05 activation | `SubscribeActivations` (server stream), `AckActivation` |
| Out-of-band sync | `SyncResourceState` |

**Session contract (enforced when session GC is on, default):**

1. Call `SubscribeActivations` promptly after `OpenSession`, **before**
   `AddSender` / `AddReceiver`.
2. To keep a session alive after dropping `SubscribeActivations`, call it
   again within the resubscribe timeout.
3. Call `CloseSession` for a clean shutdown.

Missed deadlines trigger implicit `CloseSession` (same teardown as the RPC).
Details and env vars below; full proto comments in `nvnmosd.proto`.

**Node flavours:**

- **Session-refcounted** (default): created on first `OpenSession` for a
  `node_seed`; destroyed when the last session on that node closes.
- **Persistent**: created with `AddNode`; survives until `RemoveNode`.

Many sessions may attach to the same Node (same `node_seed`). Each session owns
the resources it registers; activations are routed to the session that added the
resource.

## Environment Variables

### Session Liveness (Implicit `CloseSession`)

| Variable | Default | Meaning |
|----------|---------|---------|
| `NVNMOSD_SESSION_GC` | on | Disable with `0`, `false`, `off`, or `no`. |
| `NVNMOSD_SESSION_SUBSCRIBE_TIMEOUT_SEC` | 60 | Subscribe within this many seconds of `OpenSession`. |
| `NVNMOSD_SESSION_RESUBSCRIBE_TIMEOUT_SEC` | 5 | Resubscribe within this many seconds after the activation stream ends. |

### glibc Heap Trim (Linux)

| Variable | Default | Meaning |
|----------|---------|---------|
| `NVNMOSD_MALLOC_TRIM` | on | Trim after teardown when a node has no remaining resources. Disable with `0`, `false`, `off`, or `no`. |
| `NVNMOSD_MALLOC_INFO` | off | Log full `malloc_info` XML at `debug` around each trim. Enable with `1`, `true`, `on`, or `yes`. |

## Clients and Tests

| Tool | Role |
|------|------|
| [`gst-nmos-rs`](../gst-nmos-rs/) | GStreamer elements (`nmossrc` / `nmossink`) — primary production client. |
| [`nvnmosd-example`](../nvnmosd-example/) | Interactive regression client (C `nvnmos-example` equivalent). |
| [`nvnmosd-bench`](../nvnmosd-bench/) | Scale / latency smoke across many sessions and resources. |
| [`tests/session_gc.rs`](tests/session_gc.rs) | Integration tests for implicit `CloseSession`. |

Example smoke (two terminals, `rust/` workspace root, `NVNMOS_LIB_DIR` set):

```sh
# Terminal 1
cargo run --bin nvnmosd

# Terminal 2
cargo run --bin nvnmosd-example
```

See [`../README.md`](../README.md#running-the-smoke-test) for example flags
(`--interface-ip`, `--hold-secs`, Connection API PATCH round-trip).

Scale benchmarks: [`doc/designs/nvnmosd/scale-smoke.md`](../../doc/designs/nvnmosd/scale-smoke.md).
