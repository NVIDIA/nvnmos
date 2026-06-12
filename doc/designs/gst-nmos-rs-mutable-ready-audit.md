<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# `gst-nmos-rs` — `mutable_ready()` property audit

`mutable_ready()` lets `g_object_set()` run in **NULL or READY**; it does **not** mean the element re-reads the property on every READY-time set. Values are taken from `Settings` only when a **lifecycle step** runs (below).

Override vs cross-check rules for transport-file interaction: [README](../rust/gst-nmos-rs/README.md#property-interaction-with-transport-file). This doc covers **when** each property must be set and which outer pspec flags apply.

## Lifecycle steps

Daemon RPC steps and **inner build** (element-local — not an RPC). Inner build is **not** the same as activation-event: it is when the bin swaps the fake chain for a real transport sink/source. That happens at **add-resource** when `auto-activate=true`, or at **activation-event** otherwise.

| Step | gRPC | GStreamer | What happens |
|------|------|-----------|--------------|
| **open-session** | `OpenSession` | NULL→READY | UDS session; node identity if this session creates the Node; `SubscribeActivations` starts. |
| **add-resource** | `AddSender` / `AddReceiver` | NULL→READY (eager) or READY→PAUSED (deferred) | IS-04 resource with configuring transport file. Skipped at open when nothing to register yet. |
| **activation-event** | `ActivationEvent` → `AckActivation` | Async (often PLAYING) | IS-05 PATCH → transport file on subscription → inner build (unless already real via `auto-activate`) + ack. |
| **inner build** | — | with add-resource if `auto-activate`; else during activation-event | Real transport sink/source replaces fake chain; `mxl-domain-path`, `*-properties` bags applied to fresh inner elements. |

```text
NULL
 │
 │  open-session (always)
 │  add-resource (eager) if transport-file* or caps set
 │  inner build if auto-activate (with eager add-resource)
 ▼
READY
 │
 │  add-resource (deferred; nmossink MXL only) if transport-file*
 │  and caps were unset at NULL→READY
 │  inner build if auto-activate (with deferred add-resource)
 ▼
PAUSED
 │
 ▼
PLAYING
    activation-event (async, usually during PLAYING)
    inner build (from activation-event)
```

**Deferred add-resource** (`nmossink`, `transport=mxl` only): at READY→PAUSED the element queries upstream peer caps and calls `AddSender`. RTP senders must register eagerly. **`nmossrc` has no deferred path.**

**`auto-activate`:** at add-resource, gates whether inner build runs there (real vs fake chain) — not a shortcut to PLAYING. Ignored at activation-event. When `true`, also calls `SyncResourceState` so daemon `/active` matches without an external PATCH.

## `mutable_ready` rule


| First step   | `nmossink`                                                                      | `nmossrc`                                                                 |
| ------------ | ------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| open-session | no `mutable_ready()` — set in **NULL**                                          | same                                                                      |
| add-resource | `mutable_ready()` — **NULL** (eager) or **NULL/READY** before PAUSED (deferred) | no `mutable_ready()` — eager at NULL→READY only                           |
| inner build  | `mutable_ready()` — **NULL/READY** before PLAYING                               | same (`mxl-domain-path`, `transport-properties`, `depay-properties` only) |


**`*-properties` (current choice, may revisit):** `mutable_ready()` only, not `mutable_playing()`. Bags apply on the **next** inner build so PLAYING-time sets are rejected. Revisit if applications need to tune them during the PLAYING wait or between real→real activation-event swaps.

**gst-launch:** sets props in NULL before READY — NULL-only pspec is enough for typical lines.

## `nmossink` properties

*Add-resource rows:* `mutable_ready`, NULL or READY. If eager registration already ran at NULL→READY, later READY-time changes have no effect. Setting `transport-file`, `transport-file-path`, or `caps` at READY (before PAUSED) triggers eager add-resource and skips deferred. Deferred add-resource is currently only implemented for `transport=mxl`. `source-ip` … `destination-port` are honoured when `transport` is `udp`, `udp2`, or `nvdsudp`; ignored on `mxl`. *Inner build rows:* `mxl-domain-path` is optional at add-resource (can populate or cross-check `mxl-domain-id` via `domain_def.json`); first required at inner build for `mxlsink domain=`.

| Property | First step |
|----------|------------|
| `daemon-uri` | open-session |
| `node-seed` | open-session |
| `http-port` | open-session |
| `host-name` | open-session |
| `domain` | open-session |
| `registration-url` | open-session |
| `system-url` | open-session |
| `auto-activate` | add-resource |
| `transport` | add-resource |
| `sender-name` | add-resource |
| `mxl-domain-id` | add-resource |
| `mxl-flow-id` | add-resource |
| `label` | add-resource |
| `description` | add-resource |
| `transport-file` | add-resource |
| `transport-file-path` | add-resource |
| `caps` | add-resource |
| `transport-caps` | add-resource |
| `source-ip` | add-resource |
| `source-port` | add-resource |
| `destination-ip` | add-resource |
| `destination-port` | add-resource |
| `mxl-domain-path` | inner build |
| `transport-properties` | inner build |
| `pay-properties` | inner build |


## `nmossrc` properties

*Open-session and add-resource rows:* no `mutable_ready()` — set in **NULL** (eager add-resource only). `mxl-domain-id` is the registration requirement (UUID, not filesystem path). `mxl-flow-id` property is ignored at activation-event (PATCH transport file wins). *Inner build rows:* `mutable_ready`, NULL or READY. `mxl-domain-path` is optional at add-resource; first required at inner build for `mxlsrc domain=`. `depay-properties` applies when `transport` is `udp` or `udp2`.

| Property | First step |
|----------|------------|
| `daemon-uri` | open-session |
| `node-seed` | open-session |
| `http-port` | open-session |
| `host-name` | open-session |
| `domain` | open-session |
| `registration-url` | open-session |
| `system-url` | open-session |
| `auto-activate` | add-resource |
| `transport` | add-resource |
| `receiver-name` | add-resource |
| `mxl-domain-id` | add-resource |
| `mxl-flow-id` | add-resource |
| `label` | add-resource |
| `description` | add-resource |
| `transport-file` | add-resource |
| `transport-file-path` | add-resource |
| `caps` | add-resource |
| `transport-caps` | add-resource |
| `receiver-caps-mode` | add-resource |
| `source-ip` | add-resource |
| `interface-ip` | add-resource |
| `multicast-ip` | add-resource |
| `destination-port` | add-resource |
| `mxl-domain-path` | inner build |
| `transport-properties` | inner build |
| `depay-properties` | inner build |


## Related tests

- Deferred sender: integration plan Tier 2 (`registration_matrix.rs`, planned).
- `channel-order` via `transport-caps`: `sdp.rs` tests on `feature/mutable-ready-and-channel-order`.

