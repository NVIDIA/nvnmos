<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# BCP-008 Sender / Receiver Status Monitoring — Design Spike

Status: **proposed** (awaiting review)  
Scope: libnvnmos C API, `nvnmos_impl.cpp`, nvnmosd gRPC, gst-nmos-rs (`nmossrc` / `nmossink`)  
Depends on: [AMWA BCP-008-01](https://specs.amwa.tv/bcp-008-01/), [BCP-008-02](https://specs.amwa.tv/bcp-008-02/), [IS-12](https://specs.amwa.tv/is-12/), [MS-05-02](https://specs.amwa.tv/ms-05-02/), nmos-cpp control-protocol support as of `nmos-cpp/cci.20261005` (the version nvnmos pins), which includes [sony/nmos-cpp#511](https://github.com/sony/nmos-cpp/pull/511) and [sony/nmos-cpp#527](https://github.com/sony/nmos-cpp/pull/527)

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
| `experimental::control_protocol_state` | Holds the standard MS-05 / BCP-008 class and datatype descriptors (nmos-cpp built-ins) and the monitor domain profiles. Its constructor takes the optional `get_lost_packet_counters`, `get_late_packet_counters` and `reset_monitor` handlers |
| `make_get_control_protocol_{class,datatype,method}_descriptor_handler(control_protocol_state)` | Descriptor handlers set on `node_implementation` |
| `control_protocol_behaviour_thread(node_model, control_protocol_state, gate)` | Applies pending (delayed) status changes; runs alongside the node server threads |
| Root block + ClassManager + `receiver-monitors` / `sender-monitors` blocks | Device model skeleton |
| `make_receiver_monitor` / `make_sender_monitor` with an NMOS touchpoint | One monitor per IS-04 Sender/Receiver (BCP-008 identity mapping). Defaults: overall, connection, transmission, stream and essence Inactive; link AllUp; external sync NotUsed; `statusReportingDelay` 3 s; `autoResetCountersAndMessages` true |
| `on_monitor_activated(make_monitor_connection_activation_handler(...))` | On IS-05 activation: Inactive→Healthy (immediate) then honour `statusReportingDelay` for worse states; on deactivate → Inactive |
| `make_set_receiver_monitor_*_handler` / `make_set_sender_monitor_*_handler` | Per-domain setters keyed by monitor oid, including `synchronizationSourceId`. They wrap `nc::set_*_monitor_*_status`, which also needs the pending, class-descriptor and monitor-domain handlers |
| `on_get_lost_packet_counters`, `on_get_late_packet_counters`, `on_reset_monitor` | Optional packet-counter / `ResetCountersAndMessages` hooks; empty collections are legal when the device cannot measure |

nmos-cpp already implements `overallStatus` aggregation, transition counters, delayed reporting of healthier states, and property-changed notifications.

Since sony/nmos-cpp#511, the domains of a monitor class are data, not code. `control_protocol_state::insert_monitor_domains(class_id, domains)` adds the domains for a class derived from `NcReceiverMonitor` or `NcSenderMonitor`. `nc::set_monitor_status` sets any of those domains with the same delay and transition-counter rules. The built-in domains above do not need this.

### 2.3 What nvnmos does today

libnvnmos calls `make_node_server(node_model, node_implementation, log_model, gate)` with no `control_protocol_state`, and sets `control_protocol_ws_port` to `-1` in `src/nvnmos.cpp` so that nmos-cpp does not mount IS-12. `rust/nvnmosd/tests/advertised_apis.rs` asserts that `-1`.

### 2.4 Domains (reminder)

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

1. Pass the configured port instead of `-1` for `control_protocol_ws_port` (§2.3).
2. Construct `control_protocol_state` with library-owned packet-counter and reset handlers (§5.4), and set the descriptor handlers on `node_implementation`.
3. Register `on_monitor_activated(make_monitor_connection_activation_handler(...))` so IS-05 activations drive monitor Inactive/Healthy transitions without an application callback.
4. Ensure device-model skeleton exists (root, class manager, `receiver-monitors` / `sender-monitors` blocks).
5. Run `control_protocol_behaviour_thread` alongside the existing node server threads.

IS-04 Node controls / service advertisement for the control-protocol endpoint follows whatever nmos-cpp already publishes when the WS is enabled (no separate NvNmos invention).

### 5.2 Monitor lifecycle

Tied to existing add/remove:

| Event | Action |
|-------|--------|
| `add_nmos_sender_to_node_server` / `add_nmos_receiver_to_node_server` | Create matching `NcSenderMonitor` / `NcReceiverMonitor` with `make_sender_monitor` / `make_receiver_monitor` under the monitors block; touchpoint = IS-04 resource id; role derived from caller-chosen `name` (stable, unique per side) |
| remove sender/receiver | Remove the monitor object |
| IS-05 activate / deactivate | Handled by `on_monitor_activated` (nmos-cpp) — **no new application callback** |
| Application / element status update | §5.3 |

Initial domain values before first activation are the nmos-cpp defaults (§2.2): Inactive, except external sync, which is **NotUsed** until a reporter has a real sensor. Link status has no Inactive. It defaults to **AllUp** until the library probe below or a reporter says otherwise.

**Link-status probe in libnvnmos.** libnvnmos already resolves the interface name of each leg of an RTP Sender (from `source_ip`) or Receiver (from `interface_ip`) through nmos-cpp's host-interface list to build IS-04 `interface_bindings`. libnvnmos periodically reads the operational state of those interfaces and sets the monitor's link status to AllUp, SomeDown or AllDown, with a message naming the interfaces that are down. MXL Senders and Receivers have no interface bindings, so their link status stays AllUp.

The probe sees the interfaces in the network namespace of the process that loads libnvnmos. When the data plane runs elsewhere, its process may see different interface state. The first link-status report through §5.3 for a resource therefore stops the library probe for that resource, so each domain has only one reporter.

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

libnvnmos keeps the monitor oid for each caller-chosen `name` and side, and calls the matching `make_set_receiver_monitor_*_handler` / `make_set_sender_monitor_*_handler` setter (§2.2), which applies the delay and transition-counter rules. overallStatus is **not** set by the caller — nmos-cpp derives it.

Idempotent updates (same status + message) should be cheap / no-ops so elements can poll.

### 5.4 Counters and ResetCountersAndMessages

**v1 recommendation:** implement Get*Counters methods returning **empty collections**; ResetCountersAndMessages resets transition counters and status messages via nmos-cpp's existing path, and is a no-op for packet counters.

Rationale: BCP-008 explicitly allows empty collections when the device cannot measure; honest emptiness beats fake zeros.

**When counters are added**, the application pushes them, in the same way as status:

```c
bool nmos_resource_counters_update(
    NvNmosNodeServer *server,
    NvNmosSide side,
    const char *name,
    const NvNmosCounter *counters,  /* name, value, description */
    unsigned int num_counters
);
```

libnvnmos stores the latest values for each monitor. The `get_packet_counters_handler` functions that libnvnmos gives nmos-cpp return that stored copy, so a controller's Get*Counters call never waits on the application. The values are as fresh as the last push, which is adequate at a report interval of about a second.

libnvnmos applies ResetCountersAndMessages itself through `reset_monitor_handler`: it records the stored values at reset time and reports later pushes minus those values. The application only ever counts upwards, and has no reset callback.

The push model also suits nvnmosd: a pull would arrive on an nmos-cpp thread and would need a reverse stream to the session to answer it (§6.2). Possible sources are Rivermax statistics or the `rtpjitterbuffer` discussed in §7.4.

nmos-cpp wires the Sender's GetTransmissionErrorCounters to the same `get_lost_packet_counters` handler as the Receiver's GetLostPacketCounters. The handler receives the monitor resource, so libnvnmos can tell the two apart.

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
  // ... existing fields 1-8 ...
  // TCP port for IS-12 Control Protocol WS. 0 = disabled (default).
  // Honoured only when this call creates the Node; ignored when attaching
  // to an existing Node (same `seed`).
  uint32 control_protocol_ws_port = 9;
}
```

`NodeConfig` is used by both `AddNode` (persistent Nodes) and `OpenSession` (session-refcounted Nodes), so both creation paths get the field.

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
  STATUS_DOMAIN_UNSPECIFIED = 0;
  STATUS_DOMAIN_LINK = 1;
  STATUS_DOMAIN_CONNECTION = 2;
  STATUS_DOMAIN_TRANSMISSION = 3;
  STATUS_DOMAIN_EXTERNAL_SYNC = 4;
  STATUS_DOMAIN_STREAM = 5;
  STATUS_DOMAIN_ESSENCE = 6;
}
```

