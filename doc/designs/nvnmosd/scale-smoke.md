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

Preset runs use the default **`--remove-senders 0` / `--remove-receivers 0`**: resources
are torn down implicitly in `CloseSession` (one batched RPC per session). Pass positive
remove counts to measure explicit per-resource `RemoveResource` teardown separately —
e.g. `--remove-senders 1000 --remove-receivers 1000` on `large` to remove all before close.

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
  "scenario": { "label": "small", "nodes": 1, "add_senders": 10, "add_receivers": 10, "remove_senders": 0, "remove_receivers": 0, "sessions": 1, "clients": 1, "syncs": 1, "patches": 10 },
  "memory_kb": { "baseline": 42000, "after_add": 98000, "after_activate": 100500, "after_remove": 100500, "after_close": 43000 },
  "cpu_pct": {
    "open": { "avg": 45.0, "max": 80.0, "p95": 75.0, "samples": 1 },
    "add": { "avg": 120.0, "max": 180.0, "p95": 170.0, "samples": 8 },
    "activate_patch": { "avg": 90.0, "max": 110.0, "p95": 105.0, "samples": 4 },
    "close": { "avg": 60.0, "max": 90.0, "p95": 85.0, "samples": 2 },
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
| Memory (kB) | Resident Set Size (RSS) - baseline, after open, after add, after activate, after remove, after close |
| **cpu_pct** (%, Linux) | Per-phase daemon CPU when `--daemon-pid` set: background `/proc` sample every **100 ms** (override `--cpu-sample-ms`). Phases: `open`, `add`, `close`, plus `subscribe` / `activate_sync` / `activate_patch` / `remove` when those phases run. Each phase reports `avg`, `max`, `p95`, `samples`; `overall` merges all interval samples. Values are **core-equivalent** (100% = one full core; may exceed 100% on multi-threaded load). Omitted when no daemon PID. |
| **OpenSession** (ms) | Per gRPC call |
| **AddSender** / **AddReceiver** (ms) | Per gRPC call |
| **SubscribeActivations** (ms) | Per session when `patches > 0` |
| **SyncResourceState** (ms) | Per activate/deactivate when `syncs > 0` |
| **IS-05 GET / PATCH** (ms) | Per PATCH workflow when `patches > 0` |
| **RemoveSender** / **RemoveReceiver** (ms) | Per explicit remove when `remove-senders` / `remove-receivers` > 0 |
| **CloseSession** (ms) | Per session close (includes implicit resource cleanup when remove counts are 0) |

Latency values are **milliseconds** (float): `count`, `total`, `p50`, `p95`, `p99`, `max`.

**Interpretation Hints**

- **RSS vs resources** — daemon maps + libnvnmos model growth.
- **`remove_sender_ms` / `remove_receiver_ms` vs add** — when remove counts match add
  counts, each remove field is directly comparable to its matching add field.
- **`close_session_ms` with default removes** — includes batched per-session resource
  teardown; much larger than empty-session close when explicit remove ran first.
- **`after_remove` vs `after_close`** — `after_remove` is sampled only when explicit
  remove ran; otherwise equals post-activate RSS.
- **RSS vs nodes** — more `NodeServer` / HTTP listeners at fixed per-node resource count.
- **cpu_pct.add / activate_patch** — hottest phases on large presets; compare `avg` across runs, `max` for burst peaks.
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

- **Node side (gRPC over UDS):** `OpenSession`, register resources (`--add-senders` /
  `--add-receivers`, aliases `--senders` / `--receivers`), optional `SubscribeActivations`
  + auto-`AckActivation`, optional explicit `RemoveResource` (`--remove-senders` /
  `--remove-receivers`, default `0`), then `CloseSession`.
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
7. **remove_resources** *(when `remove-senders` or `remove-receivers` > 0)* — explicit
   `RemoveResource`, parallel per session (sequential within session). Skipped by default.
8. **close_sessions** — parallel `CloseSession` (implicit cleanup of any resources not
   removed in step 7).

The driver checks `base_http_port .. base_http_port + nodes − 1` are free before start.

## Scale Knobs

| Dimension | Range | Notes |
|-----------|-------|-------|
| **Nodes** | 1–10000 | One libnvnmos HTTP listener per seed; distinct `http_port` each. |
| **Add senders / receivers** | 0–100000 each | Registered via gRPC (`--add-senders` / `--add-receivers`; `--senders` / `--receivers` aliases). |
| **Remove senders / receivers** | 0–add count each | Explicit `RemoveResource` before close (`0` = skip; default). |
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

**Remove counts** default to **`0`** (implicit `CloseSession` cleanup — faster for scale
presets). Pass the add count on each side to benchmark explicit per-resource teardown, or
smaller counts for partial-remove / staggered scenarios.

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

### Staggered / interleaved scenario (planned)

The current harness runs **clean phases** (register all → activate all → remove subset →
close). That is ideal for scale regression and per-phase CPU/RSS attribution, but it does
not exercise cross-phase races that show up in production.

A follow-on **staggered** mode (separate from the phased presets above) would interleave
operations deterministically — not random churn — so failures are bisectable:

1. **Register batch A** (first *N* senders on each session).
2. **Start PATCH activate on batch A** while **registering batch B** on the same sessions
   (disjoint resource sets; never sync+PATCH the same sender).
3. **PATCH deactivate batch A** while **sync activate/deactivate on batch B** senders.
4. **`RemoveResource` on a subset of batch A** while batch B remains registered.
5. **`CloseSession`** tears down whatever is left.

Design constraints for this mode:

- **Disjoint targets** — each workflow names a specific `resource_handle`; no conflicting
  activation paths on the same sender.
- **Different metrics** — emit `churn_errors` and steady-state `memory_kb.after_cycle`
  rather than forcing interleaved work into today's per-phase `cpu_pct` buckets.
- **Build on explicit remove** — opt-in `--remove-*` measures per-resource teardown
  separately from `CloseSession`; staggered mode adds partial removes mid-churn on top of
  that RPC path.

Suggested first increment: **register ∥ PATCH** on disjoint sender sets (no
`RemoveResource` yet). Second increment: add **`RemoveResource` during deactivate** on a
third disjoint set. Full staggered script lands as a new bench label (e.g.
`--label staggered`) or wrapper script, keeping `small`/`large` presets unchanged.
