<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Migration

Each section lists the changes that applications need to make for a release, newest first.

## Migrating to 0.2.0

The C API and the gRPC API have not changed. The following changes in behaviour can affect an application or a pipeline.

### Annotation Limits in nvnmosd

nvnmosd refuses an IS-13 Annotation API PATCH, or an `UpdateAnnotation` call, that would store any of the following:

- a label, description or tag value longer than `NVNMOSD_ANNOTATION_LENGTH_LIMIT` (default 1024 bytes)
- a tag name longer than 256 bytes
- more than `NVNMOSD_ANNOTATION_TAG_LIMIT` writable tags (default 5)
- more than `NVNMOSD_ANNOTATION_VALUE_LIMIT` values in one tag (default 5)

nvnmosd exits at start when the file named by `NVNMOSD_ANNOTATION_CHECKPOINT_FILE` holds an annotation over one of these limits. Before upgrading, raise the limit or remove that annotation from the checkpoint file.

### Audio Channel Positions in gst-nmos-rs

`nmossrc` no longer gives audio caps a sequential `channel-mask`, which claimed front left, front right, front centre and so on, whatever the stream's channel order. Mono and stereo caps now have no `channel-mask`, and caps with more channels have `channel-mask=0x0`, as `mxlsrc` gives. `nmosaudiochannelmap` also outputs `channel-mask=0x0`. A pipeline that relies on channel positions downstream of these elements must set them itself.

`nmossink` now accepts audio caps with any `channel-mask`.

### Activation Failures in gst-nmos-rs

When `nmossrc` or `nmossink` rejects an IS-05 activation, it now sets `active` to false, which posts an `nmos-activation` element message, and posts a WARNING with the reason. It does the same when it restores its fake chain after an inner element posts an ERROR. Previously `active` could stay true while the element carried no media.

An activation of an MXL `nmossrc` to a flow whose format (video, audio or data) differs from the Receiver's now fails, so an immediate activation returns HTTP 500. Previously the activation succeeded and the Receiver carried no media.

### Video Colorimetry in gst-nmos-rs

`nmossink` now takes the colorimetry in the SDP and in the MXL flow definition from the video caps:

- The SDP always has `colorimetry`, `TCS` and `RANGE`. Previously full-range caps were described as narrow range, and caps with no colorimetry gave `BT709`.
- The MXL flow definition has `colorspace`, `transfer_characteristic` and `component_range` from the caps. Previously it always had `"colorspace": "BT709"`.
- Caps with no colorimetry use GStreamer's default for the frame size.
- `nmossink` refuses caps whose colorimetry the SDP or the MXL flow definition cannot express.

`nmossrc` now maps a received SDP's colorimetry as `rtpvrawdepay2` does. `BT2100` without `TCS` gives `bt2020-10`, since ST 2110-20 defaults `TCS` to `SDR`. `BT2020` with a bit depth below 12 gives `bt2020-10`. `RANGE=FULL` gives full-range caps.

## Migrating to 0.1.0

NvNmos had no version number before 0.1.0. These changes apply to applications written for earlier versions.

### API Changes for MXL Support

Existing application code needs to be updated as itemised below - mostly possible with search-and-replace.

#### Configuration Changes

- `NvNmosSenderConfig::sdp` and `NvNmosReceiverConfig::sdp` (`const char *`) are now `NvNmosSenderConfig::transport_file` and `NvNmosReceiverConfig::transport_file`. A new sibling `NvNmosTransport transport` field selects the format: `NVNMOS_TRANSPORT_RTP` for SDP (the zero-initialised default), `NVNMOS_TRANSPORT_MXL` for an MXL flow definition (JSON).
- The SDP identity attribute `a=x-nvnmos-id:<id>` is now `a=x-nvnmos-name:<name>` — renamed for clarity, since its value has always been the caller-chosen name (unique within the Node), not a UUID. The NMOS resource UUIDs are generated deterministically by the library from `NvNmosNodeConfig::seed` + name and are now also exposed via the new ID accessors below.
- `remove_nmos_sender_from_node_server` and `remove_nmos_receiver_from_node_server` now take a caller-chosen name (`sender_name` / `receiver_name`) instead of a UUID.

#### Activation Changes

- The IS-05 activation callback typedef `nmos_connection_rtp_activation_callback` is now `nmos_connection_activation_callback`, and its signature changed from `(server, id, sdp)` to `(server, side, name, transport_file)`. The matching `NvNmosNodeConfig::rtp_connection_activated` field is now `connection_activated`. The new `NvNmosSide` parameter (`NVNMOS_SIDE_SENDER` / `NVNMOS_SIDE_RECEIVER`) disambiguates a name that may now be shared between a Sender and a Receiver on the same Node — names are scoped per side.
- `nmos_connection_rtp_activate(server, id, sdp)` is now `nmos_connection_activate(server, side, name, transport_file)`.

#### New ID Accessors

The NMOS resource UUIDs are deterministic pure functions of `(seed, side, name)`. Pure accessors compute them without a server:

- `nmos_make_node_id(seed, out, out_len)`
- `nmos_make_device_id(seed, out, out_len)`
- `nmos_make_source_id(seed, sender_name, out, out_len)`
- `nmos_make_flow_id(seed, sender_name, out, out_len)`
- `nmos_make_sender_id(seed, sender_name, out, out_len)`
- `nmos_make_receiver_id(seed, receiver_name, out, out_len)`

Live accessors look them up on a running server:

- `nmos_get_node_id(server, out, out_len)`
- `nmos_get_device_id(server, out, out_len)`
- `nmos_get_source_id(server, sender_name, out, out_len)`
- `nmos_get_flow_id(server, sender_name, out, out_len)`
- `nmos_get_sender_id(server, sender_name, out, out_len)`
- `nmos_get_receiver_id(server, receiver_name, out, out_len)`

All write a null-terminated UUID into a buffer of at least `NVNMOS_ID_LEN` bytes (37, including the terminator). Each returns `bool`.

#### MXL Transport File Format

For `NVNMOS_TRANSPORT_MXL` the transport file is an MXL flow definition JSON (the form consumed by the MXL SDK), with NvNmos extensions carried as entries in the standard `tags` property keyed by `urn:x-nvnmos:tag:*` URN strings. See [NvNmos Extensions to the Transport File](transport-files.md) for the full set.

For MXL Senders, omitting the top-level `id` now leaves IS-05 `mxl_flow_id` unconstrained. Previously, NvNmos used the generated NMOS Flow ID as a fixed MXL flow identity. Applications that require a fixed MXL flow must now include its UUID explicitly; otherwise, a Controller may supply one or stage `"auto"` to generate a new UUID at activation.

For the full per-field documentation see the [C API reference](https://nvidia.github.io/nvnmos/nvnmos_8h.html).

### API Changes for RTP/UDP Sender IS-05 Defaults

On IS-05 activation, `"auto"` values for RTP/UDP Senders are now resolved based on the config SDP:

- `source_ip` is resolved to the SDP `a=x-nvnmos-iface-ip:` or `a=source-filter:` source address as before.
- `destination_ip` is resolved to the SDP `c=` connection address if not `0.0.0.0`; otherwise, a source-specific multicast address is generated as before.
- `destination_port` is resolved to the SDP `m=` port if non-zero; otherwise, the IS-05 default (5004) is used as before.
- `source_port` is resolved to the `a=x-nvnmos-src-port:` port if present; otherwise, the IS-05 default (5004) is used as before.
