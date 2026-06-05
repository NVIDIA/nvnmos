<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# nvnmosd Scale Smoke / Benchmark Harness

Smoke and scaling measurements for a single `nvnmosd` process: memory usage, per-phase
daemon CPU (Linux `/proc`), and latency of gRPC lifecycle operations and IS-05
activation paths.

## Quick Start

**Prerequisites**

1. Build `libnvnmos.so` via the usual CMake flow under `src/` (install tree or `build/`).
2. Rust workspace: see [`rust/README.md`](../../rust/README.md).

```bash
export NVNMOS_LIB_DIR=/absolute/path/to/build   # directory containing libnvnmos.so

./rust/nvnmosd-bench/scripts/run-nvnmosd-scale-smoke.sh
```

Default: **`small`** preset only (`syncs=1`, `patches=10`). Results in JSON Lines (JSONL)
format land in `rust/nvnmosd-bench/results/` (gitignored).

```bash
PRESETS="medium large" ./rust/nvnmosd-bench/scripts/run-nvnmosd-scale-smoke.sh
PRESETS=xlarge ./rust/nvnmosd-bench/scripts/run-nvnmosd-scale-smoke.sh
# sync-only: edit preset row or pass --syncs N --patches 0 to nvnmosd-bench directly
```

**Presets** — each row sets all scale axes:

| Label | nodes | senders | receivers | sessions | clients | syncs | patches | Intent |
|-------|-------|---------|-----------|----------|---------|-------|---------|--------|
| small | 1 | 10 | 10 | 1 | 1 | 1 | 10 | Harness sanity |
| medium | 1 | 100 | 100 | 1 | 1 | 10 | 100 | Big single node |
| large | 1 | 1000 | 1000 | 10 | 10 | 100 | 100 | Bigger node, multi-session |
| xlarge | 10 | 10000 | 10000 | 100 | 10 | 1000 | 1000 | Multi-node |

**Node HTTP ports:** each bench node gets `http_port = base_http_port + node_index`.
`base_http_port` defaults to `18080` (`NVNMOSD_BENCH_BASE_PORT`, passed through as
`nvnmosd-bench --base-http-port`). `xlarge` uses 18080–18089.

**Registration API:** bench `OpenSession` sets `network_services.registration_port` to
**65535** and leaves `registration_address` unset. In libnvnmos this is an internal
sentinel: DNS-SD browse/advertise disabled, no `registry_address`, no Registration
API HTTP — so `CloseSession` is not dominated by mDNS browse timeouts (~seconds on WSL)
or dead-registry retries.

## Reading Results

One **JSON object per line** (JSONL). Example:

```json
{
  "scenario": { "label": "small", "nodes": 1, "senders": 10, "receivers": 10, "sessions": 1, "clients": 1, "syncs": 1, "patches": 10 },
  "memory_kb": { "baseline": 42000, "after_register": 98000, "after_activate": 100500, "after_teardown": 43000 },
  "cpu_pct": {
    "open": { "avg": 45.0, "max": 80.0, "p95": 75.0, "samples": 1 },
    "register": { "avg": 120.0, "max": 180.0, "p95": 170.0, "samples": 8 },
    "activate_patch": { "avg": 90.0, "max": 110.0, "p95": 105.0, "samples": 4 },
    "teardown": { "avg": 60.0, "max": 90.0, "p95": 85.0, "samples": 2 },
    "overall": { "avg": 100.0, "max": 180.0, "p95": 160.0, "samples": 15 }
  },
  "wall_ms": 412.5,
  "add_sender_ms": { "count": 10, "total": 8.0, "p50": 0.15, "p95": 0.22, "p99": 0.28, "max": 0.35 },
  "close_session_ms": { "count": 1, "total": 3.1, "p50": 3.1, "p95": 3.1, "p99": 3.1, "max": 3.1 }
}
```

**`wall_ms`** — end-to-end wall clock for the whole scenario (all phases, sequential).

**Per-RPC latency fields** (`add_sender_ms`, `close_session_ms`, …) — one sample per
call. Phases spawn **parallel** tasks across sessions (and up to `clients`
concurrent controller HTTP workflows for PATCH). So:

