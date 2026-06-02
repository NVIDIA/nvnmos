<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# NMOS Daemon and GStreamer Plugin

A plan for a new way of supporting NMOS in GStreamer, replacing the bin-based `nvdsnmosbin` approach with an out-of-process NMOS daemon (`nvnmosd`) and a pair of single-pad GStreamer elements (`nmossrc`, `nmossink`). NvNmos provides the NMOS implementation inside the daemon; GStreamer plugins talk to the daemon over a small gRPC protocol.

## Initial scope

To cover:

1. **MXL senders and receivers** using the `mxlsrc` and `mxlsink` elements from `gst-mxl-rs` in the `mxl` repo.
2. **ST 2110** using the DeepStream `nvdsudpsrc` and `nvdsudpsink` elements (built on Rivermax SDK), in their modes with built-in RTP payload/depayload.
3. **ST 2110** (without perfect packet-pacing) using GStreamer's `udpsrc` and `udpsink` elements with the necessary RTP (de)payloaders.
4. **ST 2110 essence formats**:
    a. `video/x-raw` (ST 2110-20 `video/raw`) — 1920×1080 and 3840×2160 at common broadcast frame rates (25, 30000/1001, 30, 50, 60000/1001, 60), 10-bit and 8-bit YCbCr-4:2:2 (`format=UYVP` or `UYVV`), also RGB formats, progressive and interlaced.
    b. `video/x-jxsv` (ST 2110-22 `video/jxsv`) — placeholder until nvdsudp changes merge.
    c. `audio/x-raw` (ST 2110-30 `audio/L24` and `audio/L16`) — 16- and 24-bit LPCM, 48 kHz (and 96 kHz), 1–16 channels.
    d. `meta/x-st-2038` (ST 2110-40 `video/smpte291`) — supported via the nvdsudp ANC patch and corresponding GStreamer media type.
    e. **ST 2022-7** — stream duplication of any of the above on two NICs.
5. **MXL essence formats**:
    a. `video/x-raw,format=v210` (MXL `video/v210`) — progressive only for now.
    b. unclear how to handle MXL `video/v210a` (v210 with alpha) — stub until GStreamer support lands.
    c. `audio/x-raw,format=F32LE` (MXL `audio/float32`) — 32-bit float LPCM, 48 kHz, multi-channel.
    d. `meta/x-st-2038` (MXL `video/smpte291`) — same as for ST 2110-40.
6. **NvNmos** provides the NMOS implementation.

## Architecture

### Out-of-process daemon + single-pad elements

We pick an out-of-process design after considering the in-process bin alternative:

- **Daemon `nvnmosd`** wraps NvNmos and owns one or more NMOS Nodes. Survives pipeline restarts. Hosts multiple pipelines and multiple client processes under one or more shared Nodes.
- **`nmossrc` and `nmossink`** are single-pad Rust GStreamer elements. Each opens a session with `nvnmosd`, registers its `transport-file`, subscribes to activations, and wraps an inner transport src/sink:
  - MXL: `mxlsrc` / `mxlsink` from `gst-mxl-rs` (Rust).
  - ST 2110 with Rivermax: `nvdsudpsrc` / `nvdsudpsink` (C, DeepStream).
  - ST 2110 without Rivermax: `udpsrc` / `udpsink` + appropriate RTP (de)payloaders.
- **No GStreamer dep in `nvnmosd`.** **No NvNmos / C FFI dep in `gst-nmos-rs`.** They share only the generated gRPC client/server crate. Independently testable.
- **We do not modify** `mxlsrc`/`mxlsink`/`nvdsudpsrc`/`nvdsudpsink`/`udpsrc`/`udpsink`. We wrap them. This is non-negotiable for adoption — pushing changes upstream isn't on the critical path.
- **Rust for daemon and elements.** Rationale: shared crate for the generated gRPC client/server is natural in one language; `gst-rs` is a first-class GStreamer plugin path; `bindgen`/`cxx` make the FFI to `libnvnmos` (and reuse of `nvnmos-sys` from elsewhere) cheap. A C++ daemon was considered and rejected — it would mostly re-implement what `tonic` already gives us in Rust.

Why out-of-process over the in-process bin (replacing `nvdsnmosbin`):

- NMOS Node identity is decoupled from any one pipeline's lifecycle. Pipeline reloads no longer cause registry flap.
- Single-pad elements compose with the rest of GStreamer the way users expect; no multi-pad bin gymnastics.
- Cross-language clients become possible (Python control planes, C++ test harnesses, future engines) without GStreamer in the loop.
- Activation translation pain (the `fallbackswitch`/livesync workarounds that ended up inside `nvdsnmosbin`) moves to the right place: the *element* decides what to do during deactivation, and the application can compose around it. The bin no longer owns this complexity invisibly.

Costs accepted:

- An IPC layer (gRPC) to ship and maintain.
- Sub-millisecond latency on activation propagation over local UDS, acceptable against NMOS Connection API timescales.
- Two binaries to deploy (one container, two processes).

### Repository layout

`nvnmosd` and `gst-nmos-rs` both live under the existing `nvnmos` repo, in a new top-level Rust workspace separated from the C/C++ core.

```
nvnmos/
├── src/                          # existing C/C++ NvNmos library, unchanged
├── docs/                         # existing
└── rust/                         # new Rust workspace
    ├── Cargo.toml                # workspace = ["nvnmos-sys", "nvnmos-rpc", "nvnmosd", "gst-nmos-rs"]
    ├── nvnmos-sys/               # bindgen-generated FFI to ../src/nvnmos.h
    ├── nvnmos-rpc/               # *.proto + tonic-generated client + server stubs
    ├── nvnmosd/                  # Rust daemon binary; deps: nvnmos-sys + nvnmos-rpc (server)
    └── gst-nmos-rs/              # GStreamer plugin (Rust); deps: nvnmos-rpc (client only)
```

`gst-nmos-rs` does not link `src/`. The daemon owns all NvNmos / C-API interaction.

## Protocol

### Naming conventions

- Daemon binary: **`nvnmosd`**.
- Default endpoints: `unix:/var/run/nvnmosd.sock` (Linux), named pipe `\\.\pipe\nvnmosd` (Windows), `unix:/tmp/nvnmosd.sock` (macOS). TCP `localhost:NNNN` as a portable fallback.
- gRPC package: `nvnmos.daemon.v1`. Service: `NvnmosDaemon`. Proto file: `nvnmosd.proto`.

### Naming convention

- **`<thing>_id`** — an NMOS resource UUID. Stable, generated by libnvnmos from a seed, and what shows up at IS-04 (`/self/<id>`, `/senders/<id>`, `/receivers/<id>`).
- **`<thing>_handle`** — a daemon-local opaque token. Issued by the daemon, meaningful only within this gRPC API, and not surfaced to NMOS controllers. Treat as opaque; do not parse.

### Service surface (sketch)

The authoritative version lives at [`rust/nvnmos-rpc/proto/nvnmosd.proto`](../../../rust/nvnmos-rpc/proto/nvnmosd.proto); the sketch below is a guide.

