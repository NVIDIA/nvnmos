<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# nvnmosd

[TOC]

Linux-first NMOS daemon: one process hosts multiple NMOS **Nodes** (by `node_seed`),
**sessions** (gRPC clients), and session-linked NMOS **Senders/Receivers** backed by
[`libnvnmos`](https://nvidia.github.io/nvnmos/nvnmos_8h.html). Clients talk to it
over the [`nvnmosd` gRPC API](https://nvidia.github.io/nvnmos/grpc/).

The [Core NvNmos Concepts](https://nvidia.github.io/nvnmos/concepts.html)
guide explains the transport file, activation direction, and identity model
shared by the daemon, C API, and GStreamer elements.
[Daemon design record](https://github.com/NVIDIA/nvnmos/blob/main/doc/designs/nvnmosd/README.md)
covers architecture, element integration, and historical rationale.
[Concurrency and lock ordering](https://github.com/NVIDIA/nvnmos/blob/main/doc/designs/nvnmosd/lock-ordering.md)
documents the no-FFI-under-`state` invariant.
This documentation describes the **as-built** daemon surface for operators and
client authors.

## Using the gRPC API

**Node flavours:**

- **Session-refcounted** (default): created on first `OpenSession` for a
  `node_seed`; destroyed when the last session on that node closes.
- **Persistent**: created with `AddNode`; survives until `RemoveNode`.

Many sessions may attach to the same Node (same `node_seed`). Each session owns
the resources it registers; activations are routed to the session that added the
resource.

| Area | RPCs |
|------|------|
| Node lifecycle | `AddNode`, `RemoveNode`, `OpenSession`, `CloseSession` |
| Resources | `AddSender`, `AddReceiver`, `RemoveResource` |
| IS-05 activation | `SubscribeActivations` (server stream), `AckActivation` |
| Out-of-band sync | `SyncResourceState` |

### Minimal Client Sequence

Most clients use a session-refcounted Node:

1. Call `OpenSession` with a `NodeConfig`. The daemon creates the Node if the
   `seed` is new, or attaches the session to the existing Node for that seed.
2. Start `SubscribeActivations` immediately and keep the stream open while the
   session is in use.
3. Call `AddSender` or `AddReceiver` with the session handle, a caller-chosen
   resource name, and its configuring transport file.
4. For each `ActivationEvent`, apply the transport-file change to the local data
   plane, then call `AckActivation` with the result.
5. When the application changes its data plane without an IS-05 activation,
   call `SyncResourceState` to update the NMOS state.
6. Call `RemoveResource` when a Sender or Receiver is no longer needed, then
   call `CloseSession` during clean shutdown. Closing a session also removes any
   resources it still owns.

The activation stream must be active before resources are added. If session GC
is enabled (the default), missing the initial subscription or resubscription
deadline implicitly closes the session.

Use [`nvnmosd-example`](https://github.com/NVIDIA/nvnmos/tree/main/rust/nvnmosd-example)
for a complete Rust client and the
[gRPC API reference](https://nvidia.github.io/nvnmos/grpc/) for request fields,
responses, preconditions, and errors.

## Build and Run

Requires a pre-built `libnvnmos.so` — see the
[Rust workspace build instructions](https://github.com/NVIDIA/nvnmos/blob/main/rust/README.md#building)
for `NVNMOS_LIB_DIR` and the CMake/Conan flow.

```sh
export NVNMOS_LIB_DIR=/absolute/path/to/build
export LD_LIBRARY_PATH="$NVNMOS_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

cargo build --bin nvnmosd
cargo run --bin nvnmosd -- --uds /tmp/nvnmosd.sock
```

### Command Line

| Flag / env | Default | Meaning |
|------------|---------|---------|
| `--uds` / `NVNMOSD_UDS` | `/tmp/nvnmosd.sock` | Unix socket for gRPC. Fails if another listener owns the path; removes a stale socket file from a crashed process. |

Logging uses `tracing`; set `RUST_LOG` as usual (e.g. `RUST_LOG=info`).

## Environment Variables

### Session Liveness

These settings control when the daemon implicitly closes a session whose
activation subscription is missing or interrupted.

| Variable | Default | Meaning |
|----------|---------|---------|
| `NVNMOSD_SESSION_GC` | on | Disable with `0`, `false`, `off`, or `no`. |
| `NVNMOSD_SESSION_SUBSCRIBE_TIMEOUT_SEC` | 60 | Subscribe within this many seconds of `OpenSession`. |
| `NVNMOSD_SESSION_RESUBSCRIBE_TIMEOUT_SEC` | 5 | Resubscribe within this many seconds after the activation stream ends. |

### Node HTTP Port Allocation

| Variable | Default | Meaning |
|----------|---------|---------|
| `NVNMOSD_HTTP_PORT_MIN` | `18080` | Inclusive lower bound when `NodeConfig.http_port` is `0`. |
| `NVNMOSD_HTTP_PORT_MAX` | `18099` | Inclusive upper bound. |

When `NodeConfig.http_port` is **`0`**, the daemon picks the first port in `[MIN, MAX]` that is not already used by another Node and that the host can bind. When **`http_port` is non-zero**, the client chooses the port; the daemon rejects the create if that port is already taken by another Node or unavailable on the host.

### IS-13 annotations

The daemon remembers label, description, and writable tags for each `(seed, resource type, name)`. Node and Device use an empty name. Source, Flow, and Sender are separate entries that share the sender name. The gRPC API does not carry annotations: a new Node or a new sender/receiver is created with whatever the store already has for that key.

Names are configuration. Removing a sender, or destroying the node when its last session closes, does not forget annotations. A fresh name on every pipeline run leaves an entry behind. The only automatic removal is an IS-13 reset that clears the last annotated property on that key.

| Variable | Default | Meaning |
|----------|---------|---------|
| `NVNMOSD_ANNOTATION_API` | on | Mount the Annotation API and keep the checkpoint. Disable with `0`, `false`, `off`, or `no`. While off, the checkpoint is not opened and nodes do not advertise the API. |
| `NVNMOSD_ANNOTATION_CHECKPOINT_FILE` | `<socket-filename>-annotations.json` in the socket's directory | JSON file for the store. `/tmp/nvnmosd.sock` uses `/tmp/nvnmosd.sock-annotations.json`. |
| `NVNMOSD_ANNOTATION_DEBOUNCE_MS` | `1000` | How long after the last change before rewriting the file. |
| `NVNMOSD_ANNOTATION_ENTRY_LIMIT` | `10000` | At the limit, a new key is not stored and an error is logged. Keys already stored still apply and can still be updated. |

A missing file is created empty at start. An existing file that cannot be read or parsed, a read-only tag in the file, or a directory the process cannot write, makes the daemon exit at start. Delete or edit the file, or point `NVNMOSD_ANNOTATION_CHECKPOINT_FILE` at a directory this user can write. Two daemons given the same path would overwrite each other's updates, because each write replaces the whole file. A checkpoint write that fails after startup is logged and the daemon keeps running. Stop the daemon before deleting the file (that clears every annotation) or editing it. A graceful stop writes the file. A crash can lose changes still inside the debounce window. The file lasts as long as its directory: the default sits next to the socket, a `/tmp` one does not survive a reboot.

### libnvnmos (inherited by the daemon process)

These are read by **libnvnmos** when a Node is created.
Set them on the `nvnmosd` process (or any process that loads `libnvnmos.so`).

| Variable | Default | Meaning |
|----------|---------|---------|
| `NVNMOS_EXPERIMENTAL_SETTINGS` | off | Mount nmos-cpp `GET`/`PATCH /settings/all` on the Node HTTP port. Enable with `1`, `true`, `on`, or `yes`. Debug only: the JSON includes internal Node settings such as stored transport files. |

### Linux glibc Heap Trimming

| Variable | Default | Meaning |
|----------|---------|---------|
| `NVNMOSD_MALLOC_TRIM` | on | Trim after teardown when a node has no remaining resources. Disable with `0`, `false`, `off`, or `no`. |
| `NVNMOSD_MALLOC_INFO` | off | Log full `malloc_info` XML at `debug` around each trim. Enable with `1`, `true`, `on`, or `yes`. |

## Clients and Tests

| Tool | Role |
|------|------|
| [`gst-nmos-rs`](https://github.com/NVIDIA/nvnmos/tree/main/rust/gst-nmos-rs) | GStreamer elements (`nmossrc` / `nmossink`) — primary production client. |
| [`nvnmosd-example`](https://github.com/NVIDIA/nvnmos/tree/main/rust/nvnmosd-example) | Interactive regression client (C `nvnmos-example` equivalent). |
| [`nvnmosd-bench`](https://github.com/NVIDIA/nvnmos/tree/main/rust/nvnmosd-bench) | Scale / latency smoke across many sessions and resources. |
| [Session GC integration tests](https://github.com/NVIDIA/nvnmos/blob/main/rust/nvnmosd/tests/session_gc.rs) | Integration tests for implicit `CloseSession`. |

Example smoke (two terminals, `rust/` workspace root, `NVNMOS_LIB_DIR` set):

```sh
# Terminal 1
cargo run --bin nvnmosd

# Terminal 2
cargo run --bin nvnmosd-example
```

See the [Rust workspace smoke test](https://github.com/NVIDIA/nvnmos/blob/main/rust/README.md#running-the-smoke-test) for example flags
(`--interface-ip`, `--hold-secs`, Connection API PATCH round-trip).

[Scale benchmark guide](https://github.com/NVIDIA/nvnmos/blob/main/doc/designs/nvnmosd/scale-smoke.md).

## Other Ways to Use NvNmos

Use the [C API](https://nvidia.github.io/nvnmos/) to embed the NMOS control
plane directly in a Media Node application.

Use the
[GStreamer elements](https://nvidia.github.io/nvnmos/gstreamer/) — `nmossrc`
and `nmossink` — to connect GStreamer pipelines to `nvnmosd` and configure the
data plane.