- Do **not** multiply `p50` by `count` to compare with `wall_ms`.
- **`max`** is closer to that phase's wall-clock cost than `p50 × count`.
- **`total`** is the sum of all per-call samples (aggregate client wait time); it can be
  much larger than `wall_ms` when many calls overlap on the clock but each waited on a
  serialized daemon lock.

| Metric | Sampled |
|--------|----------------|
| Memory (kB) | Resident Set Size (RSS) - baseline, after open, after register, after activate, after teardown |
| **cpu_pct** (%, Linux) | Per-phase daemon CPU when `--daemon-pid` set: background `/proc` sample every **100 ms** (override `--cpu-sample-ms`). Phases: `open`, `register`, `teardown`, plus `subscribe` / `activate_sync` / `activate_patch` when those phases run. Each phase reports `avg`, `max`, `p95`, `samples`; `overall` merges all interval samples. Values are **core-equivalent** (100% = one full core; may exceed 100% on multi-threaded load). Omitted when no daemon PID. |
| **OpenSession** / **CloseSession** (ms) | Per gRPC call |
| **AddSender** / **AddReceiver** (ms) | Per gRPC call |
| **SubscribeActivations** (ms) | Per session when `patches > 0` |
| **SyncResourceState** (ms) | Per activate/deactivate when `syncs > 0` |
| **IS-05 GET / PATCH** (ms) | Per PATCH workflow when `patches > 0` |

Latency values are **milliseconds** (float): `count`, `total`, `p50`, `p95`, `p99`, `max`.

**Interpretation Hints**

- **RSS vs resources** — daemon maps + libnvnmos model growth.
- **RSS vs nodes** — more `NodeServer` / HTTP listeners at fixed per-node resource count.
- **cpu_pct.register / activate_patch** — hottest phases on large presets; compare `avg` across runs, `max` for burst peaks.
- **PATCH p95** vs **sync_activate_ms** — HTTP + in-band path vs out-of-band RPC only.

## Architecture

```
run-nvnmosd-scale-smoke.sh
  ├─ start nvnmosd (UDS)
  ├─ loop presets → nvnmosd-bench --label … --daemon-pid $!
  ├─ append JSONL → results/<timestamp>.jsonl
  └─ stop nvnmosd (SIGTERM)
```

**`nvnmosd-bench`** (`rust/nvnmosd-bench/`) — modelled on `nvnmosd-example`:

- **Node side (gRPC over UDS):** `OpenSession`, register resources, optional
  `SubscribeActivations` + auto-`AckActivation`, `CloseSession`.
- **Controller side (HTTP):** GET/PATCH on Connection API when `patches > 0`.
- Minimal ST 2110-20 SDP; no GStreamer.

Memory and CPU (Linux): `/proc/<daemon-pid>/status` `VmRSS` and per-phase background
sampling of `/proc/<daemon-pid>/stat` when `--daemon-pid` / `NVNMOSD_PID` is set.

## Phases (One Scenario Run)

1. **baseline** — idle daemon RSS.
2. **open_sessions** — parallel `OpenSession` (one gRPC client per session).
3. **subscribe** *(when `patches > 0`)* — activations stream + background `AckActivation` per session.
4. **add_resources** — parallel per session; senders then receivers on each session's client.
5. **activate_sync** *(when `syncs > 0`)* — `syncs` out-of-band `SyncResourceState` workflows.
6. **activate_patch** *(when `patches > 0`)* — `patches` in-band workflows: GET staged → PATCH
   activate → GET staged → PATCH deactivate (`clients` limits HTTP concurrency).
7. **close_sessions** — parallel `CloseSession`.

The driver checks `base_http_port .. base_http_port + nodes − 1` are free before start.

## Scale Knobs

| Dimension | Range | Notes |
|-----------|-------|-------|
| **Nodes** | 1–10000 | One libnvnmos HTTP listener per seed; distinct `http_port` each. |
| **Senders / receivers** | 0–100000 each | Registered via gRPC. |
| **Sessions** | 0–10000 | Concurrent gRPC sessions (`>= 1` when registering resources). |
| **Clients** | 0–1000 | Concurrent controller HTTP workflows (`0` when `patches == 0`). |
| **Syncs** | 0–100000 | Out-of-band `SyncResourceState` workflows (`0` = skip sync phase). |
| **Patches** | 0–100000 | In-band GET/PATCH activation workflows (`0` = skip PATCH phase). |

