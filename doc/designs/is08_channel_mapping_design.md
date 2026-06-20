<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# IS-08 Channel Mapping — Design

Status: proposed design  
Scope: NvNmos C/C++ API, nvnmosd gRPC daemon, gst-nmos-rs GStreamer implementation

## 1. Summary

Implement IS-08 channel mapping as a **thin wrap of nmos-cpp** in NvNmos and nvnmosd, with a **standalone GStreamer element** (`nmosaudiochannelmap`) as the primary data-plane implementation. The element exposes topology via **per-pad properties** and initial **active map** via per-src **`active-map`** `GstStructure` values.

Endpoint-local mapping inside `nmossrc` / `nmossink` remains documented backup only (Appendix A). The aggregation spike (§10) validated **`audiomixer` + per-pad `converter-config` mix-matrix** for channel concat (T9).

Control-plane limitations must not leak downward. NvNmos and nvnmosd support arbitrary Input/Output counts from day one. **`AddChannelMapping` creates real IS-08 resources**; **`SyncChannelMappingState` publishes active map only**.

**Naming (see §5.0):** libnvnmos uses **`channelmapping`** in function names (like nmos-cpp); public C **types** use **`ChannelMapping`** camel case; gRPC uses **`ChannelMapping`** CamelCase. Reserve **“active map”** for `/map/active` — avoid “channel map” elsewhere.

## 2. Layering

**Principle:** wrap nmos-cpp with as little extra logic as possible; use GStreamer pad properties and `GstStructure` for a consistent element API (same idiom as `compositor` / `audiomixer`).

**Layer boundaries (IS-04 linkage for channelmapping):** each layer speaks its own vocabulary. Do not leak daemon/GStreamer conveniences into libnvnmos.

| Layer | IS-04 parent / source linkage | `name` |
|-------|------------------------------|-----------------|
| **libnvnmos (C)** | Caller-chosen **`parent_name`** / **`sender_name`** → IS-04 UUIDs from Node **seed** (deterministic identity; same scheme as senders/receivers). Requires explicit non-empty IS-08 input/output **slugs** and channel-mapping **`name`**. | `name` (settings key; not an IS-08 REST resource id) |
| **nvnmosd (gRPC)** | Forwards names unchanged. Assigns default IS-08 **slugs** when proto **`id`** is empty (`input0`, `{name}_input0`, …); default **`routable_inputs`** to this request's input slugs; returns effective values in the add response. | `name` in request |
| **gst-nmos-rs** | Pad props `receiver-name`, `sender-name` → forwarded as proto names | `channelmapping-name` |

**Why UUID derivation is libnvnmos but slug defaulting is nvnmosd:** `parent_name` / `sender_name` → IS-04 UUID is **deterministic identity** from `(Node seed, name)` — the same class of computation libnvnmos already performs for senders/receivers, and direct C callers need it without a daemon. Empty IS-08 **slug** defaulting is different: a **client convenience** for gRPC/GStreamer (omit pad `input-id`, get `input0` back in the response). Direct C callers supply explicit slugs, analogous to embedding a name in a transport file rather than expecting libnvnmos to invent one.

libnvnmos performs name→UUID translation using the Node seed already held in `node_model.settings`. **`AddReceiver` / `AddSender` need not have run first** — deterministic ids may reference IS-04 resources not yet registered (same as senders/receivers).

```text
NvNmos (C/C++)
  Thin wrap of nmos-cpp make_channelmapping_* + channelmapping_resources
  add (create I/O), activate (data-plane → `/map/active`), activation callback (controller → data plane)

nvnmosd (gRPC)
  AddChannelMapping, RemoveChannelMapping, SyncChannelMappingState (active map only),
  SubscribeChannelMappingActivations (one stream per session), AckChannelMappingActivation
  Session ownership, GC — same patterns as Senders/Receivers / IS-05

gst-nmos-rs (GStreamer)
  Primary:  nmosaudiochannelmap  (per-pad props, IS-08 block in pipeline)
  Backup:   nmossrc / nmossink enable-channel-mapping (1 Input, 1 Output)
```

Three control-plane paths:

| Change | Mechanism |
|--------|-----------|
| Create I/O (new channel mapping) | `AddChannelMapping` — `make_channelmapping_*` + `insert_resource` (like `AddSender` / `AddReceiver`) |
| I/O churn (pad count, channel counts, metadata) | `RemoveChannelMapping` + `AddChannelMapping` — geometry fixed at create |
| Controller activation | IS-08 `POST /map/activations` → callback → `audiomixmatrix` → **ack** (no sync) |
| Data-plane → model active map publish | `SyncChannelMappingState` — **active map only**; does **not** invoke activation callback |

## 3. Primary data plane: `nmosaudiochannelmap`

### 3.1 Role

A pipeline block that owns one **channel mapping** (a set of IS-08 Inputs/Outputs under one `name` / `channelmapping-name`) and sits between audio streams:

```text
nmossrc rx_a ──┐
               ├── nmosaudiochannelmap ──► nmossink tx_x
nmossrc rx_b ──┘                         └─► nmossink tx_y
```

Each **sink pad** = one IS-08 Input. Each **src pad** = one IS-08 Output.

### 3.2 Internal structure

```text
sink_0 (N ch) ──┐
sink_1 (M ch) ──┼──► audiomixer (T = N+M+… ch logical concat)
                │         │  per-pad converter-config mix-matrix
                │         │  (disjoint placement → add == concat)
                │         ├── audiomixmatrix ──► src_0 (P ch)
                │         └── audiomixmatrix ──► src_1 (Q ch)
```

- **Aggregation:** stock **`audiomixer`** with **`converter-config`** on each sink pad (§10.1). After caps are fixed, total logical channels `T` is known; each pad’s mix-matrix maps its channels into **disjoint** output slots (zeros elsewhere). Non-overlapping placement makes the mixer’s **add** step equivalent to **concat**. Inactive/muted pads contribute silence (T9).
- **One `audiomixmatrix` per src pad**: IS-08 routing on the `T`-channel logical buffer; independent downstream caps negotiation.
- v1 matrix profile: coefficients **0.0 or 1.0** only; **one input channel per output channel** (no fan-in summing). **Duplication** of one input channel to many outputs is supported.

### 3.2.1 Topology choice: agg + matrix vs per-src mixer

Two viable internal shapes (R = sink pads, S = src pads, T = Σ Nᵢ). Both satisfy v1 IS-08; spikes T8/T9 (Plan A) and T10 (Plan B) on GStreamer 1.24.2.

```text
Plan A (v1):  1 audiomixer (concat) ──tee──► S × audiomixmatrix ──► src_* 
Plan B (alt): R × tee ──► S × audiomixer (IS-08 map on pad matrices) ──► src_*
```

See Appendix B for Plan B detail and T10 results.

| Dimension | **Plan A:** 1 mixer + S `audiomixmatrix` | **Plan B:** R tee + S `audiomixer` |
|-----------|------------------------------------------|-------------------------------------|
| Data-plane model | Concat → logical T-vector → map (§7.2) | Map directly on each output mixer’s sink pads |
| Elements | 1 GstAggregator + S matrix + tee | R tees + S GstAggregators (+ queues) |
| Matrix count / size | R matrices (T×Nᵢ) + S (Mⱼ×T); 2×2→2×2 stereo: **32** coeffs | R×S matrices (Mⱼ×Nᵢ); same case: **16** coeffs |
| Per-output activation | Update **1** `audiomixmatrix` | Update **R** pad matrices on that output’s mixer |
| Inactive input | **One** mute on central mixer pad | Mute upstream of tee, or **S** pad mutes |
| Caps | Negotiate wide **T-ch** mixer src (+ `channel-mask`) | Each output negotiates **Mⱼ** on its mixer |
| Scales better when… | **S large** (many outputs): light matrices vs S aggregators | **T large, S small**: pad matrices avoid M×T |
| Code / debug | Two matrix builders; clear concat vs map split | One matrix path; busier internal bin |

**Plan A strengths:** separation of aggregation (T9) and routing (T8); one silence point per inactive Input; cheaper per-output work when S is large; matches logical channel indexing in §7.2.

**Plan B strengths:** IS-08 active map binds 1:1 onto pad matrices; often fewer coefficients; no wide T-channel intermediate; per-output caps independent (T10 validated cross-map, duplicate, stall).

**v1 decision:** **Plan A.** Prefer one aggregation point + S lightweight matrices unless implementation of the T-wide buffer or dual matrix builder proves painful — then revisit Plan B (Appendix B).

### 3.3 Aggregation requirements

