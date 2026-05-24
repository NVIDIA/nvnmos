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
- The external pad is wired to a placeholder `fakesink` or `fakesrc`;
  there is no real MXL data path yet.

## Property surface

Set via the standard `prop=value` syntax in `gst-launch-1.0`.

Both elements:

| Property         | Type    | Required? | Notes |
| ---------------- | ------- | --------- | ----- |
| `daemon-uri`     | string  | optional  | gRPC endpoint. Only `unix:/path/to/sock` is currently supported. Default `unix:/tmp/nvnmosd.sock`. |
| `node-seed`      | string  | required  | NvNmos Node seed; sessions sharing this seed share a Node. |
| `transport`      | enum    | required  | Only `mxl` is currently supported. |
| `mxl-domain-id`  | string  | required for MXL | MXL Domain id (UUID) advertised in NMOS as `urn:x-nvnmos:tag:mxl-domain-id`. Independent of `mxl-domain-path` today; a follow-up will cross-check it against the `domain_def.json` at `mxl-domain-path` (per AMWA BCP-007-03 WIP). |
| `mxl-domain-path` | string | optional in this scaffold; effectively required once the inner `mxlsink`/`mxlsrc` is wired up | Local filesystem path identifying the MXL Domain on this host. Independent of `mxl-domain-id` today; consumed by the inner element's `domain=` property when the data path is wired up. |
| `label`          | string  | optional  | NMOS label. |
| `description`    | string  | optional  | NMOS description. |
| `transport-file` | string  | route-dependent | Literal contents of the IS-05 transport file (MXL `flow_def` JSON today; SDP later). Pass text, not a path. Convenient for programmatic callers; gst-launch users want `transport-file-path` instead. Mutually exclusive with `transport-file-path`. |
| `transport-file-path` | string | route-dependent | Filesystem path read at NULL→READY into `transport-file`. Convenience for `gst-launch-1.0`, whose pipeline parser doesn't cope with multi-line / quote-heavy property values. Mutually exclusive with `transport-file`. |
| `caps`           | GstCaps | route-dependent | Essence caps for the property route. |
| `transport-caps` | GstCaps | optional  | Typically empty for MXL. |

`nmossink`-only:

| Property      | Type   | Required? | Notes |
| ------------- | ------ | --------- | ----- |
| `sender-name` | string | required  | NMOS Sender name within the Node (`x-nvnmos-name` SDP attribute or `urn:x-nvnmos:tag:name` flow-def tag). Unique among Senders on the Node; a Receiver on the same Node may share the same name (the daemon's `by_name` index is keyed on `(node_seed, side, name)`). |
| `mxl-flow-id` | string | optional  | Override for the MXL flow id (top-level `id` in the flow_def). Defaults to a value derived from `sender-name`. |

`nmossrc`-only:

| Property        | Type   | Required? | Notes |
| --------------- | ------ | --------- | ----- |
| `receiver-name` | string | required  | NMOS Receiver name within the Node (`x-nvnmos-name` SDP attribute or `urn:x-nvnmos:tag:name` flow-def tag). Unique among Receivers on the Node; a Sender on the same Node may share the same name. |
| `receiver-caps` | bool   | optional  | Default `true`; narrow-mode rejection wired later. |

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
daemon to exercise the session lifecycle. Without `transport-file`
the element only opens a session:

```sh
# terminal 1
target/debug/nvnmosd

# terminal 2
export GST_PLUGIN_PATH=/path/to/nvnmos/rust/target/debug
GST_DEBUG=nmossink:5 gst-launch-1.0 -e \
    fakesrc num-buffers=10 ! \
    nmossink transport=mxl node-seed=demo sender-name=sender1 mxl-domain-id=1ac254d9-c9be-475a-93a7-f80b9c1063a8
```

Expected: the element logs `session opened ... no resource registered`
then `session closed`; the daemon logs the matching `OpenSession`,
`SubscribeActivations`, and `CloseSession` calls.

Add `transport-file-path=...` to additionally register the Sender via
`AddSender`. The transport file's `urn:x-nvnmos:tag:name` tag must
match `sender-name`:

```sh
GST_DEBUG=nmossink:5 gst-launch-1.0 -e \
    fakesrc num-buffers=10 ! \
    nmossink transport=mxl node-seed=demo sender-name=sender1 \
             mxl-domain-id=1ac254d9-c9be-475a-93a7-f80b9c1063a8 \
             transport-file-path=/tmp/sender1.flow_def.json
```

Expected: the element additionally logs `resource registered:
resource_handle=... resource_id=...`; the daemon logs the matching
`AddSender`. Any IS-05 PATCH activation against this resource is
auto-acked with `success=true`.

`transport-file` (literal text) remains available for programmatic
callers that compute the flow_def in memory; from gst-launch the path
form is much easier to type because the pipeline parser doesn't have
to cope with newlines and embedded quotes.