`nvnmosd-bench` enforces these upper bounds as typo guards, not product limits.

Resources are placed **round-robin across nodes** (`node_index = resource_index % nodes`).
Sessions are opened **round-robin across nodes** (`session_index % nodes`). Each resource
is registered on **session `resource_index % sessions`**.

**`syncs`** and **`patches`** are how many activation **workflows** to run (independent
counts). Targets are **evenly spaced** when the count is at most the number of registered
Connection API **senders** (receivers are registered but not activated). Larger counts
**round-robin** through those senders again. PATCH HTTP work runs in parallel up to
**`clients`**. Set either or both axes; phases run in order: sync, then patch.

Common values:

- **`sessions == nodes`** — one session per node; resources on a node share that session.
- **`sessions == senders + receivers`** — one session per resource.
- **Other `sessions`** — `N` parallel gRPC clients; resources spread round-robin.
- **`syncs` / `patches`** — workflow counts; see preset table (10% sender sample on
  `large`/`xlarge` at time of writing).

PATCH requires a routable interface IP (autodetected, or `--interface-ip` on `nvnmosd-bench`).

## Details

### NMOS Roles

The bench plays **both** NMOS roles when `patches > 0`:

| Role | NMOS meaning | Bench behaviour |
|------|--------------|-----------------|
| **Controller** | IS-05 client: GET/PATCH Connection API | HTTP PATCH workflows (no `OpenSession`) |
| **Node data-plane client** | Registers resources, applies activations | gRPC: `OpenSession`, add/remove, subscribe, ack, close |

A controller does not own a daemon session. `clients` is concurrent
controller-side HTTP workflows in the one bench process, not “number of `OpenSession`s”.

### Concurrency

- **One gRPC connection per `OpenSession`.** Node-side RPCs run in parallel across sessions;
  within a session, register RPCs stay sequential on that connection.
- **`clients`** caps concurrent controller GET/PATCH HTTP workflows (`0` unused when
  `patches == 0`; `patches > 0` requires `clients >= 1`).
- **`syncs`** is how many out-of-band `SyncResourceState` round-trips to run (evenly
  spaced; `0` = none).
- **`patches`** is how many in-band GET/PATCH workflows to run (evenly spaced; `0` = none).
  PATCH latency includes HTTP, daemon delivery, auto-`AckActivation`, and waiting on the
  matching activation event.

### What Is Not Under Test

- Real media pipelines (`nmossrc` / `nmossink`, MXL, UDP).
- Multi-process daemon fan-out (always one `nvnmosd`).
- Production mDNS / registry discovery (bench uses registry-less sentinel; see above).

## Troubleshooting

**Port already in use** — the smoke script checks `base_http_port .. base_http_port + nodes − 1`
before start. Free stale listeners (often a prior `nvnmosd`) or set `NVNMOSD_BENCH_BASE_PORT`.

**Stale `nvnmosd` processes** — failed or interrupted runs can leave background daemons running
(`pgrep -a nvnmosd`). They are usually idle (~24 MB RSS each) but clutter process lists.
Clean up with `pkill -TERM -f 'target/debug/nvnmosd'` and `rm -f /tmp/nvnmosd*.sock`.

**PATCH / interface IP** — when `patches > 0`, SDP needs a routable local IP. The bench
autodetects via the routing table; on WSL or multi-homed hosts set
`NVNMOSD_BENCH_INTERFACE_IP` or pass `--interface-ip` to `nvnmosd-bench`.

**Short phases with few CPU samples** — `open` and `subscribe` can finish in under one
sample interval (default 100 ms), yielding `samples: 0` or `1`. Lower `--cpu-sample-ms`
(e.g. 25) for finer peaks on long phases; very short phases may still have no samples.

## Future Extensions

- Multiple `nvnmosd-bench` OS processes on one daemon.
- MXL `flow_def` resources.
- Persistent nodes (`AddNode`) mixed with session-refcounted nodes.
- `perf record` / heap profiling hooks on the daemon PID.
