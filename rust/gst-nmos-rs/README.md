<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# gst-nmos-rs

GStreamer plugin (`nmos`) providing the `nmossrc` and `nmossink` elements,
talking to the `nvnmosd` NMOS daemon over gRPC. Design lives in
[`doc/designs/nvnmosd/README.md`](../../doc/designs/nvnmosd/README.md);
the workspace overview is in [`../README.md`](../README.md).

## Status

- `nmossrc` and `nmossink` are registered with their current property
  surface (visible via `gst-inspect-1.0 nmossink` and `gst-inspect-1.0 nmossrc`).
- `NULL→READY` opens a session against `nvnmosd` via gRPC over UDS
  and subscribes to activations; `READY→NULL` closes it.
- When `transport-file` is set, the element also calls `AddSender`
  (on `nmossink`) or `AddReceiver` (on `nmossrc`) so the resource is
  published in IS-04 and reachable by IS-05 controllers. When
  `transport-file` is unset the session is opened but no resource is
  registered.
- Activation events arriving on the subscription drive the inner
  data path. The element re-runs the `mxl-domain-id` / flow id
  cross-checks against the event's `transport_file` (for MXL
  receivers this is the daemon-spliced internal `flow_def` carrying
  the PATCHed `mxl_domain_id` / `mxl_flow_id`), then swaps the
  inner element between `mxlsink` / `mxlsrc` and the placeholder.
  Swaps run inline at state ≤ READY and via a single-shot IDLE pad
  probe at state ≥ PAUSED, following the idiomatic gst-plugins-rs
  pattern (`transcriberbin`, `fallbackswitch`). The activation is
  acked back to the daemon as `success=true` when the inner
  element was successfully brought up (or deactivation completed),
  and `success=false` with a `failure_reason` when it could not —
  most commonly because `mxl-domain-path` is unset on this host,
  or the event's `transport_file` mismatches a user-pinned
  `mxl-flow-id` / caps-derived flow format.