```proto
service NvnmosDaemon {
  // Persistent Nodes — Nodes that exist independently of any contributing
  // session and that serve as discovery anchors in IS-04 even when no
  // client process is currently attached. Sessions may still attach to a
  // persistent Node via OpenSession and contribute senders/receivers to it.
  // Most deployments don't need this and rely on session-refcounted Nodes
  // (created on demand by OpenSession) instead.
  rpc AddNode(AddNodeRequest) returns (AddNodeResponse);
  rpc RemoveNode(RemoveNodeRequest) returns (Empty);

  // Sessions. A session contributes resources to exactly one Node.
  // Many sessions across many client processes may share a Node by
  // node_seed. The session handle distinguishes them so that the daemon
  // can refcount Node lifetime correctly, route activations only to the
  // session that registered the affected resource, and drop the right
  // resources when a stream dies.
  rpc OpenSession(OpenSessionRequest) returns (OpenSessionResponse);
  rpc CloseSession(CloseSessionRequest) returns (Empty);

  // Resource lifecycle. transport_file = same string NvNmos takes today
  // (SDP for RTP, flow_def JSON for MXL). The x-nvnmos-name attribute
  // (SDP) or urn:x-nvnmos:tag:name tag (MXL flow_def) inside is the
  // NvNmos name of the sender or receiver; NvNmos generates the NMOS
  // /senders/<id> or /receivers/<id> UUID from it.
  // AddResourceResponse returns both the daemon-local resource_handle
  // (for subsequent RPCs) and the NMOS resource_id (informational).
  rpc AddSender(AddSenderRequest) returns (AddResourceResponse);
  rpc AddReceiver(AddReceiverRequest) returns (AddResourceResponse);
  rpc RemoveResource(RemoveResourceRequest) returns (Empty);

  // Server-streaming: one per session, kept open for the session's lifetime.
  rpc SubscribeActivations(SubscribeActivationsRequest) returns (stream ActivationEvent);

  // Required after each ActivationEvent. Unblocks the daemon's blocking
  // NvNmos activation callback; success/failure (with optional reason).
  // This does NOT call nmos_connection_activate; NvNmos updates IS-04/IS-05
  // automatically when its callback returns success.
  rpc AckActivation(AckActivationRequest) returns (Empty);

  // Report out-of-band data-plane changes. Maps to nmos_connection_activate
  // inside the daemon.
  rpc SyncResourceState(SyncResourceStateRequest) returns (Empty);
}
```

### Activation flow

1. Controller PATCHes IS-05 → NvNmos parses → NvNmos calls the daemon's activation callback (blocking, in NvNmos's HTTP handler thread, for immediate activations).
2. Daemon dispatches an `ActivationEvent` to the owning session over the open `SubscribeActivations` stream.
3. Element applies (or refuses) the activation on its inner transport src/sink.
4. Element calls `AckActivation` (success or failure, with optional reason).
5. Daemon's callback returns to NvNmos, returning the element's outcome verbatim.
6. NvNmos updates IS-04/IS-05 automatically and responds to the controller's HTTP request.

