<!--
 SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 SPDX-License-Identifier: Apache-2.0
-->

## `x-nvnmos-iface` — IS-04 interface metadata in SDP

Goal: allow NvNmos to register Senders and Receivers, and publish correct IS-04 `interface_bindings` and Node `interfaces`, when the daemon cannot see the Linux network interfaces that carry the RTP traffic (containers, split data/control plane, remote NIC ownership, etc.).

Today NvNmos resolves interface IP addresses (`a=x-nvnmos-iface-ip` and/or `a=source-filter:`) against `web::hosts::experimental::host_interfaces()` from nmos-cpp. If the IP is not bound on a visible host interface, add fails before the Node is useful.

### Background

NvNmos currently uses the interface IP for two distinct purposes:

| Purpose | Source | Consumer |
|---|---|---|
| IS-05 transport params | `a=x-nvnmos-iface-ip:` → `source_ip` / `interface_ip` | Connection API `/staged`, `/active`, activation callback SDP |
| IS-04 interface identity | IP matched in `host_interfaces()` → local `name` | Sender/Receiver `interface_bindings`; Node `interfaces` (`chassis_id`, `port_id`, `name`) |

The second step assumes the daemon shares a network namespace with the data plane. That breaks for deployments where the NMOS daemon runs elsewhere but still needs to advertise plausible IS-04 topology.

### Proposed attribute

Media-level SDP attribute on RTP Senders and Receivers (one per leg):

```
a=x-nvnmos-iface:<name> <port-id>                                              ; null chassis_id
a=x-nvnmos-iface:<name> <chassis-id> <port-id>                                 ; explicit chassis_id
a=x-nvnmos-iface:<name> <port-id> <attached-chassis-id> <attached-port-id>     ; null chassis_id, attached_network_device
a=x-nvnmos-iface:<name> <chassis-id> <port-id> <attached-chassis-id> <attached-port-id>
```

Grammar:

```
iface-value         = two-token / three-token / four-token / five-token
two-token           = name SP port-id
three-token         = name SP chassis-id SP port-id
four-token          = name SP port-id SP attached-chassis-id SP attached-port-id
five-token          = name SP chassis-id SP port-id SP attached-chassis-id SP attached-port-id
name                = 1*( non-space )
chassis-id          = 1*( non-space )        ; non-null IS-04 interfaces.chassis_id
port-id             = is04-mac-address
attached-chassis-id = 1*( non-space )        ; IS-04 attached_network_device.chassis_id
attached-port-id    = 1*( non-space )        ; IS-04 attached_network_device.port_id
is04-mac-address    = 6 lowercase hex octets, hyphen-separated (e.g. aa-bb-cc-dd-ee-ff)
```

**Two-token form:** `<name> <port-id>`. Omits `chassis-id`; NvNmos publishes `"chassis_id": null` on the Node (IS-04: “Set to null where LLDP is unsuitable”). The second token must match the IS-04 MAC pattern.

**Three-token form:** `<name> <chassis-id> <port-id>`. Publishes a non-null `chassis_id` string (MAC, IPv4, IPv6, or other freeform value permitted by IS-04).

**Four- and five-token forms:** append `attached_network_device` metadata (`attached-chassis-id`, `attached-port-id`). Both attached tokens are passed through as freeform strings (MAC, IPv4, IPv6, etc.).

**Companion attribute:** `a=x-nvnmos-iface-ip:<ip>` remains required for IS-05 transport parameters. The two attributes are complementary, not alternatives.

**ST 2022-7:** If any `a=x-nvnmos-iface` is present, every `m=` line must carry one and the `m=` count must match the IS-05 leg count (separate destination addresses only; no mixing with `host_interfaces()` per leg). **Separate source addresses** without `x-nvnmos-iface` remain valid (host lookup per leg). **Temporal redundancy** (`a=ssrc:` / `ssrc-group:DUP` per leg) is not supported (out of scope).

### IS-04 constraints

- **`name`** — required non-empty string, used by `interface_bindings`.
- **`chassis_id`** — required key whose value is JSON **null** or a **non-empty** string. An empty string is **not** valid.
- **`port_id`** — required non-empty MAC string; input may use `-` or `:` separators and any hex case. NvNmos normalises to lowercase hyphenated form (`^([0-9a-f]{2}-){5}[0-9a-f]{2}$`) before publishing.

NvNmos maps the two-token SDP form to an empty internal `chassis_id`, which `nmos::make_node_interfaces()` serialises as JSON `null`.

### libnvnmos implementation

Parsed into `nmos::node_interface` (same type used by nmos-cpp's `make_node_interfaces()` and `nmos::experimental::node_interfaces()`):

| Function | Role |
|---|---|
| `parse_iface` | SDP attribute value → `nmos::node_interface` |
| `make_iface` | `nmos::node_interface` → SDP attribute value |
| `get_session_description_interfaces` | session description → `std::vector<nmos::node_interface>` (only `m=` lines with `x-nvnmos-iface`); optional `legs` (default 0 skips check) — when non-zero and the vector is non-empty, `size` must equal `legs` |
| `get_interfaces` | configured RTP transport files in settings → `std::map` keyed by interface name (skips non-RTP entries via stored `transport` field) |
| `get_interface_name` | per-leg `interface_bindings` name from transport param address via `host_interfaces()` when SDP has no `x-nvnmos-iface` for that leg |

Node `interfaces` is built with `nmos::make_node_interfaces()` after merging entries from settings over host-visible entries.

Like `x-nvnmos-caps`, `x-nvnmos-iface` is consumed from the configuring transport file at add time only; it is not re-emitted in the IS-05 activation callback SDP (which reflects active transport parameters).

### Out of scope (follow-on)

| Area | Notes |
|---|---|
| **nvnmosd** | No code changes expected — passes SDP through to libnvnmos. |
| **gst-nmos-rs** | Optional properties or passthrough in `transport-file`. |

### Examples

Explicit node chassis:

```
a=x-nvnmos-iface-ip:192.0.2.10
a=x-nvnmos-iface:eth0 192.0.2.1 aa-bb-cc-dd-ee-ff
```

Null node chassis:

```
a=x-nvnmos-iface-ip:192.0.2.10
a=x-nvnmos-iface:eth0 aa-bb-cc-dd-ee-ff
```

Published Node interface (null chassis case):

```json
{
  "chassis_id": null,
  "port_id": "aa-bb-cc-dd-ee-ff",
  "name": "eth0"
}
```

Attached network device (four-token form):

```
a=x-nvnmos-iface-ip:192.0.2.10
a=x-nvnmos-iface:eth0 aa-bb-cc-dd-ee-ff 11-22-33-44-55-66 77-88-99-aa-bb-cc
```

Published Node interface (with attached_network_device):

```json
{
  "chassis_id": null,
  "port_id": "aa-bb-cc-dd-ee-ff",
  "name": "eth0",
  "attached_network_device": {
    "chassis_id": "11-22-33-44-55-66",
    "port_id": "77-88-99-aa-bb-cc"
  }
}
```