`STATUS_DOMAIN_UNSPECIFIED` is the proto3 zero value and is an error from a client, like `TRANSPORT_UNSPECIFIED` and `SIDE_UNSPECIFIED`.

Ownership rules match `SyncResourceState`: only the session that added the resource may report. Status codes follow the service conventions in `nvnmosd.proto`:

- `NOT_FOUND`: the session is unknown or does not own `resource_handle`.
- `FAILED_PRECONDITION`: IS-12 is not enabled on the Node, as `UpdateAnnotation` does when the Annotation API is off.
- `INVALID_ARGUMENT`: `STATUS_DOMAIN_UNSPECIFIED`, a domain that does not belong to the resource's side, or a status value outside that domain's enum.
- `UNKNOWN`: libnvnmos rejects the update for another reason.

There is no acknowledgement, like `UpdateAnnotation`. There is no reverse stream for ResetCountersAndMessages or Get*Counters: libnvnmos answers both from its own state (§5.4). When counters are added, a sibling RPC carries `nmos_resource_counters_update`.

### 6.3 What nvnmosd does *not* do

- No IS-12 message proxy.
- No interpretation of status semantics (no "derive overallStatus").
- No GStreamer or NIC polling inside the daemon.

## 7. gst-nmos-rs (`nmossrc` / `nmossink`)

