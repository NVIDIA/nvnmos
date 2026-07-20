<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Migration

## API Changes for MXL Support

Existing application code needs to be updated as itemised below - mostly possible with search-and-replace.

### Configuration Changes

- `NvNmosSenderConfig::sdp` and `NvNmosReceiverConfig::sdp` (`const char *`) are now `NvNmosSenderConfig::transport_file` and `NvNmosReceiverConfig::transport_file`. A new sibling `NvNmosTransport transport` field selects the format: `NVNMOS_TRANSPORT_RTP` for SDP (the zero-initialised default), `NVNMOS_TRANSPORT_MXL` for an MXL flow definition (JSON).
- The SDP identity attribute `a=x-nvnmos-id:<id>` is now `a=x-nvnmos-name:<name>` — renamed for clarity, since its value has always been the caller-chosen name (unique within the Node), not a UUID. The NMOS resource UUIDs are generated deterministically by the library from `NvNmosNodeConfig::seed` + name and are now also exposed via the new ID accessors below.
- `remove_nmos_sender_from_node_server` and `remove_nmos_receiver_from_node_server` now take a caller-chosen name (`sender_name` / `receiver_name`) instead of a UUID.

### Activation Changes

- The IS-05 activation callback typedef `nmos_connection_rtp_activation_callback` is now `nmos_connection_activation_callback`, and its signature changed from `(server, id, sdp)` to `(server, side, name, transport_file)`. The matching `NvNmosNodeConfig::rtp_connection_activated` field is now `connection_activated`. The new `NvNmosSide` parameter (`NVNMOS_SIDE_SENDER` / `NVNMOS_SIDE_RECEIVER`) disambiguates a name that may now be shared between a Sender and a Receiver on the same Node — names are scoped per side.
- `nmos_connection_rtp_activate(server, id, sdp)` is now `nmos_connection_activate(server, side, name, transport_file)`.

### New ID Accessors

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

### MXL Transport File Format

For `NVNMOS_TRANSPORT_MXL` the transport file is an MXL flow definition JSON (the form consumed by the MXL SDK), with NvNmos extensions carried as entries in the standard `tags` property keyed by `urn:x-nvnmos:tag:*` URN strings. See [NvNmos Extensions to the Transport File](transport-files.md) for the full set.

For the full per-field documentation see [`src/nvnmos.h`](https://github.com/NVIDIA/nvnmos/blob/main/src/nvnmos.h).

## API Changes for RTP/UDP Sender IS-05 Defaults

On IS-05 activation, `"auto"` values for RTP/UDP Senders are now resolved based on the config SDP:

- `source_ip` is resolved to the SDP `a=x-nvnmos-iface-ip:` or `a=source-filter:` source address as before.
- `destination_ip` is resolved to the SDP `c=` connection address if not `0.0.0.0`; otherwise, a source-specific multicast address is generated as before.
- `destination_port` is resolved to the SDP `m=` port if non-zero; otherwise, the IS-05 default (5004) is used as before.
- `source_port` is resolved to the `a=x-nvnmos-src-port:` port if present; otherwise, the IS-05 default (5004) is used as before.
