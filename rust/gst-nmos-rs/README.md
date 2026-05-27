<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# gst-nmos-rs

GStreamer plugin (`nmos`) providing the `nmossrc` and `nmossink` elements,
talking to the `nvnmosd` NMOS daemon over gRPC. Design lives in
[`doc/designs/nvnmosd/README.md`](../../doc/designs/nvnmosd/README.md);
the workspace overview is in [`../README.md`](../README.md).

## Property surface

Set via the standard `prop=value` syntax in `gst-launch-1.0`.

Both elements:

| Property         | Type    | Required? | Notes |
| ---------------- | ------- | --------- | ----- |
| `daemon-uri`     | string  | optional  | gRPC endpoint. Only `unix:/path/to/sock` is currently supported. Default `unix:/tmp/nvnmosd.sock`. |
| `node-seed`      | string  | required  | NvNmos Node seed; sessions sharing this seed share a Node. |
| `http-port`      | uint (0–65535) | optional  | TCP port for libnvnmos's NMOS HTTP APIs (`node_config.http_port`). `0` (the default) leaves libnvnmos on the nmos-cpp per-API defaults (Node API on 3212, Connection API on 3215). Non-zero collapses every HTTP API onto this single port — handy for firewalled / port-mapped environments where one port is much easier to expose. Honoured only by the `OpenSession` that actually creates the Node; ignored (along with the rest of `node_config`) when this element attaches to a pre-existing Node (e.g. another `nmossink`/`nmossrc` opened first with the same `node-seed`). |
| `transport`      | enum    | required  | Only `mxl` is currently supported. |
| `transport-file` | string  | route-dependent | Literal contents of the NvNmos transport file (MXL `flow_def` JSON today; SDP later) the daemon will register with the resource and re-publish into IS-05. Pass text, not a path. Convenient for programmatic callers; gst-launch users want `transport-file-path` instead. Mutually exclusive with `transport-file-path`. May be substituted by `caps` (+ `mxl-flow-id`) on either element. |
| `transport-file-path` | string | route-dependent | Filesystem path read at NULL→READY into `transport-file`. Convenience for `gst-launch-1.0`, whose pipeline parser doesn't cope with multi-line / quote-heavy property values. Mutually exclusive with `transport-file`. |
| `label`          | string  | optional  | NMOS label for this Sender/Receiver (not the Node). Overrides the transport file's top-level `label` when both are supplied. |
| `description`    | string  | optional  | NMOS description for this Sender/Receiver. Overrides the transport file's top-level `description` when both are supplied. |
| `caps`           | GstCaps | required when `transport-file*` is unset | Essence caps. Supported shapes (mirroring `mxlsink`'s pad template): `video/x-raw,format=v210,width=…,height=…,framerate=…[,interlace-mode=…]`; `audio/x-raw,format=F32LE,rate=…,channels=…`; `meta/x-st-2038,framerate=…` (the framerate must be present — set it upstream with a `capsfilter caps="meta/x-st-2038,framerate=30/1"` if needed). On both elements, drives flow_def JSON synthesis when `transport-file*` is unset and `mxl-flow-id` is set (the Sender's flow_def describes the Flow it produces; the Receiver's *configuring* flow_def describes the essence shape this Receiver accepts, which the daemon advertises as BCP-004-01 narrow Receiver Caps on IS-04 — with `urn:x-nvnmos:tag:caps` driven by `receiver-caps-mode` to indicate narrow vs wide). On `nmossrc`, the media-type structure name (`video/x-raw` / `audio/x-raw` / `meta/x-st-2038`) also picks the `mxlsrc.{video,audio,data}-flow-id=` slot. Cross-checked against the transport file's `format` field when both are supplied. |
| `transport-caps` | GstCaps | optional  | Typically empty for MXL. |
| `mxl-domain-path` | string | required for MXL | Local filesystem path identifying the MXL Domain on this host; fed into the inner `mxlsink` / `mxlsrc` `domain=` property. If a `domain_def.json` is present in the directory its `id` is used to populate or cross-check `mxl-domain-id` (see below). |
| `mxl-domain-id`  | string  | required for MXL (may be omitted if `mxl-domain-path` supplies it) | MXL Domain id (UUID) advertised in NMOS as `urn:x-nvnmos:tag:mxl-domain-id`. If `mxl-domain-path` points at a directory containing a `domain_def.json` (AMWA BCP-007-03 WIP) the file's `id` is used to populate this property when unset, or cross-checked against it when both are supplied (mismatch is an error — this is host-level identity). Overrides the transport file's tag when both are supplied. |
| `mxl-flow-id`    | string  | optional  | MXL flow id (UUID) — on `nmossink` fed into `mxlsink.flow-id=`, on `nmossrc` into the matching `mxlsrc.{video,audio,data}-flow-id=` slot picked from `caps`. Overrides the transport file's top-level `id` when both are supplied — same property-override rule as `label` / `description`. |
| `auto-activate`  | boolean | optional, default `false` | When `false` the element registers the resource so it appears on IS-04 and IS-05 but leaves the inner data path on the placeholder until an IS-05 PATCH activates it (`master_enable: true` on `/single/{senders,receivers}/{id}/active`). When `true` the element brings the inner `mxlsink` / `mxlsrc` up immediately once the configuring flow_def has been resolved at NULL→READY (or, for a deferred-mode sender, at READY→PAUSED) *and* calls `SyncResourceState` to push the daemon's IS-04/IS-05 view to active — i.e. it's a no-controller shortcut for development pipelines and for setups where flow identity comes entirely from properties / `transport-file*`. Orthogonal to how the flow_def itself becomes available: property override of `mxl-flow-id`, supplied `transport-file*`, and caps→flow_def synthesis all feed the same gate. |

`nmossink`-only:

| Property      | Type   | Required? | Notes |
| ------------- | ------ | --------- | ----- |
| `sender-name` | string | required  | NMOS Sender name within the Node (`x-nvnmos-name` SDP attribute or `urn:x-nvnmos:tag:name` flow-def tag). Unique among Senders on the Node; a Receiver on the same Node may share the same name (the daemon's `by_name` index is keyed on `(node_seed, side, name)`). Overrides the transport file's name tag when both are supplied. |

`nmossrc`-only:

| Property          | Type   | Required? | Notes |
| ----------------- | ------ | --------- | ----- |
| `receiver-name`   | string | required  | NMOS Receiver name within the Node (`x-nvnmos-name` SDP attribute or `urn:x-nvnmos:tag:name` flow-def tag). Unique among Receivers on the Node; a Sender on the same Node may share the same name. Overrides the transport file's name tag when both are supplied. |
| `receiver-caps-mode` | enum (`auto`/`narrow`/`wide`) | optional | Controls whether the Receiver published to IS-04 advertises narrow or wide Receiver Caps, via the presence of the `urn:x-nvnmos:tag:caps` flow-def tag (libnvnmos's rule: present + non-empty array means wide; absent or empty means narrow). `auto` (default) leaves the tag untouched in the spliced transport file: narrow when the transport file is present and the tag is absent, wide when the tag is already there. `narrow` strips the tag if present; `wide` ensures it is present with a non-empty marker. |

### Property interaction with `transport-file`

When a `transport-file` (literal or path) and an overlapping property
are both set, the resulting transport file handed to the daemon is
built with these rules:

| Group         | Properties | Rule when both set |
| ------------- | ---------- | ------------------ |
| Identity / cosmetic | `sender-name` / `receiver-name`, `mxl-flow-id`, `mxl-domain-id`, `label`, `description`, `receiver-caps-mode` | **Property overrides file.** The element rewrites the file's matching field/tag to the property value before the daemon sees it. |
| Essence shape | `caps`, `transport-caps` | **Cross-check.** Property must agree with the file's shape (today: `caps` first structure name vs `format`). Mismatch is a hard error at NULL→READY. |
| Activation gate | `auto-activate` | Doesn't appear in the transport file; it gates whether the data path goes live eagerly at NULL→READY (and tells the daemon to flip `/active` to `master_enable: true` via `SyncResourceState`) or waits for an IS-05 PATCH. Orthogonal to where the flow_def came from. |
| No interaction | `daemon-uri`, `node-seed`, `http-port`, `transport`, `mxl-domain-path` | These don't appear in the transport file at all. |

`mxl-domain-id` is in the override group for the file tag, but is
still **cross-checked** against `mxl-domain-path/domain_def.json`
because that file describes which Domain identity belongs to this
local mount — a different ID would be a host-level misconfiguration,
not a labelling choice.

At IS-05 activation time the daemon's transport file is authoritative
for the identity/cosmetic group (an IS-05 PATCH legitimately replaces
the configured-at-startup flow id); the essence-shape cross-check
still applies, so an activation that asks an `nmossrc` configured for
v210 video to receive an audio flow is ack-failed.

### Activation: `auto-activate` vs IS-05 PATCH

The element separates "is the resource visible to NMOS controllers?"
from "is the data path live?":

- **Resource registration** (`AddSender` / `AddReceiver`) happens
  at NULL→READY whenever a configuring flow_def is in play —
  supplied via `transport-file*`, synthesised from `caps` (+
  `mxl-flow-id`), or for the deferred-mode sender, synthesised
  from peer caps at READY→PAUSED. With no flow_def in play the
  session opens with no resource and the data path stays on the
  placeholder until an IS-05 activation supplies one.

- **Inner data path** (real `mxlsink` / `mxlsrc`) only goes live
  when `auto-activate=true` *or* when an IS-05 activation arrives.
  With the default `auto-activate=false` the element registers the
  resource but leaves the inner on the placeholder; the daemon's
  `/single/{senders,receivers}/{id}/active` shows
  `master_enable: false` until an external controller PATCHes the
  resource. Setting `auto-activate=true` is the no-controller
  shortcut: the element brings the inner up eagerly from its
  resolved configuring flow_def and calls `SyncResourceState` on
  the daemon to bring `/active` into sync — no IS-05 PATCH
  required.

## Building

```sh
cd /path/to/nvnmos/rust
cargo build -p gst-nmos-rs
```

Build output is `target/debug/libgstnmos.so` (or `target/release/...`).

## Loading the plugin

```sh
export GST_PLUGIN_PATH=/path/to/nvnmos/rust/target/debug
gst-inspect-1.0 nmos
```

`gst-inspect-1.0 nmos` prints the plugin metadata;
`gst-inspect-1.0 nmossink` and `gst-inspect-1.0 nmossrc` list the
property surface above.

## Smoke test

Drive an element through `NULL`→`PLAYING`→`NULL` against a live
daemon to exercise the session lifecycle.

Without `mxl-domain-path` (and `mxl-flow-id`) the element opens a
session but its data path stays on the placeholder:

```sh
# terminal 1
target/debug/nvnmosd

# terminal 2
export GST_PLUGIN_PATH=/path/to/nvnmos/rust/target/debug:/path/to/mxl/rust/target/debug
export LD_LIBRARY_PATH=/path/to/mxl-runtime/lib
GST_DEBUG=nmossink:5 gst-launch-1.0 -e \
    fakesrc num-buffers=10 ! \
    nmossink transport=mxl node-seed=demo sender-name=sender1 mxl-domain-id=1ac254d9-c9be-475a-93a7-f80b9c1063a8
```

Expected: `session opened ... no resource registered; inner data path:
placeholder (...)` then `session closed`. The daemon logs the
matching `OpenSession`, `SubscribeActivations`, and `CloseSession`
calls.

Add `transport-file-path=...` (or `mxl-flow-id=` directly) plus
`mxl-domain-path=` to register the Sender via `AddSender`, then add
`auto-activate=true` to also instantiate a real `mxlsink` and have
the element call `SyncResourceState` so the daemon's
`/single/senders/{id}/active` is in sync without an IS-05 PATCH:

```sh
GST_DEBUG=nmossink:5 gst-launch-1.0 -e \
    fakesrc num-buffers=10 ! \
    nmossink transport=mxl node-seed=demo sender-name=sender1 \
             mxl-domain-id=1ac254d9-c9be-475a-93a7-f80b9c1063a8 \
             mxl-domain-path=/var/lib/mxl/domain-a \
             transport-file-path=/tmp/sender1.flow_def.json \
             auto-activate=true
```

Expected: the element additionally logs `resource registered:
resource_handle=... resource_id=...; inner data path: mxl
(domain_path=..., flow_id=..., format=...)`. The daemon logs the
matching `AddSender`. An IS-05 PATCH activation against this
resource is dispatched through the element: it logs `applying
activation … plan inner=Mxl(…), ack=Success` and (when the
pipeline is past READY) the swap happens behind a single-shot IDLE
pad probe before the daemon receives the success ack. A
deactivation logs `activation is a deactivation … swapping to
placeholder` and acks success. A PATCH that the element can't
honour locally (e.g. `mxl-domain-path` is unset on this host, or
the `mxl-flow-id` property contradicts the activation's
transport file) is acked back with `success=false` and a
`failure_reason` that names the specific check that failed.

On `nmossrc` the inner `mxlsrc` also needs to know which media kind
the flow carries — `video/x-raw` → `video-flow-id`, `audio/x-raw` →
`audio-flow-id`, `meta/x-st-2038` → `data-flow-id`. Supply it either
via `caps="…"` (which is also pinned on the ghost source pad so
downstream sees the concrete essence shape) or via the `format`
field of the `transport-file`. When both are supplied they must
agree.

The same `caps` discipline applies to a future ST 2110 transport
(`udpsrc ! depayloader ! …`): the application either declares
`caps=…` on `nmossrc` / `nmossink` to drive flow-format selection and
flow_def synthesis from properties, or provides a `transport-file`
(which is then authoritative and the caps are taken from it).

`transport-file` (literal text) remains available for programmatic
callers that compute the flow_def in memory; from gst-launch the path
form is much easier to type because the pipeline parser doesn't have
to cope with newlines and embedded quotes.

For a sender driven entirely by properties (no `transport-file*`) the
essence caps can be supplied directly; add `auto-activate=true` to
let the element activate without an IS-05 controller:

```sh
GST_DEBUG=nmossink:5 gst-launch-1.0 -e \
    videotestsrc num-buffers=10 ! \
    video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001 ! \
    nmossink transport=mxl node-seed=demo sender-name=sender1 \
             mxl-domain-id=1ac254d9-c9be-475a-93a7-f80b9c1063a8 \
             mxl-domain-path=/var/lib/mxl/domain-a \
             mxl-flow-id=5fbec3b1-1b0f-417d-9059-8b94a47197ed \
             label="Studio A v210" \
             caps="video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001" \
             auto-activate=true
```

Expected: `nmossink: synthesised flow_def from caps` then the usual
`resource registered`, `inner data path: mxl (…)`, and the daemon's
matching `AddSender`. The synthesised JSON follows the MXL SDK
reference flow shapes in [`mxl/lib/tests/data/`](https://github.com/dmf-mxl/mxl/tree/main/lib/tests/data).
Fields included:

- Caps-driven: `media_type`, `grain_rate` / `sample_rate`, `frame_width` /
  `frame_height` (video), `channel_count` / `bit_depth` (audio),
  `interlace_mode` (video, only when caps carry it).
- Property-driven: `id` (= `mxl-flow-id`), `label` (= `label` property,
  falls back to `sender-name` when empty), `description` (= `description`
  property, may be empty), plus three required tags
  (`urn:x-nmos:tag:grouphint/v1.0` derived from `sender-name`,
  `urn:x-nvnmos:tag:name` = `sender-name`,
  `urn:x-nvnmos:tag:mxl-domain-id` = the resolved `mxl-domain-id`).
- Video-only defaults required by `libnvnmos`: `colorspace` = `BT709`
  and a Y/Cb/Cr 4:2:2 10-bit `components` triple derived from
  `frame_width` / `frame_height`. Use `transport-file` if you need
  BT2020 / a different layout.

Finally, `nmossink` can also defer registration to `READY→PAUSED`
and pick up the caps from upstream — useful when the upstream
element fixes caps for the sink anyway (a `capsfilter`, a parser,
or another negotiation point):

```sh
GST_DEBUG=nmossink:5 gst-launch-1.0 -e \
    videotestsrc num-buffers=10 ! \
    video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001 ! \
    nmossink transport=mxl node-seed=demo sender-name=sender1 \
             mxl-domain-id=1ac254d9-c9be-475a-93a7-f80b9c1063a8 \
             mxl-domain-path=/var/lib/mxl/domain-a \
             mxl-flow-id=5fbec3b1-1b0f-417d-9059-8b94a47197ed \
             auto-activate=true
```

Expected: at NULL→READY the element logs `session opened … no
resource registered`; at READY→PAUSED it logs `deferred mode: peer
caps fixated to …` then `deferred mode: synthesised flow_def` and
`deferred registration complete: resource_handle=… resource_id=…;
inner data path: Mxl(…)`. When upstream can't fix caps — for
example `fakesrc ! nmossink` — the state change fails with a clear
`READY→PAUSED deferred registration failed:` error telling the
user to declare `caps=…` on the element or insert a `capsfilter`
upstream. Receiver-side deferred mode is intentionally out of scope:
`nmossrc` has no peer to query.

The two pieces compose into the canonical receiver-to-sender shape:

```sh
GST_DEBUG=nmossrc:5,nmossink:5 gst-launch-1.0 -e \
    nmossrc transport=mxl node-seed=demo receiver-name=recv1 \
            mxl-domain-id=1ac254d9-c9be-475a-93a7-f80b9c1063a8 \
            mxl-domain-path=/var/lib/mxl/domain-a \
            transport-file-path=/tmp/recv1.flow_def.json \
            auto-activate=true ! \
    identity ! \
    nmossink transport=mxl node-seed=demo sender-name=send1 \
             mxl-domain-id=1ac254d9-c9be-475a-93a7-f80b9c1063a8 \
             mxl-domain-path=/var/lib/mxl/domain-a \
             mxl-flow-id=00000000-0000-0000-0000-00000000abcd \
             auto-activate=true
```

Expected: `nmossrc` advertises caps from `recv1.flow_def.json` on
its ghost src pad; `nmossink` (deferred) peer-queries them through
`identity` at READY→PAUSED, synthesises its own flow_def from those
caps, and calls `AddSender`. The daemon log shows both
`AddReceiver` (from `nmossrc`) and `AddSender` (from the deferred
`nmossink`) on the same node.

## Multi-flow pipelines

Multiple Senders and Receivers can live on the same Node by sharing
the `node-seed`; the daemon's session index is keyed on
`(node_seed, side, name)` so distinct `sender-name` / `receiver-name`
values disambiguate them. The recipe below sets up the canonical
"video + ANC" producer/consumer pair (modelled on the
`gst-mxl-rs` `video_data_sync` integration test): the producer runs
`appsrc` through `st2038extractor` and feeds both branches into
their own `nmossink`; the consumer reads both flows back through
their own `nmossrc` and drops them into per-flow `appsink`s.

```text
producer pipeline
    appsrc (v210 + GstAncillaryMeta)
      ! st2038extractor name=ext remove-ancillary-meta=true
    ext.src    ! queue ! nmossink (sender-name=video-sender, video flow)
    ext.st2038 ! queue ! nmossink (sender-name=data-sender,  data flow)

consumer pipeline
    nmossrc (receiver-name=video-receiver, video flow) ! queue ! appsink (v210)
    nmossrc (receiver-name=data-receiver,  data flow)  ! queue ! appsink (meta/x-st-2038)
```

A self-contained `gst-launch-1.0` form using `videotestsrc` +
`audiotestsrc` (so no `appsrc`/`appsink` programming is required;
this exercises the multi-flow registration path even though it
doesn't drive synthesised ANC) is in the smoke-test plan at
`doc/designs/nvnmos-example-smoke-test-plan/gst-nmos-rs-smoke.sh`
(stage 15) — see that file for the full producer/consumer
invocations. The rigorous version with real `appsrc`/`appsink`
plumbing and per-frame index validation lives in
[`tests/multi_flow_video_data.rs`](tests/multi_flow_video_data.rs)
and is `#[ignore]`d because it needs the real MXL toolchain.

To opt in to the integration test on a host with `/dev/shm` and
the full MXL runtime:

```sh
export NVNMOS_LIB_DIR=/path/to/nvnmos-build/   # contains libnvnmos.so
export MXL_PLUGIN_DIR=/path/to/mxl/rust/target/debug
export MXL_RT_LIB_DIR=/path/to/mxl/build/lib

# Build nvnmosd + the gst-nmos-rs plugin.
cargo build --manifest-path /path/to/nvnmos/rust/Cargo.toml \
    -p nvnmosd -p gst-nmos-rs

TARGET_DIR=/path/to/nvnmos/rust/target

NVNMOSD_BIN=$TARGET_DIR/debug/nvnmosd \
GST_PLUGIN_PATH=$TARGET_DIR/debug:$MXL_PLUGIN_DIR \
LD_LIBRARY_PATH=$NVNMOS_LIB_DIR/lib:$MXL_RT_LIB_DIR \
cargo test --manifest-path /path/to/nvnmos/rust/Cargo.toml \
    -p gst-nmos-rs --test multi_flow_video_data \
    -- --ignored --test-threads=1 --nocapture
```

The test spawns its own `nvnmosd`, creates a fresh `/dev/shm`
domain (auto-removed on drop), writes one configuring `flow_def.json`
per role (sender / receiver) per flow (video / data), runs the
two pipelines, pulls 30 samples from each consumer `appsink`, and
asserts that the producer frame index stamped on every v210 buffer
and every ST 2038 ANC packet appears on both sides — proving the
two flows traverse the same MXL Domain on the same daemon Node and
that the per-flow PTS gap between the two flows stays constant
across the steady-state window.

## Status

- `nmossrc` and `nmossink` are registered with their current property
  surface (visible via `gst-inspect-1.0 nmossink` and `gst-inspect-1.0 nmossrc`).
- `NULL→READY` opens a session against `nvnmosd` via gRPC over UDS
  and subscribes to activations; `READY→NULL` closes it.
- When a transport file is in play — either supplied via
  `transport-file*` or synthesised from `caps` + `mxl-flow-id` — the
  element also calls `AddSender` (on `nmossink`) or `AddReceiver`
  (on `nmossrc`) so the resource is published in IS-04 and reachable
  by IS-05 controllers. When neither source provides one the session
  is opened but no resource is registered; the element awaits an
  IS-05 activation (or, for `nmossink` only, READY→PAUSED peer-caps
  resolution — see the deferred-mode note below).
- The `auto-activate` boolean property (default `false`) controls
  whether the data path goes live eagerly at NULL→READY or waits for
  an IS-05 PATCH. Default `false` gives canonical NMOS semantics:
  the resource is registered (visible on IS-04) but the inner
  data path stays on the placeholder until an external controller
  PATCHes `master_enable: true` against the
  `/single/{senders,receivers}/{id}/staged` endpoint. `true` is the
  no-controller shortcut: once the configuring flow_def has been
  resolved the element brings the inner `mxlsink` / `mxlsrc` up
  and calls `SyncResourceState` on the daemon so
  `/single/{senders,receivers}/{id}/active` reflects
  `master_enable: true` without the IS-05 stream being involved.
  The gate is orthogonal to how the flow_def became available —
  property override of `mxl-flow-id`, supplied `transport-file*`,
  and caps→flow_def synthesis all feed the same toggle.
- Activation events arriving on the subscription drive the inner
  data path. The element reads the event's transport file (for MXL
  receivers this is the daemon-spliced internal `flow_def` carrying
  the PATCHed `mxl_domain_id` / `mxl_flow_id`), then swaps the
  inner element between `mxlsink` / `mxlsrc` and the placeholder.
  The daemon's view is authoritative for identity — an IS-05 PATCH
  legitimately replaces the configured-at-startup `mxl-flow-id` /
  `mxl-domain-id` and the element silently picks up the new values.
  The essence-shape cross-check still applies, so an activation
  that tries to push an incompatible essence type at the element
  (e.g. a v210 video flow at an `nmossrc` configured for audio
  caps) is ack-failed. Swaps run inline at state ≤ READY and via a
  single-shot IDLE pad probe at state ≥ PAUSED, following the
  idiomatic gst-plugins-rs pattern (`transcriberbin`,
  `fallbackswitch`). The activation is acked back to the daemon as
  `success=true` when the inner element was successfully brought up
  (or deactivation completed), and `success=false` with a
  `failure_reason` when it could not — most commonly because
  `mxl-domain-path` is unset on this host or the essence-shape
  cross-check failed.
- When the resolved configuration pins a Domain path *and* a Flow id
  (plus a recognised essence shape on the receiver, supplied via
  `caps` or read from the transport file's `format`), the inner data
  path is the real `mxlsink` / `mxlsrc` configured from those
  values. Otherwise the bin keeps a placeholder `fakesink` /
  `fakesrc` so the element remains valid in the pipeline.
- Both elements support a `caps`-driven flow_def synthesis path:
  when the user supplies essence caps (`video/x-raw,format=v210,…`,
  `audio/x-raw,format=F32LE,…`, or `meta/x-st-2038,framerate=…`)
  plus `mxl-flow-id` and `sender-name` / `receiver-name`, the element
  synthesises a MXL `flow_def` JSON document matching the SDK
  reference shapes in
  [`mxl/lib/tests/data/`](https://github.com/dmf-mxl/mxl/tree/main/lib/tests/data)
  and feeds it to `AddSender` / `AddReceiver` as it would a
  user-supplied transport-file. On `nmossrc` the synthesised flow_def
  describes the Receiver's expected essence shape, which the daemon
  publishes as BCP-004-01 narrow Receiver Caps on IS-04 (with the
  `urn:x-nvnmos:tag:caps` tag spliced in by `receiver-caps-mode` to
  indicate narrow vs wide); the live transport file delivered later
  via IS-05 PATCH replaces only the subscription-relevant fields.
  When both `transport-file*` and `caps` are set, `caps` is
  cross-checked against the file's `format` rather than ignored —
  see the property interaction matrix in "Property interaction with
  `transport-file`" above.
- `nmossink` also supports a *deferred mode*: when neither
  `transport-file*` nor `caps` is supplied at NULL→READY the session
  opens without a resource, and the actual `AddSender` is driven
  from `READY→PAUSED`. The ghost sink pad's upstream peer is queried
  for caps via `gst_pad_peer_query_caps()`, the result is fixated,
  and the caps-driven flow_def builder runs against those caps; on
  success the inner element swaps to `mxlsink` and the resource is
  registered. `mxl-flow-id` / `mxl-domain-id` (or
  `mxl-domain-path` with a `domain_def.json`) must still be set. If
  the peer returned ANY/EMPTY caps or a shape the builder can't
  accept (e.g. `video/x-raw,format=I420`), the state change fails
  with a clear message telling the user to declare `caps=…` or insert
  a `capsfilter` upstream. Receiver-side deferred mode is
  intentionally out of scope — `nmossrc` has no peer to query.
- `nmossrc` advertises essence caps on its ghost source pad whenever
  a flow_def is in play. The transport file (`transport-file*` at
  NULL→READY, or the daemon-spliced internal one at activation) is
  reverse-mapped to GStreamer caps (`video/x-raw,format=v210,…`,
  `audio/x-raw,format=F32LE,…`, `meta/x-st-2038,framerate=…`) and
  pinned by an internal `mxlsrc ! capsfilter` chain. Downstream caps
  queries see the concrete shape the flow will carry — this is what
  makes the canonical `nmossrc ! transform ! nmossink` pipeline work
  end-to-end at READY→PAUSED, since the deferred `nmossink`'s
  upstream peer query lands on those pinned caps and `AddSender`
  runs against the right flow_def. When no transport file is
  available (development convenience with `mxl-domain-path` +
  `mxl-flow-id` + `caps` set but no flow_def supplied), the bare
  `mxlsrc` is used and its broad pad template propagates; the
  `caps` media-type still decides which `mxlsrc.{video,audio,data}-flow-id=`
  slot receives `mxl-flow-id`.