### 7.1 Role

The element always attempts to report. The first `FAILED_PRECONDITION` from `ReportResourceStatus` means IS-12 is off on the Node. The element then stops reporting for that resource, logging it at debug level only, and posts no warning or error. There is no element property for this.

When IS-12 is enabled:

1. On activation / deactivation paths already owned by the element, rely primarily on libnvnmos's `on_monitor_activated` for Inactive↔Healthy; optionally reinforce connection/transmission/stream/essence from local observation.
2. Periodically or on bus messages, map observed health → `ReportResourceStatus`.

### 7.2 Probing matrix (v1 honesty)

| Domain | `mxl` | `udp` / `udp2` | `nvdsudp` | Notes |
|--------|-------|----------------|-----------|-------|
| connection / transmission | Buffer flow to/from `mxlsink`/`mxlsrc`; ERROR | Pad activity / ERROR; optional future `rtpjitterbuffer` stats | Pad activity / ERROR; Rivermax stats if exposed later | Core v1 value |
| stream / essence | Caps / grain read success | Depayloader ERROR / unexpected EOS | Same + nvdsudp ERROR | Medium confidence |
| link | No network interface; stays AllUp | Optional: operstate of the interfaces the element resolved for `multicast-iface` / `a=x-nvnmos-iface` | Same | libnvnmos probes by default (§5.2); an element report replaces that probe for the resource |
| external sync | NotUsed unless MXL clock API exists | NotUsed | NotUsed unless true PTP lock API is available (`ptp-src` alone is insufficient) | Prefer NotUsed over lying Healthy |
| lost / late / tx error counters | Empty | Empty unless jitterbuffer added | Empty unless Rivermax counters plumbed | See `inner.rs` note: jitterbuffer omitted until a status surface justifies latency. When added, pushed as in §5.4 |

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

- Enable IS-12 via `control_protocol_ws_port`, replacing the unconditional `-1` (§2.3); `control_protocol_state`, descriptor handlers and `control_protocol_behaviour_thread`.
- Auto monitors on add/remove; touchpoints; `on_monitor_activated`.
- `nmos_resource_status_update` (+ sync source id helper).
- Link-status operstate probe for RTP resources (§5.2).
- Empty counter methods; ResetCountersAndMessages via nmos-cpp.
- Unit / integration tests against nmos-cpp helpers; manual IS-12 client or AMWA test suite smoke where practical.

### Phase B — nvnmosd pass-through

- `NodeConfig.control_protocol_ws_port`, for `AddNode` and `OpenSession`.
- `ReportResourceStatus` RPC + ownership checks.
- Daemon tests: report after add; `NOT_FOUND` for the wrong session; `FAILED_PRECONDITION` when IS-12 off. `advertised_apis.rs` still asserts `-1` when the port is unset, and checks the port when it is set.

### Phase C — gst-nmos-rs observation

- Map activation-adjacent and bus/ERROR / buffer-idle heuristics to connection|transmission and stream|essence.
- Leave link AllUp and sync NotUsed unless a real sensor is wired.
- Document per-transport fidelity in element README / design appendix.

