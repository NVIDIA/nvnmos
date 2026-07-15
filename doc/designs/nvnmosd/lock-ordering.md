<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# `nvnmosd` — lock ordering and concurrent FFI

The daemon can deadlock, hang its whole gRPC surface, and strand a Node's
HTTP listener in `LISTEN`, when an in-band IS-05 / IS-08 activation runs
concurrently with an RPC that called libnvnmos while holding the daemon
`state` mutex.

Observed symptom: producer/consumer example pipelines run against a shared
long-lived `nvnmosd`; Ctrl+C one pipeline; the NMOS API becomes
non-responsive and the Node's `http-port` stays in `LISTEN`, so a relaunch
cannot bind it.

This document describes the failure mode, the invariant that fixes it, the
**as-built** orchestration in `rust/nvnmosd`, and what concurrency guarantees
remain (and do not remain) after releasing `state` around FFI.

## Locks in play

| Lock | Owner | Guards |
|------|-------|--------|
| `state` | `nvnmosd` (`Arc<Mutex<State>>`) | Nodes, sessions, resources, channel mappings, subscriptions, pending activations, `http_ports`, `pending_nodes` |
| `SessionGc.watchdogs` | `nvnmosd` | per-session watchdog abort handles |
| `model` (read/write) | libnvnmos / nmos-cpp (`node_model`) | the IS-04 / IS-05 / IS-08 resource model |

`model` is not visible to Rust directly; every libnvnmos FFI entry point
acquires it internally.

## The two lock orders (pre-fix)

### Order A — activation callback: `model` → `state`

nmos-cpp's `connection_activation_thread` (and the IS-08
`channelmapping_activation_thread`) take `model.write_lock()` for the whole
processing loop, and invoke the daemon's callback **while still holding it**:

- `nmos-cpp` `connection_activation.cpp` — `auto lock = model.write_lock();`
- `nmos-cpp` `connection_activation.cpp` — `connection_activated(...)`
- `nmos-cpp` `channelmapping_activation.cpp` — same shape

The callback lands in `route_activation` (`main.rs`), which takes `state`:

- dispatch: `state.lock()` → `dispatch_activation`
- then **blocks up to `ACTIVATION_ACK_TIMEOUT` (5 s) on the client ack while
  the model write-lock is still held by the caller**
- cleanup: `state.lock()` again → `cleanup_pending_activation`

So this path holds `model`, then wants `state` (twice), and keeps `model`
for up to 5 s per activation.

### Order B — RPC handler (pre-fix): `state` → `model`

Every mutating RPC handler held `state` across a `State::*` method that
called libnvnmos FFI; each FFI entry took `model`.

### Deadlock

Order A (`model`→`state`) and Order B (`state`→`model`) are opposite. Run
concurrently they are a classic AB-BA deadlock. Example:

1. An IS-05 activation is in flight: activation thread holds `model`, is in
   `route_activation`, blocked on the ack.
2. `AddSender` arrives: handler takes `state`, calls FFI, blocks on `model`.
3. The ack arrives: `AckActivation` needs `state` (held by the blocked
   `AddSender`) — cannot complete → activation cannot be acked.
4. After 5 s the activation times out and `route_activation` re-takes
   `state` for cleanup — but `AddSender` still holds `state` waiting on
   `model`. Neither can proceed. **Permanent deadlock.**

`CloseSession` was the worst case: after `remove_*` it dropped the
`NodeServer`, whose `destroy_nmos_node_server` sets `model.shutdown` (needs
`model`) and **joins the activation thread**. If that thread was parked
needing `state`, the join never returned; the cpprest `http_listener` was
never closed, so the socket stayed in `LISTEN` and a relaunch could not bind
the port.

The implicit close via session GC ran the same `close_session` under
`state` (`session_gc.rs`), so the GC self-heal path had the identical
deadlock.

## Invariant (post-fix)

**No libnvnmos FFI call while holding `state`.**

