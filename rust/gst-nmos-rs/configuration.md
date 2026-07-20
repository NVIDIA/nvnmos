<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Configuration

Choose how the GStreamer element is activated and where its configuring
transport file comes from, then set the properties for the selected transport
and essence.

## Configuration Choices

Activation policy and the source of the configuring transport file are
independent choices.

### Activation Policy

| Policy | Setting | Intended use |
| --- | --- | --- |
| Controller-managed | Leave `auto-activate=false` (the default) | Production systems where an IS-05 Controller decides when the data plane becomes active |
| Self-starting | Set `auto-activate=true` | Development and fixed pipelines that should begin processing without a Controller |

### Configuring Transport File Source

| Source | Set initially | Intended use |
| --- | --- | --- |
| Supplied file | `transport-file-path` or `transport-file` | Use an existing SDP or MXL flow definition with [NvNmos extensions](https://nvidia.github.io/nvnmos/transport-files.html#nvnmos-extensions-to-the-transport-file) |
| Synthesised from properties | `caps` plus the relevant RTP/UDP endpoints or MXL identifiers | Build the configuring SDP or MXL flow definition from element configuration |
| Upstream caps (`nmossink` only) | Omit `caps` and `transport-file*` | Defer synthesis until upstream caps arrive during preroll |

`transport` defaults to `udp`. Set `transport=mxl`, `udp2`, or `nvdsudp`
explicitly when selecting another transport implementation.

`transport-file` and `transport-file-path` are mutually exclusive. Explicit
element properties override corresponding values in a supplied transport file;
essence `caps` are cross-checked rather than substituted silently. See
[Property Interaction With Transport Files](#property-interaction-with-transport-files)
for the complete rules.

## Property Groups

Use the plugin and element reference or `gst-inspect-1.0` for exact property
details:

- [`nmossink`](https://nvidia.github.io/nvnmos/gstreamer/nmos/nmossink.html)
- [`nmossrc`](https://nvidia.github.io/nvnmos/gstreamer/nmos/nmossrc.html)
- [`nmosaudiochannelmap`](https://nvidia.github.io/nvnmos/gstreamer/nmos/nmosaudiochannelmap.html)

Those references list properties alphabetically. Use these groups to decide
which properties matter for a task:

| Group | Properties | Purpose |
| --- | --- | --- |
| Essential | `node-seed`, `sender-name` / `receiver-name`, `transport`, `caps` or `transport-file*`, `receiver-caps-mode`, `auto-activate` | Identify the Node and Sender or Receiver, choose the data plane, describe the essence and Receiver capabilities, and choose Controller-managed or self-starting activation |
| RTP/UDP | Sender: `source-ip`, `source-port`, `destination-ip`, `destination-port`; Receiver: `source-ip`, `interface-ip`, `multicast-ip`, `destination-port`; both: `transport-caps`, `format-bit-rate`, `transport-bit-rate` | Configure SDP and IS-05 endpoint values for `udp`, `udp2`, and `nvdsudp`; the bit-rate properties apply to JPEG XS on `udp` / `udp2` |
| MXL | `mxl-domain-path`, `mxl-domain-id`, `mxl-flow-id` | Select the local MXL Domain and flow for `transport=mxl` |
| Human-readable metadata | `label`, `description`, `group-hint` | Set human-readable labels, descriptions, and grouping metadata for NMOS resources |
| Node and session | `daemon-uri`, `http-port`, `host-name`, `domain`, `registration-url`, `system-url` | Connect to `nvnmosd` and configure the NMOS Node; Node properties are taken from the first session that creates a shared `node-seed` |
| Inner-element overrides | `transport-properties`, `pay-properties`, `depay-properties` | Pass advanced properties to generated inner elements; payloader and depayloader overrides apply only to `udp` / `udp2` |

The Sender and Receiver meanings of `source-ip` and `destination-port` differ.
For a Sender, `source-ip` is the local egress address and
`destination-port` is remote. For a Receiver, `source-ip` is an optional remote
source-specific multicast filter and `destination-port` is the local listen port.

### Supported Caps Essence Shapes

When `transport-file*` is unset, `caps` drives synthesis of the configuring
transport file:

- `transport=mxl` produces an MXL flow definition and also requires
  `mxl-flow-id`.
- `transport=udp`, `udp2`, or `nvdsudp` produces SDP and uses the relevant
  endpoint properties.

On `nmossrc`, `receiver-caps-mode` controls whether the synthesised
configuration advertises constrained BCP-004-01 Receiver Caps. With
`transport=mxl`, the `caps` media type also selects the corresponding
`mxlsrc` video, audio, or data flow.

| Media | Caps shape | Transports | Notes |
| --- | --- | --- | --- |
| Video (raw) | `video/x-raw,format=…,width=…,height=…,framerate=…[,interlace-mode=…]` | all | MXL: `v210`. RTP/UDP: RFC 4175 8-bit `UYVY` and 10-bit `UYVP`. |
| Video (JPEG XS) | `image/x-jxsc,…` or `video/x-jxsv,…` | `udp` / `udp2` only | `width`, `height`, and `framerate` are required. Bit rates use `format-bit-rate` and `transport-bit-rate`, not caps fields. |
| Audio | `audio/x-raw,format=…,rate=…,channels=…` | all | MXL: `F32LE`. RTP/UDP: ST 2110-30 `S24BE` (L24) and `S16BE` (L16). |
| Data (ANC) | `meta/x-st-2038,framerate=…` | all | `framerate` is required; add it with a capsfilter if necessary. |

### Property Interaction With Transport Files

When a `transport-file` (literal or path) and an overlapping property
are both set, the resulting transport file handed to the daemon is
built with these rules:

| Group         | Properties | Rule when both set |
| ------------- | ---------- | ------------------ |
| Identity | `sender-name` / `receiver-name`, `mxl-flow-id`, `mxl-domain-id` | **Property overrides file.** The element rewrites the file's matching field/tag before the daemon sees it. |
| Human-readable metadata | `label`, `description`, `group-hint` | **Property overrides file.** The element rewrites the file's matching field/tag before the daemon sees it. |
| Receiver capabilities | `receiver-caps-mode` | **Property overrides file.** The element rewrites the file's Receiver Caps marker before the daemon sees it. |
| Essence shape | `caps`, `transport-caps` | **Cross-check.** Property must agree with the file's shape (today: `caps` first structure name vs `format`). Mismatch is a hard error at NULL→READY. |
| Bit rates | `format-bit-rate`, `transport-bit-rate` | **Cross-check when both declare a rate; splice when only the property is set.** Values are kilobits per second (matching NMOS `bit_rate`, SDP `b=AS:`, and fmtp `x-nvnmos-*-bit-rate` per AMWA BCP-006-01 / RFC 9134 / ST 2110-22). When the supplied SDP omits bit rates, non-zero properties are written into the configuring SDP before the daemon sees it. |
| Activation gate | `auto-activate` | Does not appear in the transport file. Controls whether the data plane starts when configuration is resolved or waits for an IS-05 activation. Independent of the configuration source. |
| No interaction | `daemon-uri`, `node-seed`, `http-port`, `host-name`, `domain`, `registration-url`, `system-url`, `transport`, `mxl-domain-path`, `transport-properties`, `pay-properties`, `depay-properties` | These don't appear in the transport file at all. Node-identity properties (`host-name`, `domain`, `registration-url`, `system-url`, `http-port`) are forwarded to `OpenSession` as `node_config` and honoured only when that session creates the Node (first opener for a given `node-seed`). `transport-properties` / `pay-properties` / `depay-properties` tune the inner GStreamer elements at chain-build time instead. |

`mxl-domain-id` is in the override group for the file tag, but is
still **cross-checked** against `<mxl-domain-path>/domain_def.json`
because that file describes which Domain identity belongs to this
local mount — a different ID would be a host-level misconfiguration,
not a labelling choice.

At IS-05 activation time the daemon's transport file is authoritative
for the override groups (an IS-05 PATCH legitimately replaces the
configured-at-startup flow id); the essence-shape cross-check
still applies, so an activation that asks an `nmossrc` configured for
v210 video to receive an audio flow is ack-failed.

## Lifecycle, Activation, and Property Changes

| Transition or event | User-visible effect |
| --- | --- |
| NULL→READY | Connect to `nvnmosd` and add the NMOS Sender or Receiver when its configuring transport file can be resolved |
| READY→PAUSED | For a deferred `nmossink`, derive configuration from upstream caps and add the Sender |
| IS-05 activation | Build or replace the inner transport elements using the active transport file |
| READY→NULL | Remove the Sender or Receiver and close the daemon session |

When neither `transport-file*` nor `caps` is set, `nmossink` defers
configuration until it can query upstream caps at READY→PAUSED. `nmossrc`
has no deferred mode; set `caps` or `transport-file*` before READY.

The element separates NMOS resource visibility from an active data plane. With
the default `auto-activate=false`, the Sender or Receiver is visible but waits
for an IS-05 activation before starting its data plane. With
`auto-activate=true`, it starts the data plane as soon as configuration is
available. This property does not change the GStreamer pipeline state.

Set configuration properties while the element is in NULL unless the element
reference marks them as changeable in READY. A property that can be set in
READY is not necessarily applied immediately: the element reads it at the next
relevant lifecycle action. In particular, `mxl-domain-path`,
`transport-properties`, `pay-properties`, and `depay-properties` apply when the
inner data plane is next built.
