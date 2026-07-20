<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Configuring Transport Files

See [Core NvNmos Concepts](concepts.md#configuring-transport-files) for how a
configuring transport file differs from an IS-05 transport file and the active
SDP or MXL flow definition delivered during activation.

## NvNmos Extensions to the Transport File

NvNmos uses a small set of extensions in the transport file to convey configuration that the standard transport file format does not carry. The same conceptual extensions are carried differently in the two transport file formats:

- For RTP/UDP (SDP), as custom `a=x-nvnmos-*:<value>` attributes.
- For MXL flow definitions (JSON), as entries in the standard `tags` property keyed by `urn:x-nvnmos:tag:*` URN strings. The tag's value is an array of strings; generally only the first element is used.

| Concept                  | SDP attribute (RTP/UDP)    | MXL flow_def tag key (MXL)              | Applies to                                | Description                                                                                                                |
| ---                      | ---                        | ---                                     | ---                                       | ---                                                                                                                        |
| Name                     | `a=x-nvnmos-name:<v>`      | `urn:x-nvnmos:tag:name`                 | Senders and Receivers (required)          | The application's caller-chosen name for the Sender or Receiver, unique within the Node for the given side (Sender or Receiver). A Sender and a Receiver may share the same name. Used in all NvNmos API callbacks (paired with the `NvNmosSide`) |
| Group hint               | `a=x-nvnmos-group-hint:<v>`| standard `urn:x-nmos:tag:grouphint/v1.0`| Senders and Receivers (optional)          | A group hint tag advertised via `urn:x-nmos:tag:grouphint/v1.0` on the NMOS resource                                       |
| Unconstrained Receiver Caps | `a=x-nvnmos-caps:<pt> [<constraints>]` (media-level) | `urn:x-nvnmos:tag:caps` | Receivers (optional) | Marks the Receiver as unconstrained, with format-derived Capabilities omitted. For SDP, `<pt>` is required and identifies the RTP payload type. The optional constraints are currently ignored. For MXL, the tag value must be a non-empty array, for example `[""]`; its strings are currently ignored. |
| Interface IP             | `a=x-nvnmos-iface-ip:<v>`  | n/a                                     | Senders and Receivers (RTP/UDP only)      | The interface IP address used for IS-05 transport parameters (`source_ip` / `interface_ip`)                                |
| Interface metadata       | `a=x-nvnmos-iface:<name> [<chassis-id>] <port-id> [<attached-chassis-id> <attached-port-id>]` | n/a | Senders and Receivers (RTP/UDP only) | Populates IS-04 `interface_bindings` and Node `interfaces`; used when present in the transport file, otherwise the library derives the binding from host interfaces ([design](https://github.com/NVIDIA/nvnmos/blob/main/doc/designs/x-nvnmos-iface.md)) |
| Source port              | `a=x-nvnmos-src-port:<v>`  | n/a                                     | Senders (RTP/UDP only)                    | The source port from which the stream is transmitted                                                                       |
| MXL domain ID            | n/a                        | `urn:x-nvnmos:tag:mxl-domain-id`        | Senders and Receivers (MXL only, optional)| When present, a single UUID pins the MXL domain for IS-05 (`mxl_domain_id` resolves from the tag at activation). When the tag is omitted, empty, or `[""]`, the domain is application-resolved: the IS-05 constraint is unconstrained and `/active` carries a null `mxl_domain_id`, while the data plane uses the host's local MXL domain path |

For an MXL flow definition, the tag entries are stored alongside (and follow the same shape as) the standard `urn:x-nmos:tag:grouphint/v1.0` tag, e.g.:

```json
"tags": {
  "urn:x-nmos:tag:grouphint/v1.0": [ "video-sender-1:Video" ],
  "urn:x-nvnmos:tag:name": [ "video-sender-1" ],
  "urn:x-nvnmos:tag:mxl-domain-id": [ "1ac254d9-c9be-475a-93a7-f80b9c1063a8" ]
}
```

NvNmos also publishes the caller-chosen name as a `urn:x-nvnmos:tag:name` tag on the corresponding NMOS Sender or Receiver resource visible through IS-04. This helps correlate the resource with its configuring SDP or MXL flow definition during debugging and diagnostics.

For MXL Senders, the top-level `id` field of the flow definition (if present, a UUID) is used as the MXL flow identity (i.e. the `mxl_flow_id` IS-05 transport parameter); if absent, the generated NMOS Flow ID is used in its place. The NMOS Flow ID itself is always derived from the `seed` and the name (`urn:x-nvnmos:tag:name` value) and is independent of the flow definition's `id` field. For MXL Receivers, the MXL flow identity is supplied dynamically through IS-05 Connection Management, so the `id` field of the flow definition is ignored.

## Minimal Transport Files for Unconstrained Receivers

An unconstrained Receiver publishes no BCP-004-01 Receiver Caps on IS-04; constrained Receivers publish caps with a `constraint_set` derived from the configuring transport file.

| Transport | Unconstrained marker | Meaning |
| --- | --- | --- |
| RTP/UDP (SDP) | media-level `a=x-nvnmos-caps:<pt> [<constraints>]`, e.g. `a=x-nvnmos-caps:96` | `<pt>` is required; partial constraints are ignored today |
| MXL (JSON) | `tags["urn:x-nvnmos:tag:caps"]` non-empty, e.g. `[""]` | Present + non-empty array means unconstrained; partial constraints are ignored today |

**Always required:**

| Field | RTP/UDP | MXL |
| --- | --- | --- |
| Receiver name | `a=x-nvnmos-name:<name>` (session-level) | `tags["urn:x-nvnmos:tag:name"]` |
| Label | `s=` session name | top-level `"label"` |
| Description | `i=` (optional; omitted → empty) | `"description"` (required; may be `""`) |
| Format | `a=rtpmap:` (per `m=` line) | top-level `"media_type"` |
| ST 2022-7 support | one `m=` line for a single-legged Receiver; two for an ST 2022-7-capable Receiver with primary/secondary paths | n/a |
| Interface(s) | `a=x-nvnmos-iface-ip:<address>` or the full `a=x-nvnmos-iface` (media-level, per leg) | n/a |

**Required only for constrained Receivers** (may be omitted when the unconstrained marker is present):

| Format | RTP/UDP (from SDP/fmtp) | MXL (from flow_def) |
| --- | --- | --- |
| Video | `a=fmtp:` `width`, `height`, `exactframerate`, `sampling`, … | `grain_rate`, `frame_width`, `frame_height`, `components`, … |
| Audio | `a=rtpmap:` clock rate, channel count | `sample_rate`, `channel_count`, `bit_depth` |
| Data | none | none |

**RTP/UDP audio caveat:** even for an unconstrained Receiver, libnvnmos still reads `a=rtpmap:` (`L24/48000/2`, etc.) to build the IS-04 Receiver resource `media_type`. Those values are not published as Receiver Caps when unconstrained, but they must still be present in the configuring SDP.

Reference fixtures live under [`rust/gst-nmos-rs/scripts/example-pipelines/fixtures/`](https://github.com/NVIDIA/nvnmos/tree/main/rust/gst-nmos-rs/scripts/example-pipelines/fixtures). Substitute `@NAME@` (caller-chosen receiver identity), `@LABEL@` (IS-04 label), and `@NIC_IP@` (RTP/UDP interface binding only) for your deployment.

### Minimal unconstrained video — RTP/UDP

```sdp
v=0
o=- 1 0 IN IP4 0.0.0.0
s=@LABEL@
t=0 0
a=x-nvnmos-name:@NAME@
m=video 5004 RTP/AVP 96
c=IN IP4 0.0.0.0
a=rtpmap:96 raw/90000
a=x-nvnmos-caps:96
a=x-nvnmos-iface-ip:@NIC_IP@
```

No `a=fmtp:` — format family comes from `a=rtpmap:96 raw/90000`. Compare with the constrained fixture `minimal-video.sdp.in`, which needs the full ST 2110 fmtp line.

### Minimal unconstrained audio — RTP/UDP

```sdp
v=0
o=- 1 0 IN IP4 0.0.0.0
s=@LABEL@
t=0 0
a=x-nvnmos-name:@NAME@
m=audio 5004 RTP/AVP 97
c=IN IP4 0.0.0.0
a=rtpmap:97 L24/48000/2
a=x-nvnmos-caps:97
a=x-nvnmos-iface-ip:@NIC_IP@
```

`a=rtpmap:` carries encoding (`L24`), clock rate, and channel count. No `a=fmtp:` needed when unconstrained (constrained receivers would also derive caps from rtpmap here).

### Minimal unconstrained data (ANC) — RTP/UDP

```sdp
v=0
o=- 1 0 IN IP4 0.0.0.0
s=@LABEL@
t=0 0
a=x-nvnmos-name:@NAME@
m=video 5004 RTP/AVP 100
c=IN IP4 0.0.0.0
a=rtpmap:100 smpte291/90000
a=x-nvnmos-caps:100
a=x-nvnmos-iface-ip:@NIC_IP@
```

ST 2110-40 ANC uses an `m=video` line with `a=rtpmap:… smpte291/90000`. No `a=fmtp:` needed when unconstrained (constrained receivers derive `grain_rate` from `exactframerate` when present).

### Minimal unconstrained video — MXL

```json
{
  "label": "@LABEL@",
  "description": "",
  "media_type": "video/v210",
  "tags": {
    "urn:x-nvnmos:tag:name": ["@NAME@"],
    "urn:x-nvnmos:tag:caps": [""]
  }
}
```

Compare with `minimal-video.mxl.json.in`, which also needs `grain_rate`, `frame_width`, `frame_height`, `components`, etc. for constrained caps.

### Minimal unconstrained audio — MXL

```json
{
  "label": "@LABEL@",
  "description": "",
  "media_type": "audio/L24",
  "tags": {
    "urn:x-nvnmos:tag:name": ["@NAME@"],
    "urn:x-nvnmos:tag:caps": [""]
  }
}
```

No `sample_rate`, `channel_count`, or `bit_depth` when the caps tag is present.

### Minimal unconstrained data (ANC) — MXL

```json
{
  "label": "@LABEL@",
  "description": "",
  "media_type": "video/smpte291",
  "tags": {
    "urn:x-nvnmos:tag:name": ["@NAME@"],
    "urn:x-nvnmos:tag:caps": [""]
  }
}
```