Bookkeeping (maps, reservations, session attach/detach) stays under `state`.
Blocking libnvnmos work runs only after the guard is dropped. Where teardown
needs a `NodeServer`, an `Arc<NodeServer>` captured under the lock keeps the
C++ object alive until deferred `*Ffi::run()` completes outside the lock.

`route_activation` already released `state` before the ack wait; both
`state` sections in that path remain FFI-free.

## As-built orchestration

RPC handlers in `main.rs` delegate to inherent `Daemon` helpers where the
pattern is non-trivial. `State` in `state.rs` owns the bookkeeping phases
and the `*Prep` / `*Ffi` / `*Ready` types.

### Three-phase add / create (prepare → run FFI → commit)

Used when daemon maps must be validated or reserved before libnvnmos runs,
then updated after FFI succeeds.

| RPC | First phase (`state`, locked) | Second phase (no `state` lock) | Third phase (`state`, locked) |
|-----|-------------------------------|--------------------------------|-------------------------------|
| `AddNode` | `prepare_add_node` → `CreateNodePrep` | `Daemon::run_ffi` → `CreateNodePrep::run_ffi` | `commit_add_node` |
| `OpenSession` (new Node) | `prepare_open_session` → `CreateNodePrep` | `Daemon::run_ffi` → `CreateNodePrep::run_ffi` | `commit_open_session` |
| `OpenSession` (existing Node) | `prepare_open_session` → `Attached` | — (no FFI) | — |
| `AddSender` / `AddReceiver` | `prepare_add_resource` | `AddResourcePrep::run_ffi` | `commit_add_resource` |
| `AddChannelMapping` | `prepare_add_channelmapping` | `AddChannelMappingPrep::run_ffi` | `commit_add_channelmapping` |

Node create is special: `CreateNodePrep::run_ffi` builds the
`NodeServer` (`create_nmos_node_server` + `node_id()`), but activation
callbacks are supplied by `Daemon::run_ffi` in `main.rs` because they route
into `route_activation` / `route_channelmapping_activation` and need
`Arc<Mutex<State>>` — which `State` cannot capture from inside `&mut self`.

On FFI failure during node create, `Daemon::run_ffi` briefly re-locks
`state` to call `abort_pending_node` (drops the pending seed and port).

### Two-phase remove / close / sync (bookkeeping → deferred FFI)

Used when all daemon map updates can finish under the lock; libnvnmos
cleanup runs afterwards.

| RPC | Under `state` | Outside `state` |
|-----|---------------|-----------------|
| `RemoveNode` | `remove_node` → `RemoveNodeFfi` | `ffi.run()` |
| `CloseSession` | `close_session` → `CloseSessionFfi` | `ffi.run()` (removals + optional `NodeServer` destroy) |
| `RemoveResource` | `remove_resource` → `RemoveResourceFfi` | `ffi.run()` |
| `RemoveChannelMapping` | `remove_channelmapping` → `RemoveChannelMappingFfi` | `ffi.run()` |
| `SyncResourceState` | `sync_resource_state` → `SyncResourceFfi` | `ffi.run()?` |
| `SyncChannelMappingState` | `sync_channelmapping_state` → `SyncChannelMappingFfi` | `ffi.run()?` |

`CloseSessionFfi` and `RemoveNodeFfi` hold the last `Arc<NodeServer>`
until `run()` drops it, running `destroy_nmos_node_server` (shutdown +
thread-join + listener close) with no `state` lock held.

Session GC (`session_gc.rs`) uses the same close pattern: bookkeeping under
`state`, then `CloseSessionFfi::run()` outside the lock.

## Concurrency during the unlocked gap

Releasing and re-acquiring `state` does **not** remove all races; it removes
the AB-BA deadlock with `model`. Bookkeeping mutations remain serialized by
`state`. The unlocked window only runs blocking libnvnmos work or drops an
`Arc<NodeServer>` whose lifetime was captured under the lock.

### Protections

