<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# BCP-008 Sender / Receiver Status Monitoring — Design Spike

Status: **proposed** (awaiting review)  
Scope: libnvnmos C API, `nvnmos_impl.cpp`, nvnmosd gRPC, gst-nmos-rs (`nmossrc` / `nmossink`)  
Depends on: [AMWA BCP-008-01](https://specs.amwa.tv/bcp-008-01/), [BCP-008-02](https://specs.amwa.tv/bcp-008-02/), [IS-12](https://specs.amwa.tv/is-12/), [MS-05-02](https://specs.amwa.tv/ms-05-02/), nmos-cpp control-protocol support

## 1. Summary

Add **BCP-008-01 Receiver Status** and **BCP-008-02 Sender Status** to NvNmos so controllers can observe per-Sender / per-Receiver health via the standard `NcSenderMonitor` / `NcReceiverMonitor` models.

**Decision to confirm:** implement BCP-008 as a first-class feature; **bury IS-12 inside libnvnmos**. Do **not** expose a general IS-12 / MS-05 device-model API through the C layer, nvnmosd, or the GStreamer elements.

| Layer | Responsibility |
|-------|----------------|
| **libnvnmos** | Enable the IS-12 WebSocket; auto-create monitors with IS-04 touchpoints; map IS-05 activate/deactivate onto monitors; accept status updates via a narrow C API; keep transition counters, `statusReportingDelay`, overallStatus aggregation, and notifications inside nmos-cpp |
| **nvnmosd** | Thin pass-through: `ReportResourceStatus` → C API; enable control-protocol port via `NodeConfig` |
| **gst-nmos-rs** | Probe inner transport elements where honest; push status reports; document best-effort domains per transport |

**Non-goal for this work:** general IS-12 (custom workers, writable properties, IS-14 rebuild, arbitrary method dispatch). That does not fit the NvNmos "named senders/receivers + activations" model and is left for a future spike if demand appears.

## 2. Background

### 2.1 Spec relationship

BCP-008 defines *what* to monitor (`NcReceiverMonitor` / `NcSenderMonitor` and domain semantics). Controllers consume those models **only** over IS-12. There is no BCP-008 without an IS-12 control-protocol endpoint and an MS-05 device model containing the monitors.

What we can still simplify is the **application-facing surface**: apps never see OIDs, class descriptors, or Get/Set property RPCs — only "update status for this named sender/receiver".

### 2.2 What nmos-cpp already provides

The nmos-cpp-node example looks heavy because it mixes BCP-008 with demo control classes (Gain, Temperature, Example), IS-14 rebuild hooks, and simulated status churn. Stripped to BCP-008, the required pieces are:

| Piece | Role |
|-------|------|
| `control_protocol_ws_port` ≥ 0 | Enables IS-12 WS in `make_node_server` |
| `control_protocol_state` + class/datatype/method descriptor handlers | Standard MS-05 / BCP-008 class tree (nmos-cpp built-ins) |
| Root block + ClassManager + `receiver-monitors` / `sender-monitors` blocks | Device model skeleton |
| One monitor per IS-04 sender/receiver with touchpoint | BCP-008 identity mapping |
| `on_monitor_activated` | On IS-05 activation: Inactive→Healthy (immediate) then honour `statusReportingDelay` for worse states; on deactivate → Inactive |
| Application (or library) calls to `set_*_monitor_*_status` | Feed truthful domain values |
| Optional packet-counter / `ResetMonitor` handlers | Methods on the monitor; empty collections are legal when the device cannot measure |

nmos-cpp already implements `overallStatus` aggregation, transition counters, delayed reporting of healthier states, and property-changed notifications.

### 2.3 Domains (reminder)

**Receiver (`NcReceiverMonitor`):** `linkStatus`, `connectionStatus`, `externalSynchronizationStatus` (+ optional `synchronizationSourceId`), `streamStatus`, plus `overallStatus`. Methods: `GetLostPacketCounters`, `GetLatePacketCounters`, `ResetCountersAndMessages`.

**Sender (`NcSenderMonitor`):** `linkStatus`, `transmissionStatus`, `externalSynchronizationStatus` (+ optional `synchronizationSourceId`), `essenceStatus`, plus `overallStatus`. Methods: `GetTransmissionErrorCounters`, `ResetCountersAndMessages`.

BCP-008 allows empty counter collections when the device cannot measure loss/lateness.

## 3. Goals and non-goals

### Goals

1. Controllers can discover IS-12, find sender/receiver monitors by touchpoint, subscribe, and see domain statuses change with connection lifecycle and data-plane health.
2. Direct C callers and nvnmosd clients update status the same way (named resource + domain + value + message).
3. `nmossrc` / `nmossink` contribute whatever they can observe from inner elements without inventing PTP/NIC health they do not have.
4. Conformance path toward BCP-008-01 / BCP-008-02 (and the IS-12 plumbing they require), with documented fidelity gaps for early releases.

### Non-goals

- General IS-12 device control (custom `NcWorker` classes, writable app properties, IS-14 Configuration API).
- Surfacing IS-12 message types or MS-05 OIDs on the gRPC or GStreamer APIs.
- Guaranteeing full BCP-008 behavioural fidelity (true link, PTP lock, per-NIC lost/late) from GStreamer alone in v1.
- Changing the "do not modify inner transport elements" rule — probe and wrap only.

## 4. Layering

```text
Controller
  └── IS-12 WS ──► libnvnmos (nmos-cpp control_protocol_*)
                      ▲
                      │ nmos_resource_status_update(...)
                      │
              ┌───────┴────────┐
              │                │
         direct C app     nvnmosd
                              ▲
                              │ ReportResourceStatus
                              │
                         nmossrc / nmossink
                              │
                         inner mxlsrc/sink, udpsrc/sink,
                         nvdsudpsrc/sink, depayloaders, …
```

**Principle (same as IS-05 / IS-08):** control-plane complexity stays in libnvnmos; daemon is session ownership + FFI; elements own data-plane observation.

## 5. libnvnmos

### 5.1 Enablement

Add to `NvNmosNodeConfig` (zero-init = disabled, preserving today's behaviour):

| Field | Semantics |
|-------|-----------|
| `unsigned int control_protocol_ws_port` | `0` = BCP-008 / IS-12 **off**. Non-zero = listen for IS-12 on that TCP port (same pattern as nmos-cpp `control_protocol_ws_port`). |

When enabled at node create time:

1. Construct `control_protocol_state` with library-owned handlers (standard class/datatype/method descriptors; stub or library-owned packet counters — see §5.4).
2. Register `on_monitor_activated` so IS-05 activations drive monitor Inactive/Healthy transitions without an application callback.
3. Ensure device-model skeleton exists (root, class manager, `receiver-monitors` / `sender-monitors` blocks).
4. Run the control-protocol behaviour thread alongside the existing node server threads.

IS-04 Node controls / service advertisement for the control-protocol endpoint follows whatever nmos-cpp already publishes when the WS is enabled (no separate NvNmos invention).

### 5.2 Monitor lifecycle

Tied to existing add/remove:

| Event | Action |
|-------|--------|
| `add_nmos_sender_to_node_server` / receiver add | Create matching `NcSenderMonitor` / `NcReceiverMonitor` under the monitors block; touchpoint = IS-04 resource id; role derived from caller-chosen `name` (stable, unique per side) |
| remove sender/receiver | Remove the monitor object |
| IS-05 activate / deactivate | Handled by `on_monitor_activated` (nmos-cpp) — **no new application callback** |
| Application / element status update | §5.3 |

Initial domain values before first activation: Inactive (or NotUsed for sync) as appropriate. Link status has no Inactive — default **AllUp** until a reporter says otherwise (honest "we have no NIC sensor" is better documented than inventing AllDown).

**Link-status probing note:** this may be supportable without a new resource-to-interface mapping. libnvnmos already resolves each RTP Sender's `source_ip` and each Receiver's `interface_ip` through nmos-cpp's host-interface list to build IS-04 `interface_bindings`; gst-nmos-rs also resolves interface IP addresses locally when building the data path and `a=x-nvnmos-iface`. A link-status implementation could reuse those resolved bindings, inspect every interface associated with the resource, and report AllUp / SomeDown / AllDown. The probing location still needs a decision: libnvnmos can only see interfaces in the nvnmosd process's network namespace, while gst-nmos-rs may be colocated with the data-plane interfaces. Avoid two independent reporters for the same domain; prefer the data-plane process when the namespaces differ, with library-side probing as a possible direct-C / colocated fallback.

### 5.3 Status update API (sketch)

Push model, keyed like the rest of NvNmos (`NvNmosSide` + caller-chosen `name`):

```c
typedef enum _NvNmosStatusDomain {
    NVNMOS_STATUS_DOMAIN_LINK = 0,
    NVNMOS_STATUS_DOMAIN_CONNECTION,       /* receiver: connectionStatus */
    NVNMOS_STATUS_DOMAIN_TRANSMISSION,     /* sender: transmissionStatus */
    NVNMOS_STATUS_DOMAIN_EXTERNAL_SYNC,
    NVNMOS_STATUS_DOMAIN_STREAM,           /* receiver: streamStatus */
    NVNMOS_STATUS_DOMAIN_ESSENCE,          /* sender: essenceStatus */
} NvNmosStatusDomain;

/* Domain-specific enums mirror Nc*Status integer values from the feature set.
   Callers use the enum matching the domain; mismatched side/domain is an error. */

bool nmos_resource_status_update(
    NvNmosNodeServer *server,
    NvNmosSide side,
    const char *name,
    NvNmosStatusDomain domain,
    int status,                 /* Nc*Status value for that domain */
    const char *status_message  /* may be NULL */
);

/* Optional companion for synchronizationSourceId (nullable string). */
bool nmos_resource_sync_source_id_update(
    NvNmosNodeServer *server,
    NvNmosSide side,
    const char *name,
    const char *synchronization_source_id  /* NULL clears */
);
```

Implementation maps to nmos-cpp `set_receiver_monitor_*` / `set_sender_monitor_*` helpers (which already apply delay / transition-counter rules). overallStatus is **not** set by the caller — nmos-cpp derives it.

Idempotent updates (same status + message) should be cheap / no-ops so elements can poll.

### 5.4 Counters and ResetCountersAndMessages

**v1 recommendation:** implement Get*Counters methods returning **empty collections**; ResetCountersAndMessages resets transition counters and status messages via nmos-cpp's existing path, and is a no-op for packet counters.

Rationale: BCP-008 explicitly allows empty collections when the device cannot measure; honest emptiness beats fake zeros. A later revision can add optional C callbacks (`get_lost_packet_counters`, etc.) or daemon-held counters if Rivermax / `rtpjitterbuffer` telemetry lands.

### 5.5 Why not a general IS-12 C API

NvNmos deliberately exposes a closed surface (senders, receivers, channel mappings, activations). A general Get/Set/Invoke API would:

- Force apps to understand OIDs, class ids, and MS-05 typing.
- Duplicate what controllers already do over IS-12.
- Still not cover IS-14 / custom workers without dragging the nmos-cpp-node example's complexity into every layer.

BCP-008 is the high-value feature set that *does* map cleanly onto named resources.

## 6. nvnmosd

### 6.1 NodeConfig

```protobuf
message NodeConfig {
  // ... existing fields ...
  // TCP port for IS-12 Control Protocol WS. 0 = disabled (default).
  // Honoured only when this RPC creates the Node.
  uint32 control_protocol_ws_port = 9;
}
```

Optional later: allocate from an env range (like `http_port == 0`) if deployments want auto-pick; not required for the spike.

### 6.2 ReportResourceStatus

```protobuf
rpc ReportResourceStatus(ReportResourceStatusRequest) returns (Empty);

message ReportResourceStatusRequest {
  string session_handle = 1;
  string resource_handle = 2;
  StatusDomain domain = 3;
  int32 status = 4;                 // Nc*Status value
  optional string status_message = 5;
  // When domain == EXTERNAL_SYNC, optional sync source id update
  // may be carried in a oneof or sibling field — exact shape TBD in impl.
}

enum StatusDomain {
  STATUS_DOMAIN_LINK = 0;
  STATUS_DOMAIN_CONNECTION = 1;
  STATUS_DOMAIN_TRANSMISSION = 2;
  STATUS_DOMAIN_EXTERNAL_SYNC = 3;
  STATUS_DOMAIN_STREAM = 4;
  STATUS_DOMAIN_ESSENCE = 5;
}
```

Ownership rules match `SyncResourceState`: only the session that added the resource may report. Errors: `NOT_FOUND`, `PERMISSION_DENIED`, `FAILED_PRECONDITION` if IS-12 was not enabled on the Node.

No reverse stream for ResetMonitor / GetCounters in v1.

### 6.3 What nvnmosd does *not* do

- No IS-12 message proxy.
- No interpretation of status semantics (no "derive overallStatus").
- No GStreamer or NIC polling inside the daemon.

## 7. gst-nmos-rs (`nmossrc` / `nmossink`)

### 7.1 Role

When the Node has IS-12 enabled (discoverable from open-session / node create — exact signal TBD: element property `report-status=true` defaulting on if the Node supports it, or always attempt and ignore `FAILED_PRECONDITION`):

1. On activation / deactivation paths already owned by the element, rely primarily on libnvnmos's `on_monitor_activated` for Inactive↔Healthy; optionally reinforce connection/transmission/stream/essence from local observation.
2. Periodically or on bus messages, map observed health → `ReportResourceStatus`.

### 7.2 Probing matrix (v1 honesty)

| Domain | `mxl` | `udp` / `udp2` | `nvdsudp` | Notes |
|--------|-------|----------------|-----------|-------|
| connection / transmission | Buffer flow to/from `mxlsink`/`mxlsrc`; ERROR | Pad activity / ERROR; optional future `rtpjitterbuffer` stats | Pad activity / ERROR; Rivermax stats if exposed later | Core v1 value |
| stream / essence | Caps / grain read success | Depayloader ERROR / unexpected EOS | Same + nvdsudp ERROR | Medium confidence |
| link | Not observed | Weak: optional iface operstate if `interface_ip` known | Same | Default AllUp; document gap |
| external sync | NotUsed or NotUsed unless MXL clock API exists | NotUsed | NotUsed unless true PTP lock API is available (`ptp-src` alone is insufficient) | Prefer NotUsed over lying Healthy |
| lost / late / tx error counters | Empty | Empty unless jitterbuffer added | Empty unless Rivermax counters plumbed | See `inner.rs` note: jitterbuffer omitted until a status surface justifies latency |

ST 2022-7: connection PartiallyHealthy when one leg recovers traffic and the other does not is desirable later; v1 may treat dual-leg as a single connection domain until leg-level sensors exist.

### 7.3 Element surface (sketch)

- No new user-facing "IS-12" properties.
- Optional: `status-report-interval-ms` (0 = event-driven only).
- Do **not** put design vocabulary ("open-session", "BCP-008") in gst-inspect blurbs beyond a brief "reports sender/receiver status to the NMOS daemon when enabled".

### 7.4 Interaction with existing `rtpjitterbuffer` note

`inner.rs` already records that jitterbuffer / `rtp-stats` should land **alongside** a telemetry surface. BCP-008 `ReportResourceStatus` (and optional future counters) **is** that surface. Treat optional jitterbuffer as a follow-on under this design, not a prerequisite for enabling monitors.

## 8. Phased delivery plan

Phasing is for scheduling and review — not something to encode in source comments.

### Phase A — libnvnmos BCP-008 core

- Enable IS-12 via `control_protocol_ws_port`.
- Auto monitors on add/remove; touchpoints; `on_monitor_activated`.
- `nmos_resource_status_update` (+ sync source id helper).
- Empty counter methods; ResetCountersAndMessages via nmos-cpp.
- Unit / integration tests against nmos-cpp helpers; manual IS-12 client or AMWA test suite smoke where practical.

### Phase B — nvnmosd pass-through

- `NodeConfig.control_protocol_ws_port`.
- `ReportResourceStatus` RPC + ownership checks.
- Daemon tests: report after add; reject wrong session; reject when IS-12 off.

### Phase C — gst-nmos-rs observation

- Map activation-adjacent and bus/ERROR / buffer-idle heuristics to connection|transmission and stream|essence.
- Leave link AllUp and sync NotUsed unless a real sensor is wired.
- Document per-transport fidelity in element README / design appendix.

### Phase D — fidelity upgrades (optional, separate approvals)

- NIC operstate → linkStatus (possibly from daemon host netns or from the element process).
- PTP lock / `synchronizationSourceId` from Rivermax or system clock APIs.
- Optional `rtpjitterbuffer` + lost/late counters; or nvdsudp / Rivermax counters.
- ST 2022-7 leg-aware PartiallyHealthy.

## 9. Testing strategy

| Layer | Approach |
|-------|----------|
| libnvnmos | Create node with control-protocol port; add sender/receiver; assert monitor objects + touchpoints; drive status updates; activate/deactivate and assert domain Inactive/Healthy behaviour (incl. delay where testable) |
| nvnmosd | RPC round-trips; session ownership; disabled-IS-12 errors |
| gst-nmos-rs | Integration: force inner ERROR or starve buffers → Unhealthy connection/stream report; activation path does not double-fight library activate handler |
| Conformance | Track BCP-008-01 / BCP-008-02 (and IS-12) sheets used by nmos-cpp; note known gaps (counters empty, sync NotUsed, link always AllUp) so JT-NM / AMWA results are interpreted correctly |

## 10. Open questions for review

1. **Enablement default:** opt-in via non-zero `control_protocol_ws_port` only (proposed), or also an explicit bool with auto-allocated WS port?
2. **Sync domain default:** `NotUsed` when no sensor (proposed) vs always `Healthy` when SDP advertises `ts-refclk:ptp`?
3. **Link domain default:** leave `AllUp` with no reporter (proposed) vs require an explicit first report?
4. **Counters in v1:** empty collections only (proposed) vs invent a thin callback now?
5. **Element reporting when IS-12 off:** silent no-op vs property to disable attempts?
6. **Port collision:** reuse pattern of rejecting create when WS port conflicts with HTTP / events WS (nmos-cpp already rejects events==control-protocol same port)?
7. **DeepStream / in-process C path:** Phase A alone is enough for `nvnmos-example` and any Rivermax app; confirm whether Jonathan wants Phase C (GStreamer) in the same release train or after.

## 11. Appendix — rejected alternatives

| Alternative | Why rejected |
|-------------|--------------|
| Full IS-12 façade on gRPC / C | Wrong abstraction; huge surface; apps already use IS-12 as controllers |
| Status only via IS-07 events | Does not satisfy BCP-008; different feature set |
| Derive all status inside libnvnmos with no app input | Library has no data plane; would only ever reflect IS-05 active/inactive |
| Require `rtpjitterbuffer` before enabling BCP-008 | Blocks useful activation/ERROR-based status; counters are optional in the BCP |

## 12. Appendix — nmos-cpp-node mapping (what we keep / drop)

| Example node piece | NvNmos plan |
|--------------------|-------------|
| control_protocol_ws + descriptor handlers | Keep (library-owned) |
| monitor blocks + touchpoints | Keep (auto on add) |
| `on_monitor_activated` | Keep |
| `set_*_monitor_*_status` from app | Keep as `nmos_resource_status_update` |
| Gain / Temperature / Example / sender-control | Drop |
| IS-14 fingerprint / create/remove object | Drop |
| `simulate_status_monitor_activity` | Drop |
| NIC packet counter demo handlers | Drop for v1 (empty collections) |