| Condition on sink pad owning channel `i` | Behaviour |
|--------------------------------------------|-----------|
| Linked, PLAYING, buffers flowing | Use sample data |
| Linked, not yet PLAYING / no buffers yet | Silence (zeros) for that slice |
| IS-05 receiver deactivated / fake chain | Silence for that slice (mute mixer pad or upstream silence) |
| Pad unlinked | Slice treated as silence |
| Partial activation: active map uses only live Input B | Outputs from B flow; slices for A are silence |

**Must not:** block all src pads because one sink pad has no data.

### 3.4 IS-08 advertisement

- One Input per sink pad; one Output per src pad.
- **I/O id defaulting** (when pad **`input-id`** / **`output-id`** or proto **`id`** is empty): **nvnmosd** assigns before calling libnvnmos (§6.10). Direct C callers must supply explicit slugs. **First** channel mapping on the Node → `input0`, …; **later** → `{name}_input0`, … Return effective slugs in the add response (§6.2).
- **Parent association:** pad **`receiver-name`** / **`sender-name`** are forwarded through nvnmosd/proto/C as **`parent_name`** / **`sender_name`**; libnvnmos derives IS-04 UUIDs from the Node seed at add time (§2, §6.2).
- **`AddChannelMapping`** creates real Input/Output resources (`make_channelmapping_*` + `insert_resource`). Default output overload = **entirely unrouted** `/map/active` (nmos-cpp default).
- After internal matrices are programmed, **`SyncChannelMappingState`** publishes the initial identity or `active-map` values (§3.5.2, §7.4). Controller **activations** do **not** call sync — they update matrices in **PLAYING** (§7.1).
- Channel-count or pad-set changes require **`RemoveChannelMapping` + `AddChannelMapping`** plus internal rebuild (§7.6). Dynamic pad add/remove after fixation is **not** supported in v1.

### 3.5 Element properties (per-pad)

External pads use **request pad templates** `sink_%u` and `src_%u` with pad GObject properties (same launch idiom as `compositor`):

**Implementation:** properties live on **custom pad subclasses** (`GstNmosAudioChannelMapSinkPad`, `…SrcPad`, etc.). The element also implements **`GstChildProxy`** so `gst-launch-1.0`, `gst-parse-launch`, and `gst-inspect-1.0` can use the `padname::property` syntax and list pad props on the element — same pattern as `compositor` / `videomixer`. Programmatic code may set properties on the pad object directly (`g_object_set(pad, …)`) without going through ChildProxy.

```bash
gst-launch-1.0 \
  nmossrc name=rx1 ! map.sink_0 \
  nmosaudiochannelmap name=map channelmapping-name=studio \
    sink_0::receiver-name=rx1 sink_0::channels=2 \
    src_0::sender-name=tx1 \
    src_0::active-map="map,0=input0:0,1=input0:1" \
  map.src_0 ! nmossink name=tx1 sender-name=tx1
```

**Element-level** — same **session / Node** props as `nmossrc` / `nmossink` (see gst-nmos-rs README), plus **`channelmapping-name`** (caller-chosen channel mapping name):

| Property | Purpose |
|----------|---------|
| `daemon-uri` | gRPC endpoint (default `unix:/tmp/nvnmosd.sock`) |
| `node-seed` | NvNmos Node seed; sessions sharing this seed share a Node |
| `http-port`, `host-name`, `domain`, `registration-url`, `system-url` | Forwarded in `OpenSession.node_config` when this session **creates** the Node; ignored when attaching to an existing Node |
| `channelmapping-name` | Caller-chosen channel mapping name; **not** an IS-08 REST resource id |

No `transport` / `transport-file` on this element.

**Per `sink_%u` pad:**

| Property | Type | Default | Purpose |
|----------|------|---------|---------|
| `receiver-name` | string | empty | Forwarded as `parent_name`; nvnmosd maps to Input `/parent` when non-empty (§6.2) |
| `input-id` | string | empty | IS-08 Input **slug**; empty → nvnmosd default (§3.4, §6.10) |
| `name` | string | empty | IS-08 `/properties` **name** (optional); **not** `AddChannelMappingRequest.name` / `channelmapping-name` |
| `description` | string | empty | IS-08 `/properties` **description** |
| `channels` | uint | `0` | `0` = derive `N_i` from negotiated caps at fixation; `N>0` = early declare (caps must match) |

**Per `src_%u` pad:**

| Property | Type | Default | Purpose |
|----------|------|---------|---------|
| `sender-name` | string | empty | Forwarded as `sender_name`; nvnmosd maps to Output `/sourceid` when non-empty (§6.2) |
| `output-id` | string | empty | IS-08 Output **slug**; empty → nvnmosd default (§3.4, §6.10) |
| `name` | string | empty | IS-08 `/properties` **name** |
| `description` | string | empty | IS-08 `/properties` **description** |
| `channels` | uint | `0` | `0` = derive `M_j` from caps; `M>0` = early declare |
| `active-map` | `GstStructure` | `NULL` | Fixation-time initial `/map/active` for this Output (§3.5.1) |

Pad properties are writable until **fixation** (first successful internal build); `g_object_set` after fixation returns an error in v1. **`active-map` is not rewritten on activations** — user-set pad properties are a fixation seed only (GStreamer convention).

Pad `receiver-name` / `sender-name` are forwarded through nvnmosd/proto/C as **`parent_name`** / **`sender_name`**; libnvnmos derives IS-04 UUIDs from the Node seed (§2, §6.2).

There is **no** global `in-channels` / `out-channels` on the element. Internal `T = Σ N_i` is computed from sink pads.

#### 3.5.1 `active-map` (`GstStructure`)

Per-src pad property; inner structure name **`map`** (no hyphen — structure string grammar).

- **Field keys:** output channel index as string (`"0"`, `"1"`, …).
- **Field values:** `inputId:channel_index` string (e.g. `input0:2`). GStreamer infers `(string)` for these values in launch lines — `(string)` is not required in normal use.
- **Unrouted output channel:** **omit the field** (documented user behaviour).
- **Whole property `NULL`:** use default identity rules (§3.5.2).

Example:

```bash
src_0::active-map="map,0=input0:0,1=input0:1"
src_1::active-map="map,2=input1:0"   # ch 0,1 unrouted (omitted)
```

Implementation may also accept `0=(string)NULL` as unrouted (parses to NULL `GValue`, not the string `"NULL"`). **Do not document** for users; cover with a unit test only. Bare `0=null` parses as string `"null"` — wrong.

#### 3.5.2 Default identity when `active-map` unset

Each **`src_%u`** has its own **active map** (IS-08 `/map/active` per Output). “Identity” is **default active map**, not a literal `T×T` matrix.

**Multiple src pads (matched index):** `src_j` maps from **`input{j}`** (same index as `sink_j`):

```text
output channel k  →  input{j}:k    for k = 0 .. min(M_j, N_j) - 1
remaining output channels           →  unrouted
```

Example: 2× stereo in, 2× stereo out → `src_0`: `0=input0:0,1=input0:1`; `src_1`: `0=input1:0,1=input1:1`.

**Single src pad:** concat-order identity over the full logical `T`-vector:

```text
# sink0=2ch, sink1=2ch, one src_0 with 4ch out
src_0::active-map="map,0=input0:0,1=input0:1,2=input1:0,3=input1:1"
```

(when unset, element computes the equivalent active map programmatically.)

**Dimension mismatch:** `M_j > N_j` → extra output channels unrouted; `M_j < N_j` → only first `M_j` input channels routed; missing `sink_j` for `src_j` → whole output unrouted.

#### 3.5.3 Internal elements — not exposed

| Internal | Programmed by | Exposed? |
|----------|---------------|----------|
| `audiomixer` sink `converter-config` | Element from `T`, `N_i` | **Never** |
| `audiomixmatrix` per src | `mode=manual`; `in-channels=T`, `out-channels=M_j`; matrix via `g_object_set` in code | **Never** |

Do **not** expose agg mix-matrices, internal `audiomixmatrix` `matrix` / `in-channels`, or `GstValueArray` matrix syntax on the bin boundary. `gst-inspect` lists **element-level session props and external per-pad props only** (via ChildProxy).

Session lifecycle (see §7.4–§7.9):

```text
OpenSession
SubscribeChannelMappingActivations
request/link pads; set pad props
AddChannelMapping           # when geometry known — creates IS-08 Input/Output resources
READY → PAUSED: caps → build internals → SyncChannelMappingState(active_map)
activations → AckChannelMappingActivation   # PLAYING; no sync
(I/O churn → RemoveChannelMapping + AddChannelMapping + rebuild + SyncChannelMappingState)
RemoveChannelMapping / CloseSession
```

Each `nmossrc` / `nmossink` keeps its **own** session with `SubscribeActivations` only.

## 4. Backup data plane: endpoint-local mapping

Deferred optional sugar — not a first implementation path.

