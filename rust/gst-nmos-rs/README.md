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
- Activation events arriving on the subscription are auto-acked with
  `success=true`; the plugin doesn't yet apply the activation to the
  data path.
- When the resolved configuration pins a Domain path *and* a Flow id
  (plus a Flow format on the receiver), the inner data path is the
  real `mxlsink` / `mxlsrc` configured from those values. Otherwise
  the bin keeps a placeholder `fakesink` / `fakesrc` so the element
  remains valid in the pipeline; a later step (IS-05 activation,
  upstream-caps deferred mode) will rebuild the inner element from
  richer state.
- On `nmossink` the `transport-file` may be omitted in favour of the
  `caps` property: when the user supplies essence caps (`video/x-raw,format=v210,…`,
  `audio/x-raw,format=F32LE,…`, or `meta/x-st-2038,framerate=…`) plus
  `mxl-flow-id` and `sender-name`, the element synthesises a MXL
  `flow_def` JSON document matching the SDK reference shapes in
  [`mxl/lib/tests/data/`](https://github.com/dmf-mxl/mxl/tree/main/lib/tests/data)
  and feeds it to `AddSender` as it would a user-supplied
  transport-file. When both `transport-file*` and `caps` are set the
  file wins and the caps are ignored. Receiver caps→flow_def
  synthesis is intentionally not wired yet — a Receiver's
  transport-file describes the Sender's flow, not its own essence
  caps; that path will land alongside deferred mode.

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
| `caps`           | GstCaps | route-dependent | Essence caps. On `nmossink`, when `transport-file*` is unset, used to synthesise the MXL `flow_def` JSON. Supported shapes (mirroring `mxlsink`'s pad template): `video/x-raw,format=v210,width=…,height=…,framerate=…[,interlace-mode=…]`; `audio/x-raw,format=F32LE,rate=…,channels=…`; `meta/x-st-2038,framerate=…` (the framerate must be present — set it upstream with a `capsfilter caps="meta/x-st-2038,framerate=30/1"` if needed). On `nmossrc`, accepted today but not yet used to drive flow_def synthesis. |
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
| `mxl-flow-format` | enum (`video` / `audio` / `data` / `unspecified`) | required to instantiate inner `mxlsrc` (else placeholder) | Picks which of `mxlsrc.{video,audio,data}-flow-id=` receives `mxl-flow-id`. Cross-checked against the transport_file's `format` field when both are supplied. |
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
matching `AddSender`. Any IS-05 PATCH activation against this
resource is auto-acked with `success=true`.

On `nmossrc` the inner `mxlsrc` also needs to know which media kind
the flow carries; supply it either via `mxl-flow-format=video|audio|data`
or via the `format` field of the `transport-file`.

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