| Concern | Mechanism |
|---------|-----------|
| Double node create for same `node_seed` | `pending_nodes` + `http_ports` reserved in `prepare_*`; concurrent `OpenSession` / `AddNode` gets `ABORTED` / `ALREADY_EXISTS` |
| Add resource while session dies | `commit_add_resource` re-checks session exists; returns `NOT_FOUND` if `CloseSession` won the race |
| `NodeServer` destroyed while FFI uses it | `Arc<NodeServer>` in `*Prep` / `*Ffi` keeps the C++ object alive until `run_ffi` / `run()` completes |
| Conflicting FFI on one Node | libnvnmos `model` lock serialises internally |

### Accepted overlaps (documented outcomes, not bugs)

These follow directly from releasing `state` around FFI: libnvnmos and the
daemon maps can be briefly out of step. The daemon prefers a bounded,
recoverable outcome over holding `state` across blocking C++ work (which
reintroduces the deadlock).

| Scenario | Outcome |
|----------|---------|
| `CloseSession` during `AddSender` / `AddReceiver` / `AddChannelMapping` FFI | libnvnmos accepted the add, but the session is gone before commit. Commit returns `NOT_FOUND`; the daemon does **not** attempt a compensating libnvnmos remove. The object is a **stray** in libnvnmos until the Node is destroyed. The client sees a failed add, not a hung daemon. |
| Activation for a stray (or between add FFI and commit) | `dispatch_activation` / `dispatch_channelmapping_activation` find no daemon entry → NACK (`NoResource` / `NoChannelMapping`). The daemon does not remove the libnvnmos object on this path. |
| Node create: server up, not yet committed | No session or resources in daemon maps yet; activations cannot route. A second create for the same seed is blocked by `pending_nodes`. |

### Residual grey area

**HTTP port reuse timing:** `close_session` removes the port from
`http_ports` under `state`, but the OS socket may stay in `LISTEN` until
`destroy_nmos_node_server` completes in `ffi.run()`. A concurrent create
may succeed or fail depending on `is_tcp_port_bindable` and OS teardown
timing. The deadlock fix allows destroy to finish (activation thread can take
`state`); it does not formally prove instant port reuse.

## Non-issues (checked)

- **`watchdogs` vs `state`**: never nested in opposite orders.
  `cancel_timeout` / insert take `watchdogs` alone; the GC task takes `state`
  and `watchdogs` sequentially (`state` released first). No inversion.
- **`dispatch_activation` → `tx.try_send`**: non-blocking under `state`.
- **`complete_activation` → `ack_tx.send`**: capacity-1, single-use channel;
  will not block.
- **Log callback**: `log_bridge::forward` is lock-free. Close with no
  activation in flight only fires log callbacks during `close().wait()`, so
  thread-joins complete. The deadlock needs an **activation** callback (the
  only callback that takes `state`) mid-flight.

## Latency

Holding `state` across `node_server->open().wait()`, `close().wait()`, or
any FFI serialised every RPC behind one Node's blocking C++ work. The
post-fix split removes that stall.

Note: `AddSender` may still block on `model` while an in-band activation is
parked on the ack — that is correct serialization, not a deadlock.

## Trigger surface

The dangerous callback is the **in-band** IS-05 / IS-08 activation processed
by the activation threads — a controller (or smoke test) PATCH of `/staged`
with an activation. The daemon's own `auto-activate` path uses
`SyncResourceState` → `activate_connection`, which per the C contract does
**not** invoke the activation callback (it updates `/active` directly); but
it still takes `model.write_lock()`, so it remains an Order-B participant
when FFI ran under `state` (pre-fix).

## Regression coverage

`rust/nvnmosd/tests/lock_ordering_regression.rs` drives an in-band IS-05
activation (HTTP PATCH of `/staged` on the Node's port) so the activation
thread is parked on the ack, then concurrently issues `AddSender` and
`CloseSession` on the same Node. Post-fix:

- both RPCs complete within the test budget (no hang);
- `AddSender` may wait for the activation ack timeout while `model` is held
  — expected;
- `CloseSession` returns promptly and the HTTP port is released after close.

Also run the full `nvnmosd` test suite under default parallelism and
`--test-threads=1` (`http_port_release_repro`, `session_gc`, etc.).