```text
nmossrc:  transport → audiomixmatrix → src pad     (Input "network", Output "local")
nmossink: sink pad → audiomixmatrix → transport   (Input "local", Output "network")
```

Property: `enable-channel-mapping=false` (default). Equivalent to:

```text
nmossrc ! nmosaudiochannelmap channelmapping-name=… ! …
```

If implemented later in one session with both Receiver/Sender and channel map, a **unified activation stream** (§6.3) may simplify that element.

See [Appendix A](#appendix-a-endpoint-local-backup-sketch).

## 5. NvNmos C/C++ layer

Add first-class IS-08 Channel Mapping API alongside Sender/Receiver and IS-05 support. The C API must not expose GStreamer, gRPC, or gst-nmos-rs concepts.

**Implementation rule:** wrap nmos-cpp directly — no parallel IS-08 model. Map NvNmos structs onto `nmos::make_channelmapping_*` and insert/modify resources in `node_model.channelmapping_resources` (same container the Node’s Channel Mapping API serves).

Reference: `nmos-cpp-node/node_implementation.cpp` (resource creation ~1079–1248, callbacks ~2280–2680); nmos-cpp headers `channelmapping_resources.h`, `channelmapping_api.h`, `channelmapping_activation.h`.

**Terminology — three different “names”** (do not conflate):

| Field | Layer | Meaning |
|-------|-------|---------|
| **`name` (channel mapping)** | `AddChannelMappingRequest.name`, C `add_…(…, name, …)`, element **`channelmapping-name`** | Caller-chosen name; unique per Node; stored in `settings.channelmappings`; **not** IS-08 `/properties` |
| **IS-08 I/O `id`** | `ChannelMappingInput.id`, pad **`input-id`** / **`output-id`**, C `NvNmosChannelMappingInput::id` | IS-08 routing slug (`input0`, `mapB_input0`, …); **not** a UUID |
| **IS-08 `/properties` `name`** | `ChannelMappingInput.name`, C `NvNmosChannelMappingInput::name`, pad **`name`** | Short UI identifier in IS-08 (IS-08 calls it `name`, not IS-04 `label`) |

Optional pad **`description`** / C `description` / proto `description` → IS-08 `/properties.description`. **`channel_labels`** remain per-channel labels under `/channels`.

Per-pad **`name`** / **`description`** are distinct from element **`channelmapping-name`** and from GStreamer’s element instance **`name=…`** in launch lines.

**Naming convention:**

| Layer | Pattern | Examples |
|-------|---------|----------|
| **libnvnmos functions** | `channelmapping` one word (like nmos-cpp file names) | `add_nmos_channelmapping_to_node_server`, `nmos_channelmapping_activate` |
| **libnvnmos types** | `ChannelMapping` camel case in struct/enum names only | `NvNmosChannelMappingConfig`, `NvNmosChannelMappingInput`, `NvNmosChannelMappingParentType` |
| **libnvnmos enum constants** | `channelmapping` one word; value prefix includes full enum name | `NVNMOS_CHANNELMAPPING_PARENT_TYPE_RECEIVER` |
| **gRPC / proto** | `ChannelMapping` CamelCase | `AddChannelMapping`, `ChannelMappingInput`, `ChannelMappingParentType` |
| **`name` (channel mapping)** | `name` (C/gRPC), `channelmapping-name` (GStreamer element) | Caller-chosen name — remove/activate/dispatch; not an IS-08 REST resource id |

### 5.0 Alignment with nmos-cpp

| nmos-cpp | Role | NvNmos equivalent |
|----------|------|-------------------|
| `make_channelmapping_input` | Create Input resource (`/io`, `/caps`) | Built in **`add_nmos_channelmapping_to_node_server`** from `NvNmosChannelMappingInput` |
| `make_channelmapping_output` | Create Output + initial `/map/active` | Built in **`add_nmos_channelmapping_to_node_server`** — default overload = **entirely unrouted** |
| `insert_resource` on `channelmapping_resources` | Publish I/O | **`add_nmos_channelmapping_to_node_server`** (add channel mapping I/O only) |
| `modify_resource` on output `endpoint_active.map` | Update `/map/active` | **`nmos_channelmapping_activate`** |
| `on_validate_channelmapping_output_map` | Optional extra validation on activation POST (before staging); may throw → HTTP 400 | v1: default **noop** in libnvnmos; caps/schema validated by nmos-cpp |
| `on_channelmapping_activated` | Notify after `/map/active` already updated; **`void`, must not throw** | Wrapped to invoke `nmos_channelmapping_activation_callback` and **block** until nvnmosd ack (IS-05 pattern) |
| `channelmapping_activation_thread` | Apply staged activations, bump IS-04 source/device version | Internal — not reimplemented |
| HTTP `POST /map/activations` | Controller activate; 200/202/400/423 | Internal — not exposed to NvNmos apps |

**Not in IS-08 / nmos-cpp:** a single channelmapping REST resource for a whole channel mapping. The caller-chosen **`name`** in NvNmos/nvnmosd identifies which element or session owns a set of Input/Output ids; it is stored in nvnmos settings and **not** advertised in IS-08.

**IS-04 linkage at the C API:** `NvNmosChannelMappingInput::parent_name` and `NvNmosChannelMappingOutput::sender_name` are caller-chosen receiver/source/sender names (same vocabulary as `AddReceiverRequest.name` / `AddSenderRequest.name`). libnvnmos derives IS-04 UUID strings from the Node seed and passes them to `make_channelmapping_input` / `make_channelmapping_output`. **`parent_type`** selects receiver vs source when `parent_name` is set. Empty name → null `/parent` or `/sourceid`.

**Node API / port model:** IS-08 HTTP routes are **always mounted at node create** on the shared `http_port` (same as Connection) — `channelmapping_port` must **not** be `-1` in persisted settings. Remove the `channelmapping_port = -1` override in `nvnmos.cpp` (and do not set `-1` before `insert_node_default_settings`, which would be overwritten anyway). nmos-cpp mounts routes once in `make_node_server`; the `http_listener` holds a copy of the router — **do not** close/reopen listeners to add or remove routes at runtime.

**Lazy IS-04 discovery (controls only):** `make_device()` adds the IS-08 `device.controls[]` entry whenever `channelmapping_port >= 0`. To avoid advertising an empty Channel Mapping API to IS-04 controllers while HTTP routes exist:

1. **Init order** (already true in nvnmos): `make_node_server` runs **before** `node_implementation_init` / `make_device`.
2. In `node_implementation_init_`, pass settings with **`channelmapping_port = -1`** into `make_device` only — either mutate `model.settings` under the existing lock and restore immediately after, or a local `settings` copy (same effect; copy avoids restore if preferred).
3. Insert the device **without** the IS-08 control entry. Persisted `model.settings.channelmapping_port` remains `http_port` for the already-mounted HTTP API.

**After first channel mapping:** on first `add_nmos_channelmapping_to_node_server` when the node previously had no channelmapping I/O, `modify_resource` on the device to append the IS-08 `controls[]` entry (same JSON as `make_device()` in `node_resources.cpp` — extract a small helper). Bump device version for registry.

**After last channel mapping removed (optional v1 polish):** when no Input/Output resources remain, remove the IS-08 control entry from the device again. HTTP routes and `GET /x-nmos/` listing stay unchanged.

**Dormant HTTP surface:** with routes always mounted, `GET /x-nmos/` on the shared port **always** includes `channelmapping/` (nmos-cpp merges sub-routes from all mounted APIs). `GET …/inputs` and `GET …/outputs` return `[]` until the first `AddChannelMapping`. Hiding `channelmapping/` from HTTP discovery would require nmos-cpp changes — out of scope for v1.

No separate IS-08 port configuration surface in v1.

**Activation granularity:** nmos-cpp activates **one Output at a time**. The NvNmos callback and nvnmosd event carry **`output_id`** plus that output’s **active map** only — not a flattened map across all outputs.

**Application ack vs HTTP ack:** HTTP success/failure is decided by nmos-cpp **before** the activation callback (schema, caps, output lock). The callback runs **after** `endpoint_active.map` is already updated. A `false` return from `nmos_channelmapping_activation_callback` (or failed `AckChannelMappingActivation`) is logged and may affect the immediate-activation wait, but **does not roll back** the IS-08 model (same constraint as `nmos-cpp` documents for `on_channelmapping_activated`).

### 5.1 Public types

```c
typedef struct NvNmosChannelMappingInput NvNmosChannelMappingInput;
typedef struct NvNmosChannelMappingOutput NvNmosChannelMappingOutput;
typedef struct NvNmosChannelMappingActiveMapEntry NvNmosChannelMappingActiveMapEntry;
typedef struct NvNmosChannelMappingConfig NvNmosChannelMappingConfig;

typedef enum NvNmosChannelMappingParentType {
    NVNMOS_CHANNELMAPPING_PARENT_TYPE_RECEIVER = 0,
    NVNMOS_CHANNELMAPPING_PARENT_TYPE_SOURCE = 1,
} NvNmosChannelMappingParentType;
```

### 5.2 Config (`NvNmosChannelMappingConfig`)

Types mirror `make_channelmapping_*` arguments. **Constraints are not a separate matrix** — they are input `/caps` (`reordering`, `block_size`) and output `/caps` (`routable_inputs`).

**Resource names at the C API** — same vocabulary as nvnmosd/gst (§2). libnvnmos maps names to IS-04 UUIDs internally.

```c
struct NvNmosChannelMappingInput {
    const char *id;              /* IS-08 input slug; must not be NULL or empty */
    const char *name;            /* IS-08 /properties name (UI); not the caller-chosen channel mapping name */
    const char *description;     /* IS-08 /properties description */
    const char **channel_labels; /* length == num_channel_labels */
    size_t num_channel_labels;
    const char *parent_name;     /* NULL or "" → null /parent; else caller-chosen receiver/source name */
    NvNmosChannelMappingParentType parent_type; /* RECEIVER or SOURCE; ignored if !parent_name */
    bool reordering;             /* input /caps; default true */
    uint32_t block_size;         /* input /caps; default 1 */
};

struct NvNmosChannelMappingOutput {
    const char *id;              /* IS-08 output slug; must not be NULL or empty */
    const char *name;            /* IS-08 /properties name */
    const char *description;
    const char **channel_labels;
    size_t num_channel_labels;
    const char *sender_name;       /* NULL or "" → null /sourceid; else caller-chosen sender name */
    const char **routable_inputs;/* output /caps; NULL or zero length leaves /caps unrestricted */
    size_t num_routable_inputs; /* empty string entry => unrouted channels permitted */
};

struct NvNmosChannelMappingActiveMapEntry {
    const char *input_id;        /* NULL => unrouted output channel */
    uint32_t input_channel;      /* ignored when input_id NULL */
};

struct NvNmosChannelMappingConfig {
    const NvNmosChannelMappingInput *inputs;
    size_t num_inputs;
    const NvNmosChannelMappingOutput *outputs;
    size_t num_outputs;
};
```

No gain field in v1. Outputs created via **`add_nmos_channelmapping_to_node_server`** use nmos-cpp’s default `make_channelmapping_output` overload → **entirely unrouted** `/map/active` until **`nmos_channelmapping_activate`** publishes the programmed active map.

### 5.3 API functions

Parallel to IS-04 add/remove and IS-05 **`nmos_connection_activate`** (out-of-band model sync):

```c
NVNMOS_API bool add_nmos_channelmapping_to_node_server(
    NvNmosNodeServer *server,
    const char *name,
    const NvNmosChannelMappingConfig *mapping);

NVNMOS_API bool nmos_channelmapping_activate(
    NvNmosNodeServer *server,
    const char *name,
    const char *output_id,
    const NvNmosChannelMappingActiveMapEntry *active_map,
    size_t num_active_map);

NVNMOS_API bool remove_nmos_channelmapping_from_node_server(
    NvNmosNodeServer *server,
    const char *name);
```

**`add_nmos_channelmapping_to_node_server`** (like `AddSender` / daemon `AddChannelMapping`):

**Scope:** the `name` argument is the caller-chosen name of the channel mapping. All inserts/updates/erases touch **only** I/O recorded in `settings.channelmappings[name]`. **Never** modify Input/Output resources owned by another channel mapping. IS-08 slugs in `mapping` must be **unique per kind on the Node** (input ids among inputs, output ids among outputs); the same string may name both an input and an output, per IS-08 / nmos-cpp. If an id of the same kind is already owned by a different channel mapping → fail (same as nvnmosd `ALREADY_EXISTS`, §6.9).

**v1: create only.** A channel mapping is added **once**; geometry/metadata changes use **`remove_nmos_channelmapping_from_node_server` + `add_…`** (§2, §7.6). If `name` is already registered → **fail** (do not upsert in place).

1. If the node has no channelmapping I/O yet, append the IS-08 entry to `device.controls[]` (§5.0).
2. Build `make_channelmapping_input` / `make_channelmapping_output` from `mapping` (derive IS-04 UUIDs from `parent_name` + `parent_type` → `/parent`; `sender_name` → `/sourceid`).
3. **`insert_resource`** each Input/Output (default output overload = unrouted).
4. Record `{input ids, output ids}` under `settings.channelmappings[name]`.
5. Must **not** invoke the activation callback.

I/O churn → **`remove_nmos_channelmapping_from_node_server` + `add_nmos_channelmapping_to_node_server`**.

**`nmos_channelmapping_activate`** (data-plane → model; parallel **`nmos_connection_activate`**):

1. Replace the published active map for **`output_id`** via `modify_resource` + `make_channelmapping_active_map`. Dense array: index `i` is output channel `i`; `NULL` `input_id` → unrouted slot. **`active_map_count` must equal that output's channel count.**
2. Must **not** invoke the activation callback.

**`remove_nmos_channelmapping_from_node_server`:** erase all Input/Output resources owned by `name` from `channelmapping_resources` and nvnmos settings. If no channelmapping I/O remains on the node, remove the IS-08 `device.controls[]` entry (§5.0).

### 5.4 Activation callback

Registered on `NvNmosNodeConfig` alongside `connection_activated` (exact field name TBD).

```c
typedef bool (*nmos_channelmapping_activation_callback)(
    NvNmosNodeServer *server,
    const char *name,            /* caller-chosen channel mapping name; not IS-08 /properties name */
    const char *output_id,       /* IS-08 output id just activated */
    const NvNmosChannelMappingActiveMapEntry *active_map,
    size_t num_active_map);    /* dense; index i is output channel i */
```

**When invoked:** after nmos-cpp has merged the staged action into `endpoint_active.map` for **`output_id`**. **Not** invoked for `nmos_channelmapping_activate`.

**Return value:** `true` if the data plane applied the active map; `false` on failure. Recover via follow-up **`nmos_channelmapping_activate`** if needed (data-plane drift).

### 5.5 libnvnmos wiring (internal)

```text
.on_validate_channelmapping_output_map(noop or forward to future C hook)
.on_channelmapping_activated([](const nmos::resource& output) {
    parse channelmapping_id + endpoint_active.map
    → nmos_channelmapping_activation_callback(name, output_id, active_map…)
    → block on nvnmosd ack when daemon-owned
})
```

**Scheduled activations (nvnmos view):** trust nmos-cpp `channelmapping_activation_thread`; nvnmos stack applies each callback/event **immediately** when fired. No schedule metadata on gRPC/C path in v1.

## 6. nvnmosd gRPC layer

Extend the existing daemon model (`AddNode`, `OpenSession`, `AddSender`/`AddReceiver`, `SubscribeActivations`, `SyncResourceState`, …) rather than inventing a parallel lifecycle.

### 6.1 New RPCs

```proto
rpc AddChannelMapping(AddChannelMappingRequest) returns (AddChannelMappingResponse);
rpc RemoveChannelMapping(RemoveChannelMappingRequest) returns (Empty);
rpc SyncChannelMappingState(SyncChannelMappingStateRequest) returns (Empty);
rpc SubscribeChannelMappingActivations(SubscribeChannelMappingActivationsRequest)
    returns (stream ChannelMappingActivationEvent);
rpc AckChannelMappingActivation(AckChannelMappingActivationRequest) returns (Empty);
```

Same ack pattern as `SubscribeActivations` / `AckActivation` for IS-05.

### 6.2 Messages

```proto
message AddChannelMappingRequest {
  string session_handle = 1;
  string name = 2;           // caller-chosen channel mapping name; unique per Node
  repeated ChannelMappingInput inputs = 3;
  repeated ChannelMappingOutput outputs = 4;
}

message AddChannelMappingResponse {
  string channelmapping_handle = 1;  // daemon handle; no IS-08 REST resource id
  // Effective IS-08 slugs after empty-id defaulting (§6.10), same order as request.
  repeated string input_ids = 2;
  repeated string output_ids = 3;
}

message RemoveChannelMappingRequest {
  string session_handle = 1;
  string channelmapping_handle = 2;
}

enum ChannelMappingParentType {
  CHANNEL_MAPPING_PARENT_TYPE_RECEIVER = 0;  // proto3 default; ignored if parent_name empty
  CHANNEL_MAPPING_PARENT_TYPE_SOURCE = 1;
}

message ChannelMappingInput {
  string id = 1;                       // IS-08 slug; empty → nvnmosd default (§6.10)
  string name = 2;                     // IS-08 /properties name (not AddChannelMappingRequest.name)
  string description = 3;              // IS-08 /properties description
  repeated string channel_labels = 4;
  string parent_name = 5;              // empty → no /parent
  ChannelMappingParentType parent_type = 6;  // receiver vs source; ignored if parent_name empty
  bool reordering = 7;                 // default true when unset
  uint32 block_size = 8;               // default 1 when unset
}

message ChannelMappingOutput {
  string id = 1;                       // IS-08 slug; empty → nvnmosd default (§6.10)
  string name = 2;                     // IS-08 /properties name
  string description = 3;
  repeated string channel_labels = 4;
  string sender_name = 5;              // empty → null /sourceid
  repeated string routable_inputs = 6; // empty → same-request input slugs (§6.2)
}

message ActiveMapEntry {
  optional string input_id = 1;
  optional uint32 input_channel = 2;
}

message SyncChannelMappingStateRequest {
  string session_handle = 1;
  string channelmapping_handle = 2;
  string output_id = 3;
  repeated ActiveMapEntry active_map = 4;  /* dense; index i is output channel i */
}

message SubscribeChannelMappingActivationsRequest {
  string session_handle = 1;
}

message ChannelMappingActivationEvent {
  string channelmapping_handle = 1;
  string activation_handle = 2;
  string output_id = 3;        // IS-08 output id (one Output per event)
  repeated ActiveMapEntry active_map = 4;  /* dense active map for output_id */
}

message AckChannelMappingActivationRequest {
  string session_handle = 1;
  string activation_handle = 2;
  bool success = 3;
  string failure_reason = 4;
}
```

**Parent / sourceid (all layers → libnvnmos):** proto and C carry **`parent_name`** / **`sender_name`** (GStreamer-friendly). libnvnmos derives IS-04 UUID strings from the Node **seed** (same scheme as senders/receivers):

- empty `parent_name` → null `/parent` (`parent_type` ignored)
- else `parent_type == RECEIVER` → receiver id from `(seed, parent_name)`
- else `parent_type == SOURCE` → source id from `(seed, parent_name)`
- empty `sender_name` → null `/sourceid`
- else → source id from `(seed, sender_name)` (IS-04 **Source** id, not Sender id)

nvnmosd **forwards names unchanged**; libnvnmos derives IS-04 UUIDs from the Node seed on add (same as senders/receivers).

**ID accessors:** libnvnmos exposes the same pure and live helpers as for senders/receivers — see README *ID-Accessors*:

- `nmos_make_receiver_id(seed, parent_name, …)` — input `/parent` when `parent_type == RECEIVER`
- `nmos_make_source_id(seed, name, …)` — output `/sourceid` from `sender_name`, or input `/parent` when `parent_type == SOURCE` (IS-04 **Source** id, not Sender id; same caller-chosen name string as the sender)
- `nmos_get_source_id(server, source_name, …)` — live lookup after `AddSender` (Source id only; returns false if the sender is not on the Node)
- **`AddChannelMappingResponse` does not return IS-04 UUIDs** — only IS-08 slugs + `channelmapping_handle`

Registration lookup is **not** required — ids are deterministic. Controllers may see `/parent` or `/sourceid` before the endpoint appears in IS-04.

**`routable_inputs` default:** when omitted or empty on an Output, daemon sets **`routable_inputs` = all Input ids in the same `AddChannelMappingRequest`** (session-scoped routing). Explicit list required only for cross-channel-mapping routing (§6.10).

**`NvNmosChannelMappingConfig`:** the `{inputs[], outputs[]}` bundle passed to `add_nmos_channelmapping_to_node_server` — channel mapping I/O geometry, not a single IS-08 REST resource.

### 6.3 Activation streams

**Primary (standalone-first):** separate server streams.

- `nmossrc` / `nmossink` sessions: `SubscribeActivations` (IS-05) only.
- `nmosaudiochannelmap` session: `SubscribeChannelMappingActivations` (IS-08) only — **one stream per session**; each **`ChannelMappingActivationEvent`** carries **`output_id`**.

GC: require `SubscribeChannelMappingActivations` before `AddChannelMapping` (same principle as IS-05). Losing the stream starts the existing session resubscribe timeout.

**Optional (endpoint-local backup):** fold into one stream with a `oneof`:

```proto
message ActivationEvent {
  oneof event {
    ConnectionActivation connection = 1;
    ChannelMappingActivation channelmapping = 2;
  }
}
```

Use only when one session owns both Senders/Receivers and a channel map.

### 6.4 Daemon state

```rust
channelmappings: HashMap<String, ChannelMappingEntry>,
channelmappings_by_name: HashMap<(String, String), String>,
channelmapping_subscriptions: HashMap<String, tokio_mpsc::Sender<Result<ChannelMappingActivationEvent, Status>>>,
outputs_by_id: HashMap<(String, String), String>,  /* (node_seed, output_id) → session_handle */
pending_channelmapping_activations: HashMap<String, PendingChannelMappingActivation>,
next_channelmapping_id: AtomicU64,
next_channelmapping_activation_id: AtomicU64,

struct ChannelMappingEntry {
    name: String,
    node_seed: String,
    session_handle: String,
    input_ids: HashSet<String>,
    output_ids: HashSet<String>,
}
```

Index `(node_seed, name) → channelmapping_handle` for remove/activate. Index **`(node_seed, output_id) → session_handle`** for activation dispatch (parallel IS-05 `by_name`).

### 6.5 Session ownership

- One **channel mapping** (`name` / `channelmapping-name`) per owning session.
- `CloseSession` removes all channel mappings (like Senders/Receivers).
- Only the owning session may `RemoveChannelMapping` or `SyncChannelMappingState`.
- **`name` unique per Node**; IS-08 **input ids unique among inputs** and **output ids unique among outputs** on the Node across all channel mappings (§6.10). The same string may be both an input id and an output id.

### 6.6 Lifecycles

**Create I/O (like `AddSender` / `AddReceiver`):**

```text
AddChannelMapping(inputs, outputs)
  → validate session, subscription, unique name + per-kind input/output ids
  → add_nmos_channelmapping_to_node_server(name, mapping)
  → make_channelmapping_input/output + insert/modify in channelmapping_resources
  → outputs unrouted by default (nmos-cpp default overload)
  → return channelmapping_handle
```

Defer **`AddChannelMapping`** until channel counts are known — typically at **`READY→PAUSED`**, unless per-pad `channels` allow **`NULL→READY`** (§7.8).

**Publish active map (data-plane → model only):**

```text
SyncChannelMappingState(active_map)
  → nmos_channelmapping_activate(name, active_map)
  → modify_resource endpoint_active.map per output
  → does NOT invoke activation callback
```

**Controller activation (in-band, per Output):**

```text
POST /map/activations → … → ChannelMappingActivationEvent { output_id, active_map… }
  → AckChannelMappingActivation
```

Do **not** call `nmos_channelmapping_activate` on this path.

**I/O / metadata churn:** `RemoveChannelMapping` + `AddChannelMapping` (+ element rebuild + `SyncChannelMappingState`).

**Remove:** `RemoveChannelMapping` or `CloseSession` → `remove_nmos_channelmapping_from_node_server`.

### 6.7 Validation and errors

| Layer | Validates |
|-------|-----------|
| **nmos-cpp** (activation POST) | JSON schema; caps; routable_inputs; merged map → HTTP 400/423 |
| **nvnmosd** (`AddChannelMapping`) | Non-empty ids; `channel_labels` length; **per-kind unique** input ids and output ids on Node; parent_name/type consistency |
| **nvnmosd** (`SyncChannelMappingState`) | Active map references fixed I/O; indexes in range |
| **Client** (Ack path) | Data plane can apply per-output active map |

| Condition | tonic code |
|-----------|------------|
| Malformed add/sync | `INVALID_ARGUMENT` |
| Duplicate `name` or IS-08 id (same kind) | `ALREADY_EXISTS` |
| `add_nmos_channelmapping_*` failure | `INTERNAL` |

### 6.8 Transport deactivation

IS-05 deactivation does **not** remove IS-08 I/O. Last active map retained; deactivated paths produce silence at aggregation.

### 6.9 Responsibility split (summary)

| Task | nmos-cpp | libnvnmos | nvnmosd | gst client |
|------|----------|-----------|---------|------------|
| Create Input/Output | insert | `add_nmos_channelmapping_to_node_server` | `AddChannelMapping` | pad props → request |
| Update `/map/active` (data-plane) | modify_resource | `nmos_channelmapping_activate` | `SyncChannelMappingState` | after matrix programming |
| Notify data plane | callback | wrap + block for ack | stream | `audiomixmatrix` |
| I/O churn | — | remove + add | `RemoveChannelMapping` + `AddChannelMapping` | rebuild |

### 6.10 Multi-channel-mapping Nodes

Multiple sessions on the same **`node_seed`** may each call **`AddChannelMapping`** — parallel to multiple `AddSender` / `AddReceiver` on one Node:

```text
node-seed=studio
  session A: nmossrc… + nmosaudiochannelmap channelmapping-name=mapA + nmossink…
  session B: nmossrc… + nmosaudiochannelmap channelmapping-name=mapB + nmossink…
```

| Rule | Detail |
|------|--------|
| **Slug namespace** | IS-08 input **slugs** unique among inputs and output **slugs** unique among outputs per Node (enforced by nvnmos). The same string may name both an input and an output (IS-08 / nmos-cpp). **First** channel mapping on the Node: default `input0`, `output0`, …; **later** channel mappings: `{name}_input0`, … (§3.4). Explicit pad **`input-id`** / proto **`id`** overrides. |
| **Ownership** | `(node_seed, name)` → channel mapping; `(node_seed, output_id)` → **session** for activation dispatch. |
| **`routable_inputs` default** | Restricted to **Input ids from the same `AddChannelMappingRequest`**. Routing across channel mappings requires explicit `routable_inputs` (advanced). |
| **Unknown input on activation** | nmos-cpp may accept if `routable_inputs` allows; element **NACKs** if the Input id is not on its sink pads. |
| **Names before registration** | libnvnmos derives IS-04 ids from `parent_name` / `sender_name` + Node seed without `AddReceiver` / `AddSender` (§6.2). |

Fine-grained per-Input/Output add/remove is **deferred** — whole channel mapping add/remove matches element fixation and nmos-cpp churn model.

**Empty I/O slug defaulting (nvnmosd):** when `ChannelMappingInput.id` / `ChannelMappingOutput.id` (or C `id` NULL/empty) is omitted, assign before libnvnmos:

```text
if Node has no channelmapping Input/Output resources yet:
  input[i]  → "input{i}"    # i = 0, 1, … in request order
  output[j] → "output{j}"
else:
  input[i]  → "{name}_input{i}"
  output[j] → "{name}_output{j}"
```

`{name}` = `AddChannelMappingRequest.name`. Return effective slugs in **`AddChannelMappingResponse.input_ids` / `output_ids`**. Element uses those for matrix programming and `active-map` when pad **`input-id`** / **`output-id`** were left empty. Empty pad **`name`** / **`description`** → omit or pass through as empty IS-08 `/properties` (optional in IS-08).

## 7. gst-nmos-rs element behaviour

Applies to `nmosaudiochannelmap` (primary). Endpoint-local backup follows the same activation/sync/matrix rules on a single 1×1 topology.

### 7.1 Applying activations

Same pattern as IS-05 on `nmossrc` / `nmossink`: marshal from the tonic task onto the GStreamer thread with `Element::call_async`. Events are **per IS-08 Output** (`output_id` + active map for that output only).

**The pipeline stays in PLAYING.** An activation updates `audiomixmatrix` coefficients only — no element state change, no internal subgraph tear-down, no `SyncChannelMappingState`. This mirrors `nmossink` transport swaps applied inside `call_async` while the bin remains PLAYING.

```text
ChannelMappingActivationEvent { output_id, active_map… }
  → weak-ref upgrade
  → call_async
  → map output_id → src pad; validate input ids vs sink pads
  → update audiomixmatrix for that src pad only (`g_object_set` on internal element; §3.5.3)
  → AckChannelMappingActivation success/failure
```

A controller POST affecting multiple outputs yields **multiple events** — one per Output, matching nmos-cpp’s **one `on_channelmapping_activated` callback per Output** (shared `activation_id`, independent invocations). Handle independently; each ack unblocks one libnvnmos activation wait.

Never set matrices directly from the tonic task.

If the active map references inputs/outputs or channel indexes outside the **frozen** topology (§7.4), **NACK** the activation apply — do not partially update matrices.

### 7.2 Active map → matrix conversion

Output-by-input matrix per src pad. Logical input channel indices span all sink pads in pad order (sink_0 channels, then sink_1, …).

```rust
let mut matrix = vec![vec![0.0f32; total_logical_inputs]; output_channels];
for (output_channel, ch) in active_map_for_this_output.iter().enumerate() {
    if let Some(inp) = ch.input_channel {
        matrix[output_channel][inp as usize] = 1.0;
    }
}
```

Reject bindings that imply **summing** multiple inputs into one output (v1). Reject duplicate input assignments to the same output channel.

### 7.3 Aggregation and routing (`audiomixer` + `audiomixmatrix`)

Two matrix layers (§3.5.3):

| Layer | Element | When set | Changes in PLAYING? |
|-------|---------|----------|---------------------|
| Concat | `audiomixer` sink `converter-config` | Fixation / topology rebuild only | **No** |
| IS-08 active map | `audiomixmatrix` per src (`mode=manual`, `in-channels=T`, `out-channels=M_j`) | Fixation + activations + `SyncChannelMappingState` path | **Yes** (activations) |

After all sink/src pad caps are known:

1. Compute `T = sum(N_i)` — total logical input channels.
2. Negotiate **`audiomixer` src** to `T` channels (include `channel-mask` on multichannel caps).
3. For each sink pad `i` with `N_i` channels and cumulative offset `O_i`:
   - Set pad property **`converter-config`** with `GstAudioConverter.mix-matrix` (`GST_TYPE_ARRAY` of rows; each row `G_TYPE_FLOAT` coefficients).
   - Matrix size: **`T` rows × `N_i` columns**; `matrix[O_i + k][k] = 1.0`, all other entries `0.0`.
4. Set `ignore-inactive-pads=true` on the mixer where supported.
5. For IS-05-deactivated or absent input: **mute** the corresponding mixer sink pad (preferred over NULL while pad remains linked).
6. Tee the `T`-channel mixer output to per-src **`audiomixmatrix`** elements; set `in-channels=T`, `out-channels=M_j`, initial `matrix` from `active-map` or default identity (§3.5.2).

In **`manual`** mode, `audiomixmatrix` **`matrix` must be set before link/negotiation** on that element. The bin sets it programmatically during fixation before linking internals.

Initial build and any later **topology rebuild** (§7.6) follow these steps. Pad add/remove after fixation is not supported in v1.

Reference implementation of T9 validation: `rust/gst-nmos-rs/scripts/is08-aggregation-spike/spike_t9.c`.

### 7.4 Element lifecycle and property fixation

Three layers, aligned with `nmossrc` / `nmossink` session timing:

| Layer | What | When writable | Source of truth |
|-------|------|---------------|-----------------|
| **Session** | `node-seed`, daemon connection | `NULL → READY` | element properties |
| **Pad metadata** | `receiver-name`, `sender-name`, ids, `channels`, `active-map` | before **fixation** (typically while `≤ READY`) | per-pad properties (custom pad subclass) |
| **Channel geometry** | `N_i` per sink pad, `M_j` per src pad, logical `T` | at **fixation** | negotiated **audio caps** and/or per-pad `channels` early declare |

**Fixation** = first successful internal build (§7.3) plus initial **`SyncChannelMappingState`**. After fixation the element records frozen pad ids, channel counts, and cumulative input offsets. IS-08 activations (§7.1) may change the active map within that geometry only.

| GStreamer transition | Element actions |
|----------------------|-----------------|
| **`NULL → READY`** | `OpenSession`, `SubscribeChannelMappingActivations`. Request and link all sink/src pads; set pad properties. **`AddChannelMapping`** if every pad’s channel count is known (declared `channels` and/or fixed peer caps); else defer add. |
| **`READY → PAUSED`** | Negotiate caps on all pads; validate against declared `channels` if set. **`AddChannelMapping`** if not done yet (outputs unrouted). Build internals (§7.3). **`SyncChannelMappingState`** (active map only). Fail if build, add, or sync fails. |
| **`PAUSED → PLAYING`** | No topology work; data plane already configured. |
| **`READY → NULL` / `PAUSED → NULL`** | Tear down internals; `RemoveChannelMapping` / close session. |

Changing **`channelmapping-name`** or pad metadata after fixation requires **`READY → NULL`** (or `NULL`) first — `g_object_set` returns an error in v1.

### 7.5 Activations vs topology/caps changes

| Change | v1 behaviour | Pipeline state |
|--------|--------------|----------------|
| Controller **activation** (active map on fixed topology) | Update `audiomixmatrix` only; **AckChannelMappingActivation** | Stays **PLAYING** (§7.1) |
| **Caps** change on a linked pad (channel count, format) | **`RemoveChannelMapping` + `AddChannelMapping`** + full internal tear-down/rebuild (§7.6) → **`SyncChannelMappingState`** | May stay PLAYING if rebuild runs on GStreamer thread; brief glitch acceptable |
| **Pad add/remove** after fixation | **Unsupported** — fail with `GST_ELEMENT_ERROR` / failed pad request | No silent partial topology |
| Application **metadata** change | **`RemoveChannelMapping` + `AddChannelMapping`** — not `SyncChannelMappingState` | No state change required if geometry unchanged |

Controller activations must **never** change channel count — that is enforced by IS-08 `/caps` in nmos-cpp. The element mirrors that: active-map bindings are validated against frozen counts.

### 7.6 Caps and topology rebuild

When negotiated **audio caps** on any linked pad change channel count or format (including after fixation in **`PAUSED`/`PLAYING`**):

1. **`RemoveChannelMapping`** — tear down IS-08 resources for this map.
2. **Tear down** the internal aggregation/routing subgraph (`audiomixer`, per-src `audiomixmatrix`, tee links).
3. **`AddChannelMapping`** with updated topology from current pads + caps.
4. **Rebuild** internals (§7.3) with `active-map` or default identity active map.
5. **`SyncChannelMappingState`** with the programmed active map.

Perform tear-down/rebuild on the **GStreamer thread** (`call_async` or pad/stream thread) — do not manipulate bin children from the tonic task.

If rebuild **fails**: post `GST_MESSAGE_ERROR` with a clear reason; fail an in-progress state change if applicable; **NACK** activation applies that no longer match the new geometry.

**Pad add/remove after fixation** is **not** supported in v1 — recovery: `READY → NULL`, reconfigure pads, start again.

### 7.7 `SyncChannelMappingState` triggers

Call `SyncChannelMappingState` (**active map only**) when:

- initial fixation completes — matrices programmed; publish identity or `active-map` values
- caps-driven **rebuild** completes (§7.6)
- data-plane active-map change that did **not** originate from a controller activation (rare in v1; same code path as initial publish)

Do **not** sync:

- after a successful controller **activation** ACK (§7.1) — model is already updated by nmos-cpp
- for metadata or topology — use **`RemoveChannelMapping` + `AddChannelMapping`** instead

### 7.8 Deferred `AddChannelMapping`

**Default:** defer **`AddChannelMapping`** until channel counts are known from negotiated caps — typically at **`READY → PAUSED`**, matching deferred **`AddSender`** on `nmossink`.

**Early add:** if every pad has known channel count at **`NULL → READY`** (per-pad `channels` and/or fixed caps on linked peers), **`AddChannelMapping`** may run immediately; caps negotiation later **confirms** counts.

Do not publish zero-channel Inputs/Outputs.

### 7.9 `nmosaudiochannelmap` startup example (Option A)

```text
NULL → READY
  OpenSession
  SubscribeChannelMappingActivations
  request/link all sink and src pads; set pad props (receiver-name, active-map, …)
  AddChannelMapping(io, outputs unrouted)   # if geometry known; else defer

READY → PAUSED
  negotiate audio caps on every pad; validate vs declared channels
  AddChannelMapping(...)                          # if not done at NULL→READY
  build audiomixer concat (§7.3) + per-src audiomixmatrix (identity / active-map)
  SyncChannelMappingState(active_map)             # data plane → /map/active

PAUSED → PLAYING
  (no topology work)

PLAYING
  activations → call_async audiomixmatrix update → AckChannelMappingActivation

(caps channel-count change → RemoveChannelMapping + AddChannelMapping + rebuild + SyncChannelMappingState)

READY → NULL
  tear down internals; RemoveChannelMapping; CloseSession
```

Controllers may briefly see **unrouted** outputs between `AddChannelMapping` and `SyncChannelMappingState` during `READY→PAUSED`; fail the transition if sync fails.

## 8. v1 feature profile

**Support:**

- Audio only
- Multiple sink and src pads on `nmosaudiochannelmap` (fixed pad set before fixation)
- Per-pad properties (`sink_%u::…`, `src_%u::…`), optional **`name`** / **`description`**, and `active-map` `GstStructure` for initial `/map/active`
- Default slugs: first channel mapping `input0`/`output0`; later `{channelmapping-name}_input0` when ids omitted (§6.10)
- Any-to-any routing where output `routable_inputs` / input caps allow
- Default identity rules (§3.5.2); omitted `active-map` fields = unrouted; duplicate one input to many outputs
- Channel counts from negotiated audio caps and/or per-pad `channels` early declare; **channel-count change → Remove + Add + rebuild + active-map sync** (§7.6)
- Controller activation ACK/NACK **in PLAYING** (active-map changes only; §7.1)
- `SyncChannelMappingState` for **active map only** (data-plane → model)

**Do not support initially:**

- Arbitrary gain (only 0/1)
- Fan-in / summing multiple inputs into one output
- Dynamic channel-count control via IS-08 activations (active map changes only within fixed `/caps`)
- **Pad add/remove after fixation** without `NULL` reset (fail robustly; §7.6)
- Exposed internal `audiomixer` / `audiomixmatrix` properties (§3.5.3)
- Metadata or topology updates via `SyncChannelMappingState`
- Transport-file channel-map syntax
- Embedded endpoint-local matrix (until backup explicitly chosen)

Controller **scheduled** activations are accepted in IS-08 (nmos-cpp handles timing and HTTP 202); the nvnmos stack applies each callback/event immediately when fired (§5.0).

## 9. Implementation order

1. ~~Aggregation spike~~ — **done** (§10); **audiomixer concat** validated (T9)
2. NvNmos C/C++ channelmapping API + nmos-cpp wiring (`add_nmos_channelmapping_to_node_server`, `nmos_channelmapping_activate`; remove `channelmapping_port = -1` from `make_settings`; lazy `controls[]` via `make_device` settings trick §5.0)
3. nvnmosd proto, state, integration tests:
   - `AddChannelMapping`; `SyncChannelMappingState` active-map-only; activation ACK
   - I/O churn (`Remove` + `Add`); multi-channel-mapping per-kind id uniqueness
4. `nmosaudiochannelmap`: shared session props, `channelmapping-name`, custom pad subclasses (+ ChildProxy for launch/inspect), `active-map` parser, internal **audiomixer** + **`audiomixmatrix`**
5. Demos: two receivers → map → two senders; partial activation; 1×1 L/R swap
6. Optional: endpoint-local backup (Appendix A) — not a second matrix code path

## 10. Aggregation spike

**Status: complete** (GStreamer 1.24.2). Runner: `rust/gst-nmos-rs/scripts/is08-aggregation-spike/run_spike.py`. Full output: `doc/designs/is08_aggregation_spike_results.md`.

| ID | Test | Outcome |
|----|------|---------|
| T1 | `interleave` — NULL second input | **STALL** |
| T2 | `interleave` — PAUSED second input | **STALL** |
| T1b | `interleave` — only one sink linked | **STALL** (no output) |
| T3 | `deinterleave`→`interleave` 4ch — NULL one stereo | **STALL** |
| T5 | `audiomixer` — mute second pad | **PASS** (sums; wrong semantics) |
| T6 | `audiomixer` — NULL second input | **PASS** (sums; wrong semantics) |
| T7 | `adder` — NULL second input | **STALL** |
| T8 | `audiomixmatrix` single stream | **PASS** (control) |
| T9 | `audiomixer` disjoint mix-matrix → 4ch concat | **PASS** |

**Conclusion:** Use **`audiomixer` + per-pad `converter-config` mix-matrix`** (disjoint placement) to build the logical `T`-channel input vector, then **`audiomixmatrix` per src pad** for IS-08 routing. `interleave` / deinterleave→`interleave` remain unsuitable (T1–T3).

### 10.1 `audiomixer` concat via `converter-config`

Default `audiomixer` behaviour **sums** aligned channels (T5/T6 without placement matrices). For IS-08 we need **concat** into `T = N_0 + N_1 + …` channels:

| Step | Action |
|------|--------|
| Caps fixed | Know each `N_i` and total `T` |
| Mixer src | Negotiate **`T` channels** (+ valid `channel-mask`) |
| Per sink pad `i` | `converter-config` → `GstAudioConverter.mix-matrix` of size **`T × N_i`** |
| Placement | Row `O_i + k`, col `k` = `1.0`; all other entries `0.0` (`G_TYPE_FLOAT`) |
| Mix step | Disjoint rows → **add == concat** |
| Inactive input | **Mute** pad → silence in that slice (T9); flow continues |
| Routing | Shared `T`-ch buffer → per-src **`audiomixmatrix`** (IS-08 activations) |

**T9** (`spike_t9.c`) validates: 2× stereo → 4ch out; all channels live; mute silences one pair only; NULL on one branch still ~98 buf/s on output.

**Caveats:** all sink pads must use **non-overlapping** placement; any overlap reintroduces summing. Caps or channel-count change triggers a full internal rebuild (§7.6). Custom aggregation remains a fallback only if this pattern fails on target hardware/format combinations.

### 10.2 Why not `interleave` / default mixer sum?

| Approach | Stall on missing input? | Concat? |
|----------|-------------------------|---------|
| `interleave` (T1–T3) | **STALL** | No |
| `audiomixer` default sum (T5/T6) | Continues | No — sums aligned channels |
| `audiomixer` + disjoint mix-matrix (T9) | Continues | **Yes** |
| Custom GstAggregator | Yes (if designed) | Yes — fallback only |

`adder` stalls like legacy mixers (T7). **`audiomixmatrix`** applies the active map on one interleaved buffer (T8); it is not a multi-sink aggregator.

## 11. Decisions and remaining questions

**Decided:**

- **Naming:** libnvnmos functions `channelmapping` one word; C types `NvNmosChannelMapping*`; gRPC `ChannelMapping*`; reserve **active map** for `/map/active` (§5.0).
- **nmos-cpp IS-08 surface:** wrap directly — `make_channelmapping_*`, `channelmapping_resources`, activation thread (§5.0).
- **Three RPC paths:** `AddChannelMapping` (create I/O), `SyncChannelMappingState` (active map only), activation + ack (§2, §6.6).
- **C API:** `add_nmos_channelmapping_to_node_server`, `nmos_channelmapping_activate`, `remove_nmos_channelmapping_from_node_server`; callback `nmos_channelmapping_activation_callback` (§5.3).
- **I/O bundle:** `NvNmosChannelMappingConfig` / `{inputs, outputs}` on add; **`NvNmosChannelMappingActiveMapEntry`** on activate/callback (dense; array index = output channel).
- **IS-04 linkage:** C API uses resource **names** (`parent_name`, `sender_name`); libnvnmos derives IS-04 UUIDs from the Node seed (§2, §5.2, §6.2). `parent_type` default RECEIVER, ignored when `parent_name` empty.
- **ID accessors:** `nmos_make_source_id` and `nmos_get_source_id` for IS-04 Source UUID from the caller-chosen sender name (§6.2); listed in README with other `nmos_*_id` helpers.
- **Multi-channel-mapping Nodes:** multiple `AddChannelMapping` per Node; per-kind unique IS-08 ids; default `routable_inputs` scoped to same request (§6.10).
- **Element session props:** same as `nmossrc`/`nmossink` + `channelmapping-name` (§3.5).
- **Default outputs at create:** unrouted; initial active map via `SyncChannelMappingState` after matrix programming (Option A, §7.9).
- **Pad API:** per-pad properties; `input-id`/`output-id` (IS-08 slug), `name`/`description` (IS-08 `/properties`), separate from **`channelmapping-name`**; ChildProxy for launch/inspect (§3.5).
- **IS-08 `/properties`:** exposed on C struct and proto; optional pad props; distinct from `AddChannelMappingRequest.name` (§5.0 terminology table).
- **Activation:** one **`SubscribeChannelMappingActivations` stream per session**; one **event per Output** (`output_id` in event).
- **Channel Mapping API:** HTTP routes always on `http_port` at node create; lazy IS-04 `device.controls[]` (temporary `channelmapping_port = -1` for `make_device` only; append on first add, optional remove on last — §5.0). No listener recycle.

**Remaining:**

1. ~~Move **`parent_name` / `sender_name` → IS-04 UUID** derivation from nvnmosd into libnvnmos~~ (done).

## 12. References

- `doc/designs/is08_aggregation_spike_results.md`
- `doc/designs/is08_per_src_mixer_spike_results.md` (exploratory; Appendix B)
- `doc/designs/nvnmosd/README.md`
- `rust/nvnmos-rpc/proto/nvnmosd.proto`
- AMWA IS-08 v1.0.1 Channel Mapping API
- nmos-cpp: `Development/nmos/channelmapping_resources.{h,cpp}`, `channelmapping_api.{h,cpp}`, `channelmapping_activation.{h,cpp}`; example `Development/nmos-cpp-node/node_implementation.cpp`
- GStreamer: `audiomixmatrix`, `interleave`, `deinterleave`, `audiomixer`

---

## Appendix A: Endpoint-local backup sketch

Not in v1 implementation scope. Documented for simple 1×1 topologies only.

```text
enable-channel-mapping: bool = false   # set before NULL → READY
```

| Element | Input ID | Output ID | Data path |
|---------|----------|-----------|-----------|
| `nmossrc` | `network` (parent: Receiver) | `local` | transport → audiomixmatrix → src pad |
| `nmossink` | `local` | `network` (optional source_id) | sink pad → audiomixmatrix → transport |

One session per element would subscribe to **both** IS-05 and IS-08 activation streams (or unified `oneof` §6.3). Prefer documenting the compositional equivalent:

```text
nmossrc ! nmosaudiochannelmap ! …
```

instead of maintaining a second matrix implementation inside `nmossrc`/`nmossink`.

---

## Appendix B: Per-src audiomixer alternative (exploratory spike)

**Status:** exploratory only — **does not change the v1 plan** (§3.2, §7.3, §10). Tradeoff summary: §3.2.1. Documented so Plan B can be revisited without re-discovering the analysis.

### B.1 Proposal

Use **one `audiomixer` per src pad** inside `nmosaudiochannelmap`, with **`tee`** fan-out from each external sink pad to every output mixer. No central aggregation mixer and no `audiomixmatrix`. The IS-08 map for each Output is expressed **directly** on sink pad `converter-config` mix-matrices:

```text
sink_0 (N₀ ch) ──tee──┬──► audiomixer → src_0 (M₀ ch)
                      └──► audiomixer → src_1 (M₁ ch)

sink_1 (N₁ ch) ──tee──┬──► audiomixer → src_0
                      └──► audiomixer → src_1
```

Per sink pad `i` on output mixer `j`: matrix size **`M_j × N_i`**; set `matrix[output_ch][input_ch] = 1.0` for each active-map binding. v1 constraints (0/1 coeffs, no fan-in) imply **disjoint output rows** per mixer → add behaves like map, not sum.

### B.2 Potential advantages

- IS-08 active map binds 1:1 onto pad matrices (no logical `T`-channel index in the data plane).
- Fewer matrix coefficients in simple topologies (e.g. 2× stereo → 2× stereo: 16 vs 32 for agg + map).
- Activation on one Output updates only that mixer’s pad matrices.
- Duplicate-to-many outputs is natural (same coeff on the corresponding pad of each output mixer).

### B.3 Caveats

- Not “audiomixer only”: **`tee`** (or equivalent fan-out) per sink is required.
- **`N_src` GstAggregators** on teed streams — sync/latency behaviour needs validation on target hardware.
- Heavier per output than `audiomixmatrix` for pure matrix work.
- Internal bin is harder to inspect than agg + matrix.

### B.4 Spike T10 (separate runner)

Runner: `rust/gst-nmos-rs/scripts/is08-per-src-mixer-spike/run_spike.py`  
Results: `doc/designs/is08_per_src_mixer_spike_results.md`  
Reference: `spike_t10.c`

| ID | Test | Outcome (GStreamer 1.24.2) |
|----|------|----------------------------|
| T10a | Cross-map on one output mixer; mute one sink pad | **PASS** |
| T10b | Duplicate one input channel to both output mixers | **PASS** |
| T10c | NULL one input branch; both output mixers keep flowing | **PASS** (~103 buf/s each) |

**T10 conclusion:** per-src `audiomixer` routing is **technically viable** on 1.24.2 for the scenarios tested. **v1 still uses Plan A** (§3.2, §3.2.1): central agg `audiomixer` + per-src `audiomixmatrix`. Revisit this appendix if Plan A’s wide T-buffer or dual matrix builder becomes the bottleneck.

---

## Appendix C: Identity active map at `AddChannelMapping` (Option B)

**Status:** documented alternative — **v1 uses Option A** (§7.9).

Option B passes identity `active_map` into `make_channelmapping_output(..., active_map)` at **`AddChannelMapping`** time, so `/map/active` advertises the map **before** internal matrices exist. No initial `SyncChannelMappingState` is required for startup.

| | **Option A (v1)** | **Option B** |
|--|-------------------|--------------|
| `AddChannelMapping` outputs | Unrouted (nmos-cpp default) | Identity `active_map` in create call |
| Initial `SyncChannelMappingState` | Yes — after matrix programming | No (only for later local active-map changes) |
| Controller view during `READY→PAUSED` | Briefly unrouted until sync | Identity advertised before matrices exist |

Option A keeps a **single active-map publish path** for the element’s whole life: data plane programs matrices, then `SyncChannelMappingState` reflects `/map/active`. Option B is viable if controllers must never see unrouted outputs during startup — at the cost of `/map/active` potentially leading the data plane by a narrow window unless internals are built before `AddChannelMapping`.