`AckActivation` outcomes propagate end-to-end — the daemon does **not** swallow failures and return `true` to NvNmos. (Rationale: `nvdsnmosbin`'s callback always returned `true`, which left controllers thinking activations had succeeded when downstream property changes had silently failed. The lesson is to keep the outcome honest and visible at IS-05.)

Important: **the daemon never calls `nmos_connection_activate` in this path.** That call is reserved for `SyncResourceState`, i.e., reporting out-of-band data-plane changes that NvNmos didn't initiate. Phase 1 has no use case for it.

### Multi-Node support

The daemon hosts zero or more NvNmos node-server instances, each with its own HTTP port. The `node_seed` is the daemon's lookup key — the underlying NMOS `/self` `node_id` (UUID) is generated deterministically by libnvnmos from the seed, so the same seed always yields the same `node_id`. The common deployment has a single Node; multi-Node is supported but rarely needed in practice.

Node lifetime comes in two flavours:

- **Session-refcounted (default).** Created on demand by the first `OpenSession` for an absent `node_seed`, using the `NodeConfig` carried in that request. Subsequent `OpenSession` calls for the same seed attach to the existing Node; their `NodeConfig` is ignored. Torn down when the last contributing session closes.
- **Persistent.** Created explicitly via `AddNode(node_seed, node_config)`, torn down explicitly via `RemoveNode(node_seed)`. Survives independently of any session, which is what makes it usable as a discovery anchor in IS-04 with zero contributing client processes.

The rules between the two flavours:

- `AddNode` errors with `ALREADY_EXISTS` if any Node — persistent or session-refcounted — already exists for the seed.
- `RemoveNode` errors with `FAILED_PRECONDITION` if the Node is session-refcounted (close the sessions instead), or if any sessions are still attached to the persistent Node (close them first; we don't yank the rug out from under live clients).
- `OpenSession` attaches to whichever flavour the Node happens to be. The session itself doesn't care, and `CloseSession` only triggers teardown for session-refcounted Nodes.

### Daemon internal state

The daemon's runtime state lives entirely in a single `State` struct in [`rust/nvnmosd/src/state.rs`](../../../rust/nvnmosd/src/state.rs), guarded by an `Arc<Mutex<State>>` in `main.rs`. There is no on-disk persistence; the daemon process is otherwise stateless beyond the UDS socket and tonic's per-connection internals. The shape below is the wire-protocol model expressed as data; it is what defines the per-RPC error rules quoted above.

#### Collections

| Collection | Keyed by | Holds | Used for |
|---|---|---|---|
| `nodes` | `node_seed` | `NodeServer` handle, cached `node_id`, `Lifetime`, attached-session refcount | At most one libnvnmos node-server per seed; many sessions can attach to the same Node. |
| `sessions` | `session_handle` (`sess-N`) | the `node_seed` the session is attached to, the set of `resource_handle`s the session owns | Per-client logical attachment to a Node. |
| `resources` | `resource_handle` (`res-N`) | `name`, owning `session_handle`, owning `node_seed`, `Side` (Sender or Receiver) | Live senders or receivers contributed by sessions. |
| `by_name` | `(node_seed, Side, name)` | `resource_handle` | Reverse index for (a) the per-side duplicate-name check in `AddSender` or `AddReceiver`, and (b) the activation router's lookup from libnvnmos's IS-05 callback (which delivers `(side, name)`) back to the owning session. Keying on `Side` lets a Sender and a Receiver share the same `name` on the same Node — the daemon distinguishes them by side. |
| `subscriptions` | `session_handle` | tokio mpsc `Sender<ActivationEvent>` half | At most one open `SubscribeActivations` stream per session; the receiver half is held by the streaming RPC handler. |
| `pending_activations` | `activation_handle` (`act-N`) | owning `session_handle`, `std::sync::mpsc::SyncSender<AckOutcome>` | Activations the daemon has pushed to a subscriber and is now blocking the libnvnmos worker on, waiting for `AckActivation`. The worker is parked in `recv_timeout` with `ACTIVATION_ACK_TIMEOUT` (currently 5 s; the libnvnmos IS-05 PATCH stays open for the same duration). |

Three monotonic `AtomicU64` allocators inside `State` issue the daemon-local handles. The `sess-N` / `res-N` / `act-N` formats are deliberately opaque to clients (see *Naming convention*).

#### Invariants

- Every `resource_handle` in `sessions[s].resources` has a matching entry in `resources`, and `resources[h].session_handle == s`.
- Every `(node_seed, side, name)` in `by_name` points at a `resource_handle` that exists in `resources`; the resource's `node_seed`, `side`, and `name` fields match the key. The activation router logs an error and NACKs if it ever finds this broken.
- Every `session_handle` in `subscriptions` has a matching `sessions` entry. `CloseSession` removes both.
- Every `activation_handle` in `pending_activations` belongs to a session that still has a `subscriptions` entry. `CloseSession` drains pending activations for the closing session; dropping the `SyncSender` wakes the libnvnmos worker with a `Disconnected` error and NACKs the IS-05 controller.
- `nodes[seed].attached_sessions` equals the number of `sessions` entries whose `node_seed == seed`.

#### Mutating entry points

State is only modified through methods on `State`, all called under the daemon-wide mutex:

| Concern | Methods |
|---|---|
| Node lifecycle | `add_node`, `remove_node`, `open_session`, `close_session` |
| Resource lifecycle | `add_sender`, `add_receiver`, `remove_resource`, `sync_resource_state` |
| Activation flow | `subscribe_activations`, `dispatch_activation` (from the libnvnmos worker via the activation trampoline), `complete_activation` (from `AckActivation`), `cleanup_pending_activation` (idempotent post-recv from the libnvnmos worker) |

`dispatch_activation` is the only entry point reached from a libnvnmos worker thread; the others all run on the gRPC service's tokio executor. Holding the same mutex across both means the `AckActivation` handler can never observe a `pending_activations` entry that hasn't been published yet, even though the libnvnmos worker is blocked on the channel during the round-trip.

## Element design

### Pad config

The `nmossrc`/`nmossink` elements expose **essence** on their external pads (`video/x-raw`, `audio/x-raw`, `meta/x-st-2038`), never the wire form. Depayloaders / payloaders are added internally when needed (for `udpsrc` / `udpsink` paths); for `mxlsrc`/`mxlsink` and `nvdsudpsrc`/`nvdsudpsink` no extra (de)payloader is needed because those inner elements already operate on essence buffers. The choice of wire transport is something the element *configures*, not something it exposes — so a `gst-launch-1.0` user sees the same essence-shaped pad regardless of whether the inner chain is MXL, RTP via Rivermax, or RTP via OSS.

#### Connection / Node

- `daemon-uri` — gRPC endpoint (UDS / pipe / TCP).
- `node-seed` — the NvNmos **seed** identifying which Node to participate in. The daemon resolves the seed to an NvNmos node-server instance, creating one on demand if absent. This is *not* the NMOS `/self` (Node) resource id — that UUID is generated by NvNmos from the seed (plus internal salting). Two sessions sharing the same `node-seed` (in the same daemon) participate in the same Node.

#### Transport selection

The wire transport is selected explicitly by the user. The element does **not** auto-detect — both RTP options exist for good reasons and silently picking one based on build-time availability or caps heuristics would be a footgun.

Each value names the inner GStreamer element family the user is signing up for, so there's no translation between property value and pipeline behaviour:

| `transport` value | Inner src element | Inner sink element | Phase | Notes |
|---|---|---|---|---|
| `mxl` | `mxlsrc` | `mxlsink` | 1 | MXL shared-memory; requires `mxl-domain-id`. |
| `nvdsudp` | `nvdsudpsrc` | `nvdsudpsink` | 2 | RFC 4175 / ST 2110 via the DeepStream `nvdsudp*` elements (built on Rivermax SDK); high-precision packet pacing. Required for ST 2022-7. |
| `udp` | `udpsrc` + `rtp*depay` | `rtp*pay` + `udpsink` | 3 | RFC 4175 / ST 2110 via OSS GStreamer `udp*` + RTP (de)payloaders; no perfect pacing. Cannot support ST 2022-7. |

Required property on both `nmossrc` and `nmossink`. The element refuses an unsupported value with a clear error at element-construction time (i.e. when the build doesn't include that transport, or the user picks a Phase that hasn't shipped yet). The `transport` value also gates which per-transport defaults the element applies (notably `TP=2110TPN` for `nvdsudp` vs `TP=2110TPW` for `udp`, per *Defaults the element synthesises*).

#### Routes to a complete transport file

There are three ways to fully describe a sender or receiver. Each is sufficient on its own; they can be combined, with the property route either overriding or cross-checking matching fields in `transport-file` depending on the property (see Route C and the property-interaction matrix in the gst-nmos-rs crate README).

**Route A — `transport-file` only.** Provide a complete SDP (RTP) or MXL flow_def JSON. Self-sufficient; the element derives the essence caps from it (using a port of `nvds_nmos_bin/src/helpers/sdp_caps_to_raw_caps.{cpp,h}` for SDP, and an MXL equivalent). Required-route for users who already have an authoritative transport file or who need precise control over fields the property route doesn't surface.

**Route B — property route.** Build up the transport file from typed properties on the element:

| Property | Carries | Required? | Applies to |
|---|---|---|---|
| `caps` | essence pad caps (`video/x-raw,...`, `audio/x-raw,...`, `meta/x-st-2038,...`) | Required on `nmossrc`; on `nmossink` may be omitted in deferred mode (see below) | Both |
| `transport-caps` | SDP `a=fmtp:`-equivalent extras as a GstCaps blob. Almost everything is synthesised by the element from essence caps + per-format defaults + per-transport defaults (see *Defaults the element synthesises* below); set this only to **override** a default or to carry an SDP extra the element doesn't know (e.g. non-default PT, `2110BPM` instead of `2110GPM`, audio `ptime`, `TCS`, …). Conventionally `application/x-rtp,...` but the element only consults fields, not the structure name. | Optional; typically empty even for RTP, usually empty for MXL. | Both |
| `sender-name` (on `nmossink`), `receiver-name` (on `nmossrc`) | `x-nvnmos-name` SDP attribute (RTP) or `urn:x-nvnmos:tag:name` flow-def tag (MXL) — NvNmos name for this sender or receiver. Names are unique within a side on the Node: a Sender and a Receiver may share the same name (the daemon's `by_name` is keyed on `(node_seed, side, name)` and `ActivationEvent.side` disambiguates the activation callback). | Required (either as a property, or carried in `transport-file`) | Per element (split to avoid collision with `GstObject.name`) |
| `iface-ip` | `x-nvnmos-iface-ip` — interface IP for SDP `o=` / `c=` lines | Required for RTP, ignored for MXL | Both |
| `mxl-domain-id` | `urn:x-nvnmos:tag:mxl-domain-id` flow-def tag | Required for MXL, ignored for RTP | Both |
| `mxl-flow-id` | flow_def top-level `id` (sender side; receiver side comes from activation) | Optional; defaults to derived from the sender's name | `nmossink` (MXL) |
| `label` | NMOS label — SDP `s=` line for RTP, `label` for MXL flow_def | Optional | Both |
| `description` | NMOS description — SDP `i=` line for RTP, `description` for MXL flow_def | Optional | Both |
| `receiver-caps-mode` | `auto` (default) / `narrow` / `wide` — controls whether IS-04 publishes narrow Receiver Caps derived from the transport file, or wide caps (`x-nvnmos-caps` present, permissive). `auto` leaves the `urn:x-nvnmos:tag:caps` tag untouched, so the result is narrow when the transport file is present and doesn't carry that tag, and wide when the tag is already present; `narrow` and `wide` force the tag in (or out of) the spliced transport file. | Optional | `nmossrc` only |

**Route C — `transport-file` + property overrides / cross-checks.** Provide a baseline `transport-file` and combine it with any of the Route B properties. The rule depends on the property:

- **Identity / cosmetic properties** (`sender-name` / `receiver-name`, `mxl-flow-id`, `mxl-domain-id`, `label`, `description`, `receiver-caps-mode`) — **override** the matching field/tag in the file. This is the natural "I have a template SDP or flow_def, but the per-instance bits (`sender-name` or `receiver-name`, `iface-ip`, port, label, ...) change" workflow, and nvdsnmosbin already worked this way.
- **Essence-shape properties** (`caps`, `transport-caps`) — **cross-check** against the file's shape. Mismatch is a hard error at NULL→READY; the application is asked to align the two rather than have one silently win over the other.

The full matrix (including which properties have no transport file interaction at all) lives in the gst-nmos-rs crate README under "Property interaction with `transport-file`".

#### Defaults the element synthesises

For RTP, the element fills the SDP from essence caps + per-format defaults + per-transport defaults. `transport-caps` only needs to be set to override one of these or to add an `fmtp` extra the element doesn't synthesise:

- **`media`**, **`encoding-name`** — from the essence caps top-level type and format (`video/x-raw` → `media=video, encoding-name=raw`; `audio/x-raw` PCM → `L16` for S16BE / `L24` for S24BE; `meta/x-st-2038` → `media=video, encoding-name=smpte291`; `video/x-jxsv` → `jxsv`).
- **`clock-rate`** — video / ANC / JXSV → `90000`; audio → `rate` from the essence caps.
- **`payload` (PT)** — by media: video → `96`, audio → `97`, ancillary → `98`. Override via `transport-caps` if a deployment requires specific PT values.
- **`depth`, `sampling`** — from the essence format string (`v210` / `UYVP` → `depth=10, sampling=YCbCr-4:2:2`; similar mappings for the other supported formats).
- **`colorimetry`** — from `video/x-raw, colorimetry=` if present (mapped to the ST 2110 form, e.g. `bt709` → `BT709`); otherwise `BT709`.
- **`PM` (Packing Mode)** — `2110GPM`. `2110BPM` requires explicit override.
- **`SSN`** — by essence: `ST2110-20:2017` / `ST2110-30:2017` / `ST2110-40:2018` / `ST2110-22:2022`.
- **`TP` (Traffic Profile)** — by `transport`: `nvdsudp` (Rivermax-paced) → `2110TPN` (narrow); `udp` (OSS) → `2110TPW` (wide).

For MXL the per-format mapping to `flow_def.json` is derivable from essence caps in the same way; `transport-caps` is essentially unused.

#### Deferred mode (`nmossink` only)

If neither `transport-file` nor `caps` is provided, `nmossink` defers NMOS registration to **READY → PAUSED** and derives `caps` from a `gst_pad_peer_query_caps` against upstream. Standard GStreamer caps negotiation does the work; the user can constrain with `capsfilter ! nmossink` the way they would with any other sink. `sender-name` (and `iface-ip` or `mxl-domain-id` as relevant) is still required as a property because it's not derivable from the pipeline. `nmossrc` cannot use deferred mode — there is no peer to query.

**No data needs to flow for the sender to register.** Caps negotiation in GStreamer is a query-based mechanism (pad template caps + `peer_query_caps`), not a buffer-driven one. For a typical flow-transform pipeline `nmossrc ! transform ! nmossink`:

- `nmossrc`'s source-pad caps are fixed from its `transport-file` at NULL→READY (these are the caps the receiver *would* receive, known the moment the session is opened — independent of whether the receiver has been activated by a controller or whether any data has arrived on the wire).
- They propagate downstream during READY→PAUSED.
- The deferred `nmossink`'s peer query lands on those caps and registers the sender with the daemon.
- The pipeline reaches PAUSED as part of any normal `set_state(PLAYING)` transition — typically in milliseconds, well before the receiver is activated or starts producing buffers.

Both endpoints are at IS-04 by the time the pipeline is up, and there is no Catch-22 (PAUSED ≠ streaming-to-network). The internal gating mechanism (Phase 1 as-built: the inner chain is a `fakesink` behind the anchor while deactivated — see [*Phase 1 as-built: anchor + block-probe*](#phase-1-as-built-anchor--block-probe); originally planned as `output-selector → fakesink`, per *Sink deactivation*) keeps the inner wire sink out of the data path until an IS-05 activation arrives.

**Where deferred mode does break:** any chain where an intermediate element determines its output caps from buffer content — `h264parse` needing SPS/PPS, decoders inferring colorimetry from packet headers, etc. The peer query at PAUSED can't fix caps if upstream hasn't seen data. For those pipelines, declare `caps` explicitly on `nmossink` (Route B) and the sender registers at NULL→READY instead, alongside the receiver.

**Timing subtlety to be aware of.** In deferred mode the sender registers one state transition after the receiver (PAUSED vs READY). A controller polling IS-04 during the narrow window between NULL→READY and READY→PAUSED will see the receiver listed but not the sender. Harmless in practice (the window is tens to hundreds of milliseconds for a normal pipeline startup), but if a deployment cares — for instance because a discovery scan is running in parallel with pipeline startup — declare `caps` explicitly and registration moves to READY for both.

#### Narrow vs wide caps (`receiver-caps-mode`)

In all three modes, the GStreamer pad caps are fixed at the format derived from the transport file (or declared via `caps`). The `receiver-caps-mode` property only controls what's advertised in IS-04:

- `receiver-caps-mode=auto` (default): the element leaves the `urn:x-nvnmos:tag:caps` tag untouched in the spliced transport file. The outcome therefore depends on the file: narrow when the transport file is present and the tag is absent, wide when the tag is already in the file.
- `receiver-caps-mode=narrow`: IS-04 publishes narrow Receiver Caps matching the transport file. The element forces the narrow path by removing any `urn:x-nvnmos:tag:caps` from the spliced transport file and rejects any activation carrying a structurally different transport file.
- `receiver-caps-mode=wide`: IS-04 publishes wide Receiver Caps (`x-nvnmos-caps` present, content per NvNmos's existing semantics). The element splices `urn:x-nvnmos:tag:caps = [""]` (the libnvnmos "present + non-empty" wide marker) into the transport file and accepts a wider set of incoming transport files; a structurally divergent one triggers a CAPS event renegotiation downstream (Phase 2+ work — see Phasing).

#### gst-launch examples

A simple MXL sender:

```bash
gst-launch-1.0 \
  videotestsrc is-live=true ! video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001 ! \
  nmossink \
    transport=mxl \
    daemon-uri=unix:/var/run/nvnmosd.sock \
    node-seed=my-node \
    sender-name=cam-1 \
    mxl-domain-id=8e2c1ab8-4bdd-46c2-9b8d-1d2a3c4e5f60 \
    label='Camera 1'
```

(`caps` not strictly required here — upstream caps from the `videotestsrc!filter` already specify v210 1080p30000/1001, and deferred mode picks them up at READY → PAUSED.)

A minimal RTP sender via OSS `udpsink` — no `transport-caps` needed, the element synthesises the SDP from upstream caps and defaults:

```bash
gst-launch-1.0 \
  videotestsrc is-live=true ! video/x-raw,format=UYVP,width=1920,height=1080,framerate=60/1,colorimetry=bt709 ! \
  nmossink \
    transport=udp \
    daemon-uri=unix:/var/run/nvnmosd.sock \
    node-seed=my-node \
    sender-name=cam-1 \
    iface-ip=10.0.0.1 \
    label='Camera 1'
```

The same sender wrapping `nvdsudpsink` (note `transport=nvdsudp` switches the inner-element family and the `TP` default flips from `2110TPW` to `2110TPN`), also overriding the default payload type and forcing Block Packing Mode via `transport-caps`:

```bash
gst-launch-1.0 \
  videotestsrc is-live=true ! video/x-raw,format=UYVP,width=1920,height=1080,framerate=60/1,colorimetry=bt709 ! \
  nmossink \
    transport=nvdsudp \
    daemon-uri=unix:/var/run/nvnmosd.sock \
    node-seed=my-node \
    sender-name=cam-1 \
    iface-ip=10.0.0.1 \
    label='Camera 1' \
    transport-caps='application/x-rtp, payload=99, PM=(string)2110BPM'
```

An RTP receiver with a template SDP and per-instance overrides. Reusing the sender's name above for the receiver is fine — the daemon scopes names by side so a Sender and a Receiver named `cam-1` on the same Node are distinct resources:

```bash
gst-launch-1.0 \
  nmossrc \
    transport=udp \
    daemon-uri=unix:/var/run/nvnmosd.sock \
    node-seed=my-node \
    transport-file="$(cat template.sdp)" \
    receiver-name=cam-rx-1 \
    iface-ip=10.0.0.1 \
    label='Receiver 1' ! \
  ...downstream...
```

#### Per-transport properties (out of scope here)

ST 2022-7 NIC selection, jitter-buffer overrides, and similar per-transport knobs live as per-transport properties; they're documented in their own phases. Sender timestamp regeneration is deferred until UDP/MXL inner-element semantics are pinned down (see "Sender timestamp modes" below).

### Lifecycle

- Pad set is **fixed at PLAYING**. Adding/removing elements (and thus NMOS resources) is supported while the pipeline is NULL, not while running.
- **NULL → READY**: open daemon session, subscribe to activations. Register the resource here too if `transport-file` or `caps` is present (Routes A / B / C).
- **READY → PAUSED**: per-transport prep on inner src/sink. For `nmossink` in deferred mode, this is where `caps` is derived from upstream peer caps query and the resource registered with the daemon.
- **PAUSED → PLAYING**: live data flow begins (see Source behaviour below). Wire transmission gated by activation regardless of pipeline state.

### Sink (`nmossink`) deactivation

> **Superseded in Phase 1.** The `output-selector` mechanism described below is the **originally-planned** design and was not adopted as-is. Phase 1 implementation replaced it with a permanent `identity` anchor + `IDLE | BLOCK_DOWNSTREAM` block-probe + chain swap, uniform across `nmossink` and `nmossrc`. See [*Phase 1 as-built: anchor + block-probe*](#phase-1-as-built-anchor--block-probe) below for the as-built mechanism and the reasons we diverged. The prose below is preserved as historical context.

Goal: keep consuming upstream so the rest of the pipeline isn't blocked; no transmission on the wire.

**Mechanism: `output-selector` → { inner wire sink | `fakesink sync=true async=false` }.** Active-pad flips on activation/deactivation. This is the pattern in `nvdsnmosbin/src/gstnvdssdpsink.cpp` (see the construction at the `output_selector` / `fakesink sync=true async=false` lines) and we adopt it verbatim because it solves two problems we'd otherwise hit:

- `sync=true` on the blackhole makes the fakesink behave as a clocked sink (so upstream live-timing isn't broken).
- `async=false` keeps the blackhole out of the bin's async-state preroll dance.
- The inner wire sink is removed from the data-flow when deactivated, so its internal pacing / packet scheduler (Rivermax for `nvdsudpsink`, MXL writer for `mxlsink`) doesn't see gappy input.

`valve` / `tee` alternatives were considered and rejected: `valve` drops buffers downstream-of-itself, which still leaves the inner sink in the data path and exposes it to gappy input; `tee` works but adds a buffer copy. The output-selector pattern is cheaper and aligns with the prior art.

### Source (`nmossrc`) deactivation and reconfiguration

> **Superseded in Phase 1.** The `input-selector` + flush-start/stop + (Phase 2+) pause-top-level-pipeline mechanism described below is the **originally-planned** design and was not adopted as-is. Phase 1 implementation replaced it with the same permanent `identity` anchor + block-probe + chain swap mechanism used on `nmossink`. The (A) / (B) / (C) problem decomposition below remains useful, and Phase 2 (`nvdsudp` / OSS `udp`) may still need to re-introduce some of the clock-renegotiation machinery; see [*Phase 1 as-built: anchor + block-probe*](#phase-1-as-built-anchor--block-probe) below for what shipped and the reasons we diverged. The prose below is preserved as historical context.

This is the hardest single design question. The prior art in `nvds_nmos_bin/src/gstnvdssdpsrc.cpp::react_to_sdp_change` evolved through three commits worth examining in order:

1. **Original pattern** (release/26.1): on every SDP change, divert flow via `input-selector → disabled_pad`, tear down the receiver pipeline (`set_state(NULL)`), pause the top-level pipeline (to force clock renegotiation off `nvdsudpsrc`), rebuild the receiver chain for the new SDP, link it back, switch the input-selector to the new pad, resume the top-level pipeline.
2. **MR 126 / `bugfix/lock-state-propagation`** (commit c834499) adds `gst_element_set_locked_state(receiver_pipeline, TRUE)` on the disable path. The in-code comment is precise: *"Without this, resuming the top-level pipeline (during another receiver's reconnect) would bring this receiver back to PLAYING, and the subsequent attempt to set it to NULL deadlocks against the streaming thread."* This is the multi-receiver coupling fix.
3. **MR 128 / `bugfix/5597128`** (commit 716db8d) extends the pause/flush/resume dance to cover the disable path too, factors out idempotent `pause_top_level_pipeline` / `resume_top_level_pipeline` helpers, and adds `flush-start` / `flush-stop` events downstream to unblock streaming threads before the receiver pipeline transitions to NULL.

Three distinct engineering problems are bundled in here:

- **(A) Inner-source teardown / rebuild on activation changes that need a different inner chain.** Tearing down a running source while downstream is consuming it deadlocks unless you first flush downstream (MR 128) and lock state of receivers that should remain inactive (MR 126).
- **(B) Clock renegotiation when the wire source's clock changes.** GStreamer re-selects its clock provider on the next NULL→PLAYING transition of the providing element; pausing+resuming the top-level pipeline is the standard cudgel because `nvdsudpsrc` is the clock provider.
- **(C) Multi-receiver coupling.** Resuming the top-level pipeline propagates state to all receivers, including disabled ones; hence MR 126.

Our design separates these problems by phase rather than trying to solve them all up front:

- **Phase 1 (MXL).** Problem (B) probably doesn't apply — `mxlsrc` reads from MXL shared memory and is not (we expect) the providing clock for the pipeline. We can avoid pausing the top-level pipeline entirely, and the workflow reduces to: divert via input-selector → flush-start downstream → lock-state + NULL the inner mxlsrc → (re)build for the new MXL flow → unlock-state + sync-state-with-parent → switch input-selector → flush-stop. The two MR fixes (locked-state on disable; downstream flush dance) are adopted from day one. If `mxlsrc` supports switching the flow id at runtime via a `GST_PARAM_MUTABLE_PLAYING` property, we may be able to skip the tear-down step entirely; that's an `mxlsrc` capability check, not a design choice for `nmossrc`.
- **Phase 2 (nvdsudpsrc).** Problem (B) returns; we adopt the full prior-art workflow with both MR 126 and MR 128 fixes folded in from the start. The open design question is whether pausing the top-level pipeline stays inside `nmossrc` (matching the prior art) or moves up the stack to somewhere with a global view of which receivers share a pipeline — defer until Phase 2 prototyping.
- **Phase 3+ (OSS `udpsrc` payloaders).** Lighter clock-provider story than `nvdsudpsrc`; revisit whether (B) applies on a case-by-case basis.

**Will multi-element composition change the calculus?** Possibly, and worth flagging explicitly. In `nvdsnmosbin` all receivers are pads on one bin, so they unavoidably share a top-level pipeline — `find_top_level_pipeline` always lands on the user's pipeline and the pause/resume blast radius is fixed by that topology. With independent `nmossrc` elements the application *chooses* the blast radius:

- Multiple `nmossrc` elements in the same user pipeline reproduces the prior-art situation: pausing for one bounces the others, so (A), (B), (C) and the MR 126 / MR 128 fixes apply unchanged.
- Putting each `nmossrc` in its own `GstBin` or its own top-level pipeline scopes the blast radius to that sub-pipeline. Clock renegotiation in one receiver no longer disturbs another, and (C) softens accordingly.
- Independent elements also make it natural to designate exactly one receiver as the pipeline's clock source (`provide-clock=true` on its inner element, `provide-clock=false` on the rest), decoupling (B) from incidental siblings.

We don't take a position on isolation strategy in Phase 1 — MXL avoids (B) regardless — but the lever exists, where it didn't in the bin form, and the documentation should make that explicit when we start writing user-facing element docs in Phase 2.

Per-format synthetic filler (e.g., black v210, silence) when an `nmossrc` is *inactive but should still produce something downstream* is a separate, opt-in concern via an `idle-mode` property. Available only for formats we can synthesize cheaply (raw audio, raw video); compressed essences (jxsv) don't get filler. Standard GStreamer gap mechanisms (`rtpjitterbuffer` GAP events, livesync, downstream `is-live` semantics) handle steady-state missing-data — they do **not** solve (A), (B), or (C) on their own.

### Phase 1 as-built: anchor + block-probe

Phase 1 implementation evolved a different (and simpler) mechanism than the `output-selector` / `input-selector` patterns prescribed above. Two issues drove the rework during bring-up:

1. **Sticky-event continuity across mid-stream IS-05 PATCH activations.** Switching the data path by retargeting a ghost pad (or by flipping a selector's active pad) loses STREAM_START / CAPS / SEGMENT in flight, leaving downstream with stale or absent caps. We observed this directly as a video freeze on the consumer side when a Sender was re-activated against a different MXL Flow id.
2. **`libmxl` per-process state release on re-activations.** An IS-05 activation against the same Flow id (`real → real` in the activation router's terms — for example, when a controller toggles `master_enable` without changing identity) needs an intermediate "fully released" state to let `libmxl`'s `FlowWriter` / `FlowReader` drop before the new instance tries to attach to the same shared-memory slot. Without this, the second instance silently keeps consuming/producing on stale handles.

#### The as-built mechanism

Uniform across `nmossink` and `nmossrc`:

- A permanent `identity` element — the **anchor** — sits behind a fixed ghost-pad target. The ghost is wired to the anchor's outer-facing pad once at construction and **never** retargeted. This is the invariant that solves (1): downstream is always linked to the anchor's external pad, and sticky events live there independent of whatever is wired behind the anchor.
- The **chain** (one of: fake chain, real `mxlsink` / `mxlsrc` chain) lives behind the anchor (`chain → anchor.sink_pad → anchor.src_pad → ghost.target` on the sink side, mirror-image on the source side).
- The activation handler runs on a `call_async` worker thread (so the daemon's gRPC client thread is never parked behind a state transition). It:
  1. Installs an `IDLE | BLOCK_DOWNSTREAM` probe on the anchor's chain-side pad.
  2. Unlinks the old chain from the anchor, sets it to `NULL`, removes it from the bin.
  3. Builds the new chain, adds it to the bin, links it to the anchor, syncs its state with the parent.
  4. Removes the probe.
  
  Sticky events re-flow to the new chain automatically on the next buffer push at the anchor — this is GStreamer's normal "next-buffer carries sticky events forward" semantic, which means e.g. `mxlsink::set_caps` fires before the first `render()`.
- For `real → real` re-activations the handler runs the swap twice under the same overall handler invocation: `real → fake → real`. This satisfies (2) — the old `FlowWriter` / `FlowReader` is fully dropped during the `fake` interlude before the new one attaches.

#### The fake chain

The fake chains keep the element valid in the pipeline while deactivated, replacing the `output-selector → blackhole` / `input-selector → disabled_pad` gating from the original design:

- **`nmossink`**: a `fakesink`, which accepts ANY caps. Equivalent role to the originally-planned `fakesink sync=true async=false` blackhole.
- **`nmossrc`**: a live `appsrc` with `format=Time` and caps set from the best-available source (the `caps` property, or caps derived from `transport-file*`). We never push buffers into it, so its basesrc loop blocks in `create()` while downstream caps queries are answered against the concrete essence shape — the pipeline can reach PLAYING while waiting for an IS-05 activation. At constructed-time (before any properties have been set) the `appsrc` is built without caps, and the NULL→READY transition replaces it with a caps-aware `appsrc` as soon as a caps source becomes available.

#### Where this design lands relative to the original (A) / (B) / (C) decomposition

Mapping back to the three engineering problems identified in *Source deactivation and reconfiguration* above:

- **(A) Inner-source teardown / rebuild on activation changes.** Solved by the block-probe: `IDLE | BLOCK_DOWNSTREAM` already gives us a quiet point with downstream paused, so no separate `flush-start` / `flush-stop` is needed in Phase 1.
- **(B) Clock renegotiation when the wire source's clock changes.** Doesn't bite in Phase 1 because `mxlsrc` doesn't provide the pipeline clock. Phase 2 (`nvdsudpsrc`) may need to re-introduce some of the pause-top-level-pipeline machinery from the original design; the anchor pattern composes with that orthogonally.
- **(C) Multi-receiver coupling.** Doesn't apply: independent `nmossrc` elements each have their own anchor and are mutually independent. The `gst_element_set_locked_state` workaround from `nvdsnmosbin` MR 126 is unnecessary.

#### Why we ended up here, in one sentence

The original design adopted the selector pattern verbatim from `nvdsnmosbin`. Phase 1 bring-up surfaced (1) sticky-event continuity issues that the selector pattern doesn't solve well (because downstream's peer pad changes on every flip), and (2) `libmxl` per-process state-reuse semantics that no design captured at the time the design doc was written. The anchor + block-probe + `real → fake → real` mechanism handles both natively, generalises across `nmossink` and `nmossrc`, and is the one described in the user-facing element README ([`rust/gst-nmos-rs/README.md`](../../../rust/gst-nmos-rs/README.md), *Status* section) and in [`rust/gst-nmos-rs/src/inner.rs`](../../../rust/gst-nmos-rs/src/inner.rs).

### Sender timestamp modes (deferred)

A `timestamp-mode` property (passthrough vs. regenerated wire timestamps) was sketched here but has been pulled out of Phase 1. The semantics differ between `mxlsink` (no regenerate path until the upstream element grows one) and the `udpsink` family (where regenerate maps to existing properties), and committing to a GObject surface before those mappings are concrete risks shipping a knob whose `regenerate` value silently no-ops on MXL. Will return as a per-transport property once both inner elements expose the corresponding knobs; an issue on the `mxl` repo will be filed at that point.

## Phasing

- **Phase 0 — Daemon + test client**. Build the foundation: `nvnmos-sys` (FFI bindings to `libnvnmos`), `nvnmos-rpc` (proto + generated stubs), and `nvnmosd` itself. The Phase 0 daemon is **Linux-first**, supports **multi-Node**, uses **UDS for local IPC (plain TCP `localhost` as a portable fallback)**, and implements **`SyncResourceState`** from the start — real (non-gst-launch) clients will need the out-of-band data-plane sync path, and bolting it on later complicates the daemon's threading model. Alongside the daemon, build a **Rust test client modelled on the existing `nvnmos-example`** (the C app in `src/main.c`): opens a session, registers a few MXL and RTP senders/receivers, drives through the same interactive stages (remove some, add back, observe activations, deactivate, destroy session), and exercises `OpenSession` / `AddSender` / `AddReceiver` / `SubscribeActivations` / `AckActivation` / `SyncResourceState` end-to-end. This proves the daemon works without GStreamer in the loop, and gives us a regression harness for every subsequent phase. (A C++ test client could be added later but isn't part of Phase 0. Cross-host TCP + TLS and Windows/macOS daemon targets are deferred to Phase N.)
- **Phase 1 — MXL (all essences)**: `nmossrc` + `nmossink` for the full set of MXL flows `gst-mxl-rs` already supports — `video/x-raw` v210, `audio/x-raw` F32LE, and `meta/x-st-2038` ANC (what MXL flow_def.json calls `video/smpte291`). Single Node, gst-launch demoable. No nvdsudp involvement (so no Rivermax requirement). All three routes from the Pad config section are live (`transport-file`, property route, deferred mode on `nmossink`); `transport-caps` is typically empty for MXL.
- **Phase 2 — ST 2110 via nvdsudp**: ST 2110-20 video, ST 2110-30 audio, and ST 2110-40 ANC together. ANC support in nvdsudp lands in the next DeepStream release (also mapped to `meta/x-st-2038` in GStreamer), so Phase 2 is designed ready for all three. Reuse the activation→property translation patterns identified by the lessons review. Extends the property-route synthesis to emit SDP from `caps` + `transport-caps` + top-level props, defaulting RTP-side fields when absent (PT 96, `encoding-name` from essence lookup). Ports `nvds_nmos_bin/src/helpers/sdp_helpers.{cpp,h}` and `sdp_caps_to_raw_caps.{cpp,h}` to drive both directions of the caps ↔ SDP conversion. Enables `receiver-caps-mode=wide` (wide-mode renegotiation) end-to-end.
- **Phase 3 — OSS `udpsrc`/`udpsink`** with payloaders. Same NMOS surface, different inner chain.
- **Phase 4 — `video/x-jxsv` (ST 2110-22)** once nvdsudp changes merge.
- **Phase 5 — ST 2022-7** redundancy (transport-flavour property surface; `nvdsudp` already supports it that way) - can't be implemented with OSS `udpsrc`/`udpsink`. **`video/v210a`** stub if GStreamer support is still missing.
- **Phase N — deferred**: cross-host gRPC + TLS; Windows + macOS daemon targets (Phase 0 designs to keep these tractable but doesn't ship them); optional non-gRPC fallback (JSON-RPC over WebSocket) only if there's a concrete adoption blocker.

## Risks called out

- **Connection API responsiveness**: slow elements stall PATCH responses. Mitigation: tight default `AckActivation` timeout; document the failure mode.
- **Daemon discovery**: `daemon-uri` property with a sensible default. Document; don't auto-spawn.
- **Daemon reconnect**: long-lived `SubscribeActivations` streams must reconnect cleanly across daemon restarts. Session id durable across reconnects; daemon keeps session state until explicit close or TTL.
- **Inner-sink state under gating**: the originally-planned `output-selector` pattern removes the inner sink from the data path on deactivation. We still need to confirm per-transport that `nvdsudpsink`/`mxlsink` cleanly handle being de-pathed at runtime; if either disagrees, fall back to keeping it on the path with `fakesink sync=true async=false` *downstream* of the inner sink instead of *replacing* it (slightly more expensive, more conservative). (Phase 1 update: the as-built mechanism tears down and rebuilds the inner sink across activations rather than de-pathing it; this has been validated against `mxlsink`. Phase 2 must still validate the same against `nvdsudpsink`. See [*Phase 1 as-built: anchor + block-probe*](#phase-1-as-built-anchor--block-probe).)
- **Cross-platform paths**: UDS on Linux/macOS, named pipe on Windows, TCP fallback. gRPC supports all three.
- **NvNmos C-API thread safety**: confirmed thread-safe — every state mutation in `nvnmos_impl.cpp` takes `model.write_lock()` before touching the nmos-cpp model. The daemon can call NvNmos from any worker thread; no daemon-side serialisation needed.

## Lessons from `nvds_nmos_bin` and `gst-mxl-rs`

Folded in from the focused review of `nvds_nmos_bin/src/` (and its bug-fix branches `bugfix/nvnmos`, `bugfix/fix-ds-patches`, `bugfix/dbus-system-bus-address`, `bugfix/avahi-capabilities`, `bugfix/cap_dac_read_search`, `bugfix/lock-state-propagation`), plus `mxl/rust/gst-mxl-rs/src/`. The findings that shape the design are recorded inline above; this section keeps the audit trail.

- **Sink gating** is `output-selector` + `fakesink sync=true async=false` (`gstnvdssdpsink.cpp` around the `output_selector` construction). Originally planned to be adopted verbatim. See *Sink deactivation*. (Phase 1 update: not adopted as-is — the chain-swap-behind-an-anchor mechanism in [*Phase 1 as-built: anchor + block-probe*](#phase-1-as-built-anchor--block-probe) achieves the same outcome via a different mechanism.)
- **Source regain-sync / reconfiguration** in the prior art is a three-commit story in `gstnvdssdpsrc.cpp::react_to_sdp_change`: input-selector + tear-down/rebuild + pause-the-top-level-pipeline (release/26.1), plus the locked-state fix (MR 126 / `bugfix/lock-state-propagation`) for multi-receiver coupling, plus the flush-event + idempotent-pause/resume refinement (MR 128 / `bugfix/5597128`). Originally planned to adopt the architecture *with both bug-fixes folded in from day one*, and avoid the clock-renegotiation pause/resume in Phase 1 because MXL doesn't drive the pipeline clock. See *Source deactivation and reconfiguration*. (Phase 1 update: the same chain-swap-behind-an-anchor mechanism applies — the MR 126 lock-state fix is unnecessary because independent `nmossrc` elements don't share state, and the MR 128 flush dance is unnecessary because the block-probe gives us an equivalent quiet point. See [*Phase 1 as-built: anchor + block-probe*](#phase-1-as-built-anchor--block-probe).)
- **Activation callback honesty**: `gstnvdsnmosbin.cpp::nmos_connection_callback` always returns `true`. Failures in downstream property changes were not surfaced to NvNmos / IS-05. The new design propagates `AckActivation` outcomes end-to-end (see *Activation flow*).
- **SDP-driven properties are `GST_PARAM_MUTABLE_PLAYING`** on `gstnvdssdpsink`/`gstnvdssdpsrc`. That's how activation flips inner-element configuration without a state cycle. Our `nmossrc`/`nmossink` keep the same property mutability for the per-transport properties that translate from activation events.
- **In-process re-invocation of the callback** (`gstnvdsnmosbin.cpp` at `nmos_connection_callback(&fake_server, id, patched_sdp)`) is how `nvdsnmosbin` simulates out-of-band updates. That use-case maps cleanly onto our `SyncResourceState` RPC; we shouldn't need the `fake_server` trick.
- **Caps mapping** in `helpers/sdp_caps_to_raw_caps.{cpp,h}` and `helpers/sdp_helpers.{cpp,h}` is reusable structurally — we port the SDP↔caps logic for Phase 2 (`udpsrc`/`nvdsudpsrc` paths) rather than re-derive it. MXL has no equivalent — it's a thin essence-plus-metadata wrapper, so caps↔`flow_def.json` is straightforward.
- **`gst-mxl-rs` API surface** (`mxlsrc`/`mxlsink` `imp.rs`): essence buffers in/out, no RTP meta to maintain; PTS-passthrough is supported, regenerate isn't yet (we file an issue when the design solidifies, see *Sender timestamp modes*). No surprise composition issues for Phase 1.

### Proposed `nmossrc` / `nmossink` state-machine table

> **Superseded in Phase 1.** The table below reflects the **originally-planned** `output-selector` / `input-selector` design. Phase 1 implementation does not separate the data path from the "Activated" state at this granularity: the chain *is* the data path, and the activation handler swaps it in place behind the anchor (see [*Phase 1 as-built: anchor + block-probe*](#phase-1-as-built-anchor--block-probe)). The table is preserved as historical context.

State refers to GStreamer element state; "Activated" tracks the latest IS-05 activation.

| State        | Activated | `nmossink` data path                                   | `nmossrc` data path                                                       | Notes |
|---           |---        |---                                                     |---                                                                        |---|
| `NULL`       | n/a       | (no resource)                                          | (no resource)                                                             | Pre-`OpenSession` |
| `READY`      | unknown   | selector built; selector → blackhole                   | input-selector built; input-selector → disabled_pad; no inner src yet     | Session open; resource registered if `transport-file`/`caps` known |
| `PAUSED`     | unknown   | as `READY` (deferred mode resolves `caps` here)        | as `READY`                                                                | Caps negotiation completes; no wire I/O |
| `PLAYING`    | inactive  | selector → blackhole                                   | input-selector → disabled_pad; inner src absent or `locked_state=TRUE` at NULL | No wire I/O for this element; siblings unaffected |
| `PLAYING`    | active    | selector → inner sink                                  | inner src built+linked via `sync_state_with_parent`; input-selector → receiver_pad | Wire I/O on |
| any → `NULL` | n/a       | `RemoveResource` + `CloseSession` (last element wins for the session) | same                                                       | Daemon refcounts session usage |

Transitions on `Activated` are driven by `ActivationEvent` messages; they do **not** require a GStreamer element state change on `nmossrc` / `nmossink` itself. The `nmossrc` activate transition runs the flush/rebuild/sync dance internally (see *Source deactivation and reconfiguration*). Transitions on element state are driven by GStreamer normally.

## Branch and rebase strategy

This plan is being developed on the `feature/nvnmosd` branch, which is branched off `feature/mxl` because Phase 1 depends on the MXL transport work in that branch. Rebase onto `main` once `feature/mxl` is merged, then continue Phase 1 work on `feature/nvnmosd`.

## Notes from the original framing

Preserved from the earlier scoping draft so the rationale isn't lost.

> My thinking is to make an nmosbin element that uses NvNmos and can be configured with source/sink pads which are implemented as a short chain of the above real source/sink and payload/depayload elements. Those source and sink pads can then be connected to video/audio/ancillary data processing pipelines, even allowing e.g. an ST 2110 receiver (represented as a source pad) to go through processing and be retransmitted (from a sink pad) as ST 2110, or similarly to build a simple pipeline for MXL-to-ST 2110 or vice-versa. This should be possible to set up entirely through a pipeline description, no additional programmatic APIs required so that useful functions can be built with just a `gst-launch-1.0` command.

> However, we have attempted this before (see nvdsnmosbin) and encountered various issues:
> - maintaining RTP timestamps from source to sink (nvdsudp now supports setting PTS based on RTP timestamps and carrying RTP timestamps on buffers as GstMeta)
> - dealing with the asynchronous nature of NMOS activations meaning that sources and sinks start and stop producing or consuming buffers in the GStreamer pipeline at different times, but we want to keep the whole pipeline running

> The existing nvdsnmosbin implementation has shim bin elements that are SDP-based that do that work of adapting NvNmos activations into property changes on the underlying nvdsudpsrc and nvdsudpsink elements. They also use various other elements to deal with some of the challenges mentioned above. We'll need to do something similar for MXL activations, and handle the OSS udpsrc/udpsink mode neatly.