- When the resolved configuration pins a Domain path *and* a Flow id
  (plus a recognised essence shape on the receiver, supplied via
  `caps` or read from the transport_file's `format`), the inner data
  path is the real `mxlsink` / `mxlsrc` configured from those
  values. Otherwise the bin keeps a placeholder `fakesink` /
  `fakesrc` so the element remains valid in the pipeline.
- On `nmossink` the `transport-file` may be omitted in favour of the
  `caps` property: when the user supplies essence caps (`video/x-raw,format=v210,…`,
  `audio/x-raw,format=F32LE,…`, or `meta/x-st-2038,framerate=…`) plus
  `mxl-flow-id` and `sender-name`, the element synthesises a MXL
  `flow_def` JSON document matching the SDK reference shapes in
  [`mxl/lib/tests/data/`](https://github.com/dmf-mxl/mxl/tree/main/lib/tests/data)
  and feeds it to `AddSender` as it would a user-supplied
  transport-file. When both `transport-file*` and `caps` are set the
  file wins and the caps are ignored.
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
  a flow_def is in play. The transport_file (`transport-file*` at
  NULL→READY, or the daemon-spliced internal one at activation) is
  reverse-mapped to GStreamer caps (`video/x-raw,format=v210,…`,
  `audio/x-raw,format=F32LE,…`, `meta/x-st-2038,framerate=…`) and
  pinned by an internal `mxlsrc ! capsfilter` chain. Downstream caps
  queries see the concrete shape the flow will carry — this is what
  makes the canonical `nmossrc ! transform ! nmossink` pipeline work
  end-to-end at READY→PAUSED, since the deferred `nmossink`'s
  upstream peer query lands on those pinned caps and `AddSender`
  runs against the right flow_def. When no transport_file is
  available (development convenience with `mxl-domain-path` +
  `mxl-flow-id` + `caps` set but no flow_def supplied), the bare
  `mxlsrc` is used and its broad pad template propagates; the
  `caps` media-type still decides which `mxlsrc.{video,audio,data}-flow-id=`
  slot receives `mxl-flow-id`.

## Property surface

Set via the standard `prop=value` syntax in `gst-launch-1.0`.

Both elements:

| Property         | Type    | Required? | Notes |
| ---------------- | ------- | --------- | ----- |
| `daemon-uri`     | string  | optional  | gRPC endpoint. Only `unix:/path/to/sock` is currently supported. Default `unix:/tmp/nvnmosd.sock`. |
| `node-seed`      | string  | required  | NvNmos Node seed; sessions sharing this seed share a Node. |
| `transport`      | enum    | required  | Only `mxl` is currently supported. |
| `mxl-domain-id`  | string  | required for MXL (may be omitted if `mxl-domain-path` supplies it) | MXL Domain id (UUID) advertised in NMOS as `urn:x-nvnmos:tag:mxl-domain-id`. If `mxl-domain-path` points at a directory containing a `domain_def.json` (AMWA BCP-007-03 WIP) the file's `id` is used to populate this property when unset, or cross-checked against it when both are supplied (mismatch is an error). |
| `mxl-domain-path` | string | optional in this scaffold; effectively required once the inner `mxlsink`/`mxlsrc` is wired up | Local filesystem path identifying the MXL Domain on this host. If a `domain_def.json` is present in the directory its `id` is used to populate or cross-check `mxl-domain-id` (see above). The path itself will be consumed by the inner element's `domain=` property when the data path is wired up. |
| `label`          | string  | optional  | NMOS label. |
| `description`    | string  | optional  | NMOS description. |
| `transport-file` | string  | route-dependent | Literal contents of the IS-05 transport file (MXL `flow_def` JSON today; SDP later). Pass text, not a path. Convenient for programmatic callers; gst-launch users want `transport-file-path` instead. Mutually exclusive with `transport-file-path`. On `nmossink`, may be substituted by `caps`. |
| `transport-file-path` | string | route-dependent | Filesystem path read at NULL→READY into `transport-file`. Convenience for `gst-launch-1.0`, whose pipeline parser doesn't cope with multi-line / quote-heavy property values. Mutually exclusive with `transport-file`. |
| `caps`           | GstCaps | required when `transport-file*` is unset | Essence caps. Supported shapes (mirroring `mxlsink`'s pad template): `video/x-raw,format=v210,width=…,height=…,framerate=…[,interlace-mode=…]`; `audio/x-raw,format=F32LE,rate=…,channels=…`; `meta/x-st-2038,framerate=…` (the framerate must be present — set it upstream with a `capsfilter caps="meta/x-st-2038,framerate=30/1"` if needed). On `nmossink`, drives flow_def JSON synthesis when `transport-file*` is unset. On `nmossrc`, the media-type structure name (`video/x-raw` / `audio/x-raw` / `meta/x-st-2038`) decides which `mxlsrc.{video,audio,data}-flow-id=` slot receives `mxl-flow-id`. Cross-checked against the transport_file's `format` field when both are supplied. |
| `transport-caps` | GstCaps | optional  | Typically empty for MXL. |

`nmossink`-only:

| Property      | Type   | Required? | Notes |
| ------------- | ------ | --------- | ----- |
| `sender-name` | string | required  | NMOS Sender name within the Node (`x-nvnmos-name` SDP attribute or `urn:x-nvnmos:tag:name` flow-def tag). Unique among Senders on the Node; a Receiver on the same Node may share the same name (the daemon's `by_name` index is keyed on `(node_seed, side, name)`). |
| `mxl-flow-id` | string | required to instantiate inner `mxlsink` (else placeholder) | MXL flow id (UUID) fed into `mxlsink.flow-id=`. Cross-checked against the transport_file's top-level `id` when both are supplied. |

`nmossrc`-only:

| Property          | Type   | Required? | Notes |
| ----------------- | ------ | --------- | ----- |
| `receiver-name`   | string | required  | NMOS Receiver name within the Node (`x-nvnmos-name` SDP attribute or `urn:x-nvnmos:tag:name` flow-def tag). Unique among Receivers on the Node; a Sender on the same Node may share the same name. |
| `mxl-flow-id`     | string | required to instantiate inner `mxlsrc` (else placeholder) | MXL flow id (UUID) the inner `mxlsrc` should pull. Cross-checked against the transport_file's top-level `id` when both are supplied. Normally an NMOS Receiver learns this from IS-05 PATCH activation; setting it as a property is a development convenience. |
| `receiver-caps`   | bool   | optional  | Default `true`; narrow-mode rejection wired later. |

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
`mxl-domain-path=` to register the Sender via `AddSender` *and*
instantiate a real `mxlsink`:

```sh
GST_DEBUG=nmossink:5 gst-launch-1.0 -e \
    fakesrc num-buffers=10 ! \
    nmossink transport=mxl node-seed=demo sender-name=sender1 \
             mxl-domain-id=1ac254d9-c9be-475a-93a7-f80b9c1063a8 \
             mxl-domain-path=/var/lib/mxl/domain-a \
             transport-file-path=/tmp/sender1.flow_def.json
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
`transport_file`) is acked back with `success=false` and a
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
essence caps can be supplied directly:

```sh
GST_DEBUG=nmossink:5 gst-launch-1.0 -e \
    videotestsrc num-buffers=10 ! \
    video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001 ! \
    nmossink transport=mxl node-seed=demo sender-name=sender1 \
             mxl-domain-id=1ac254d9-c9be-475a-93a7-f80b9c1063a8 \
             mxl-domain-path=/var/lib/mxl/domain-a \
             mxl-flow-id=5fbec3b1-1b0f-417d-9059-8b94a47197ed \
             label="Studio A v210" \
             caps="video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001"
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
             mxl-flow-id=5fbec3b1-1b0f-417d-9059-8b94a47197ed
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
            transport-file-path=/tmp/recv1.flow_def.json ! \
    identity ! \
    nmossink transport=mxl node-seed=demo sender-name=send1 \
             mxl-domain-id=1ac254d9-c9be-475a-93a7-f80b9c1063a8 \
             mxl-domain-path=/var/lib/mxl/domain-a \
             mxl-flow-id=00000000-0000-0000-0000-00000000abcd
```

Expected: `nmossrc` advertises caps from `recv1.flow_def.json` on
its ghost src pad; `nmossink` (deferred) peer-queries them through
`identity` at READY→PAUSED, synthesises its own flow_def from those
caps, and calls `AddSender`. The daemon log shows both
`AddReceiver` (from `nmossrc`) and `AddSender` (from the deferred
`nmossink`) on the same node.