### Phase D — fidelity upgrades (optional, separate approvals)

- NIC operstate → linkStatus from the element process, for deployments where the data plane is in a different network namespace from libnvnmos.
- PTP lock / `synchronizationSourceId` from Rivermax or system clock APIs.
- Optional `rtpjitterbuffer` + lost/late counters; or nvdsudp / Rivermax counters; pushed through `nmos_resource_counters_update` (§5.4).
- ST 2022-7 leg-aware PartiallyHealthy.
- MXL-specific monitor classes using `insert_monitor_domains` (§2.2). Jonathan's unmerged `mxl-status-monitors` branch of nmos-cpp has example `NvMXLReceiverMonitor` / `NvMXLSenderMonitor` classes, with ring/publish, grain, flow and domain status domains.

## 9. Testing strategy

| Layer | Approach |
|-------|----------|
| libnvnmos | Create node with control-protocol port; add sender/receiver; assert monitor objects + touchpoints; drive status updates; activate/deactivate and assert domain Inactive/Healthy behaviour (incl. delay where testable) |
| nvnmosd | RPC round-trips; session ownership (`NOT_FOUND`); disabled-IS-12 errors (`FAILED_PRECONDITION`) |
| gst-nmos-rs | Integration: force inner ERROR or starve buffers → Unhealthy connection/stream report; activation path does not double-fight library activate handler |
| Conformance | Track BCP-008-01 / BCP-008-02 (and IS-12) sheets used by nmos-cpp; note known gaps (counters empty, sync NotUsed, link reflects only the libnvnmos process's view of RTP interfaces and is always AllUp for MXL) so JT-NM / AMWA results are interpreted correctly |

## 10. Open questions for review

1. **Enablement default:** opt-in via non-zero `control_protocol_ws_port` only (proposed), or also an explicit bool with auto-allocated WS port?
2. **Sync domain default:** resolved: `NotUsed` when there is no sensor, even when the SDP advertises `ts-refclk:ptp` (§5.2).
3. **Link domain default:** resolved: `AllUp`, with a libnvnmos operstate probe for RTP resources that a data-plane report replaces (§5.2).
4. **Counters in v1:** empty collections only (proposed) vs application-pushed counters (§5.4) now? Resolved: when counters are added, the application pushes them; libnvnmos does not call back into the application.
5. **Element reporting when IS-12 off:** resolved: silent no-op after the first `FAILED_PRECONDITION`, with no element property (§7.1).
6. **Port collision:** resolved: rely on nmos-cpp's existing check (events WS and control-protocol WS on the same port) and on the bind failure; libnvnmos adds no check of its own.
7. **DeepStream / in-process C path:** Phase A alone is enough for `nvnmos-example` and any Rivermax app; confirm whether Jonathan wants Phase C (GStreamer) in the same release train or after.

## 11. Appendix — rejected alternatives

| Alternative | Why rejected |
|-------------|--------------|
| Full IS-12 façade on gRPC / C | Wrong abstraction; huge surface; apps already use IS-12 as controllers |
| Status only via IS-07 events | Does not satisfy BCP-008; different feature set |
| Derive all status inside libnvnmos with no app input | Library has no data plane; would only ever reflect IS-05 active/inactive |
| Require `rtpjitterbuffer` before enabling BCP-008 | Blocks useful activation/ERROR-based status; counters are optional in the BCP |
| Counters fetched by a callback into the application | Through nvnmosd, needs a reverse stream and a wait inside the IS-12 method call; the application would also have to handle reset (§5.4) |

## 12. Appendix — nmos-cpp-node mapping (what we keep / drop)

| Example node piece | NvNmos plan |
|--------------------|-------------|
| control_protocol_ws + descriptor handlers | Keep (library-owned) |
| monitor blocks + touchpoints | Keep (auto on add) |
| `on_monitor_activated` | Keep |
| `make_set_*_monitor_*_handler` setters called by the app | Keep, behind `nmos_resource_status_update` |
| Custom monitor classes and domains (`insert_monitor_domains`) | Not in v1; possible MXL monitors in Phase D |
| Gain / Temperature / Example / sender-control | Drop |
| IS-14 fingerprint / create/remove object | Drop |
| `simulate_status_monitor_activity` | Drop |
| NIC packet counter demo handlers | Drop for v1 (empty collections) |
