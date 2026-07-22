<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Configuration

Each element creates its NMOS Sender or Receiver from a
[configuring transport file](https://nvidia.github.io/nvnmos/concepts.html#configuring-transport-files):
SDP for RTP/UDP or an MXL flow definition. The element can obtain this file
from `transport-file*`, synthesise it from other properties, or, for `nmossink`,
derive it from upstream caps.

## Configuration Choices

You can choose the activation policy separately from how the element obtains
the configuring transport file.

### Activation Policy

| Policy | Setting | Intended use |
| --- | --- | --- |
| Controller-managed | Leave `auto-activate=false` (the default) | Production systems where an IS-05 Controller decides when the data plane becomes active |
| Self-starting | Set `auto-activate=true` | Development and fixed pipelines that should begin processing without a Controller |

### How the Configuring Transport File Is Obtained

| Source | Set initially | Intended use |
| --- | --- | --- |
| Supplied transport file | `transport-file-path` or `transport-file` | Use an existing SDP or MXL flow definition with [NvNmos extensions](https://nvidia.github.io/nvnmos/transport-files.html#nvnmos-extensions-to-the-transport-file) |
| Synthesised from properties | `caps` plus the relevant RTP/UDP endpoints or MXL identifiers | Build the configuring SDP or MXL flow definition from element configuration |
| Upstream caps (`nmossink` only) | Omit `caps` and `transport-file*` | Defer until upstream caps arrive during preroll |

`transport` defaults to `udp`. Set `transport=mxl`, `udp2`, or `nvdsudp`
explicitly when selecting another transport implementation.

`transport-file` and `transport-file-path` are mutually exclusive. Explicit
element properties override corresponding values in a supplied transport file;
essence `caps` are cross-checked rather than substituted silently. See
[How Properties Are Combined](#how-properties-are-combined)
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
| MXL | `mxl-domain-path`, `mxl-domain-id`, `mxl-flow-id` | Select the local MXL domain and flow for `transport=mxl` |
| Human-readable metadata | `label`, `description`, `group-hint` | Set human-readable labels, descriptions, and grouping metadata for NMOS resources |
| Node and session | `daemon-uri`, `http-port`, `host-name`, `domain`, `registration-url`, `system-url` | Connect to the daemon and configure the NMOS Node; Node properties are taken from the first session that creates a shared `node-seed` |
| Inner-element overrides | `transport-properties`, `pay-properties`, `depay-properties` | Pass advanced properties to generated inner elements; payloader and depayloader overrides apply only to `udp` / `udp2` |

The `source-ip` and `destination-port` properties follow the corresponding IS-05
transport parameter semantics, so their Sender and Receiver meanings differ.
For a Sender, `source-ip` is the local egress address and
`destination-port` is remote. For a Receiver, `source-ip` is an optional remote
source-specific multicast filter and `destination-port` is the local listen port.

### Supported Caps Essence Shapes

When `transport-file*` is unset, `caps` drives synthesis of the configuring
transport file sent to the daemon:

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

### How Properties Are Combined

When `transport-file*` is set alongside other properties, the element applies
these rules to construct the configuring transport file sent to the daemon:

| Group         | Properties | Combination rule |
| ------------- | ---------- | ------------------ |
| Identity | `sender-name` / `receiver-name`, `mxl-flow-id`, `mxl-domain-id` | **Apply property value.** The element writes the property value into the configuring transport file, replacing the supplied value if present. |
| Human-readable metadata | `label`, `description`, `group-hint` | **Apply property value.** The element writes the property value into the configuring transport file, replacing the supplied value if present. |
| Receiver capabilities | `receiver-caps-mode` | **Apply property value.** The element writes the selected Receiver Caps marker into the configuring transport file, replacing the supplied marker if present. |
| Essence shape | `caps` | **Cross-check.** The caps must be compatible with the supplied file's essence. For MXL, the format family must agree; for RTP/UDP, the caps must intersect the SDP essence shape. Mismatch is a hard error at NULL→READY. |
| Bit rates | `format-bit-rate`, `transport-bit-rate` | **Cross-check or apply property values.** A missing bit rate is approximated separately for the SDP and property values. If the SDP and properties both specify bit rates, the resulting values must agree; if only the properties do, the property-derived values are applied. |
| RTP parameters | `transport-caps` | **Cross-check or apply property values.** Dynamic payload type, audio clock rate, and `a-ptime` / `a-maxptime` are applied to the configuring SDP. The remaining RTP and essence-shape fields must agree with the supplied SDP. |
| Activation policy | `auto-activate` | **Not in the configuring transport file.** Selects self-starting or Controller-managed activation. |
| Other configuration | `daemon-uri`, `node-seed`, `http-port`, `host-name`, `domain`, `registration-url`, `system-url`, `transport`, `mxl-domain-path`, `transport-properties`, `pay-properties`, `depay-properties` | **Not in the configuring transport file.** Node properties configure the NMOS Node; when sessions share a `node-seed`, the first session to create the Node determines those settings. The remaining properties control the daemon connection or configure the local data plane and its inner GStreamer elements. |

Bit rates are in kilobits per second and correspond to NMOS Flow and Sender
`bit_rate`. SDP may carry the transport rate in `b=AS:` and either rate in the
NvNmos extension `fmtp` parameters `x-nvnmos-format-bit-rate` and
`x-nvnmos-transport-bit-rate`.

`mxl-domain-id` is in the override group for the supplied file's tag, but is
still **cross-checked** against `<mxl-domain-path>/domain_def.json`
because that file identifies the MXL domain mounted at this path; a different
ID would be a host configuration error, not a value that can be overridden.

IS-05 activation values take precedence over corresponding start-up
configuration. For a Sender, active `transport_params` are reflected in its
`/transportfile`. For a Receiver, a Controller may also supply a
`transport_file`. In either case, the active essence must remain compatible
with `caps`; for example, an `nmossrc` configured for `v210` video rejects
an activation carrying audio.

## Lifecycle, Activation, and Property Changes

| Transition or event | User-visible effect |
| --- | --- |
| NULL→READY | Connect to the daemon and, if the element can prepare the configuring transport file, add the NMOS Sender or Receiver |
| READY→PAUSED | For a deferred `nmossink`, derive the configuring transport file from upstream caps and add the Sender |
| IS-05 activation | Build or replace the inner transport elements using the effective SDP or MXL flow definition delivered for activation |
| READY→NULL | Remove the Sender or Receiver and close the daemon session |

When neither `transport-file*` nor `caps` is set, `nmossink` defers
configuration until it can query upstream caps at READY→PAUSED. `nmossrc`
has no deferred mode; set `caps` or `transport-file*` before READY.

The element separates NMOS resource visibility from an active data plane. With
the default `auto-activate=false`, the Sender or Receiver is visible but waits
for an IS-05 activation before starting its data plane. With
`auto-activate=true`, the element starts the data plane as soon as it has
prepared the configuring transport file. This property does not change the
GStreamer pipeline state.

Set configuration properties while the element is in NULL unless the element
reference marks them as changeable in READY. A property that can be set in
READY is not necessarily applied immediately: the element reads it at the next
relevant lifecycle action. In particular, `mxl-domain-path`,
`transport-properties`, `pay-properties`, and `depay-properties` apply when the
inner data plane is next built.
