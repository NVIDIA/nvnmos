<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

## `gst-nmos-rs` — ST 2022-7 dual-leg RTP (separate destination addresses)

**Status:** implemented in `gst-nmos-rs` (unit-tested; Rivermax hardware soak pending).

Goal: support SMPTE ST 2022-7 stream duplication on two network interfaces when `transport=nvdsudp`, by preserving dual-`m=` configuring SDPs for NMOS registration, interpreting per-leg `a=inactive` / IS-05 `rtp_enabled` at activation, wiring only **active** legs on the inner chain, and driving the DeepStream `nvdsudpsrc` / `nvdsudpsink` redundancy API where supported.

This plan covers the **gst-nmos-rs** side (SDP passthrough, activation gating, inner chain). `nvnmosd` already emits and interprets `a=inactive` per leg via `rtp_enabled`. OSS `udp` / `udp2` remain **single-leg only** and must **reject** dual-leg `transport-file*` at element creation.

### Background

ST 2022-7 in our scope is **separate destination addresses**: two `m=` lines carrying the same essence over independent paths (typically two interfaces on a dual-port NIC). This matches:

- IS-05 RTP `transport_params` as a two-element array
- `a=group:DUP` at session level (when present)
- Per-leg `a=x-nvnmos-iface` / `a=x-nvnmos-iface-ip` (see [`x-nvnmos-iface.md`](x-nvnmos-iface.md))

Prior art: [`nvds_nmos_bin/src/helpers/sdp_helpers.cpp`](../../nvds_nmos_bin/src/helpers/sdp_helpers.cpp) and [`gstnvdssdpsrc.cpp`](../../nvds_nmos_bin/src/gstnvdssdpsrc.cpp) map dual-leg SDP → `local-iface-ip` + `st2022-7-streams` on `nvdsudpsrc`.

Phase placement: **Phase 5** in [`nvnmosd/README.md`](nvnmosd/README.md) (after `transport=nvdsudp` single-leg Phase 2 lands).

### Three SDP layers (do not conflate)

| Layer | Artifact | Leg count / `a=inactive` |
|---|---|---|
| **1. Configuring** | `transport-file*` after [**passthrough**](#configuring-passthrough) → `nvnmosd` | **Preserve** user leg count (1 or 2) and every `a=inactive` verbatim |
| **2. Activation** | Auto-activate sync or IS-05 activation SDP | Interpret `a=inactive` on **each** `m=` (1- or 2-leg) |
| **3. Chain build** | [`UdpMedia`](../../rust/gst-nmos-rs/src/session/udp/types.rs) + inner factories | **Active legs only**; fake / single-leg / ST 2022-7 |

**Passthrough** (gst-nmos-rs term, not generic NMOS): when the user supplies `transport-file*`, [`passthrough_with_overrides`](../../rust/gst-nmos-rs/src/sdp_passthrough.rs) edits the parsed `SDPMessage` in place and serialises it back — **without** round-tripping through `UdpMedia` / `build_sdp` — so vendor attributes, `a=ts-refclk:`, `a=inactive`, etc. survive to `libnvnmos`. Synthesis (no `transport-file*`) uses `from_caps` → `build_sdp` and is always **single-leg, never `a=inactive`**.

```
transport-file* (configuring)  →  passthrough  →  nvnmosd registration
        ↓
activation SDP (auto-activate or IS-05)  →  per-leg a=inactive interpretation
        ↓
chain build  →  fake | single-leg real | dual-leg ST 2022-7 (nvdsudp only)
```

### Scope constraint

**In scope**

- Two `m=` blocks, **same essence** (equivalent PT / `rtpmap` / `fmtp`; cross-check like [`gst-nmos-rs-sdp-passthrough-plan.md`](gst-nmos-rs-sdp-passthrough-plan.md)).
- **Separate destination addresses** only.
- Per-leg `a=inactive` (IS-05 `rtp_enabled: false`).
- **Active-leg count** 0, 1, or 2 determines chain mode at activation.
- Secondary-only activation (leg 0 inactive, leg 1 active): valid; chain uses leg 1 only; `nvdsudpsink` gets a **synthesised single-`m=`** runtime SDP.

**Out of scope**

- **Temporal redundancy** (`a=ssrc:` / `ssrc-group:DUP` per leg).
- **Separate source addresses** (single `m=` with multi-source `a=source-filter:`).
- ST 2022-7 on OSS `udpsrc` / `udpsink` — dual-leg `transport-file*` on `transport=udp` / `udp2` is an **error** at element creation (do not register a 2022-7-capable NMOS resource the element cannot honour).
- Mixed-essence multi-`m=` (e.g. video + audio).
- Three or more `m=` blocks.

### Design principles

#### 1. No leg-2 scalars on `nmossrc` / `nmossink`

Top-level properties keep the IS-05 **single-leg** vocabulary. Redundancy is carried by dual-`m=` configuring / activation SDP, IS-05 `transport_params[0]` / `[1]`, and [`transport-properties`](gst-nmos-rs-inner-properties-plan.md) passthrough to inner nvdsudp fields.

#### 2. `UdpMedia.secondary` is for **chain build** — active redundant leg only

[`parse_sdp`](../../rust/gst-nmos-rs/src/sdp.rs) (chain lens) maps **active** legs only:

> `secondary` is populated only when a second **active** leg exists. `a=inactive` legs are ignored for inner chain configuration.

Configuring SDP handed to `nvnmosd` is the **passthrough text**, not this collapsed view.

#### 3. Active legs, not `m=` index

After classifying each media line:

1. Collect **active** legs in SDP order.
2. Map to [`UdpMedia`](../../rust/gst-nmos-rs/src/session/udp/types.rs):
   - **0 active** → fake chain at activation (essence may still be parsed from leg 0 for cross-check)
   - **1 active** → `primary` = that leg, `secondary: None` (leg 0 or leg 1)
   - **2 active** → first active → `primary`, second active → `secondary`

#### 4. Property priority at chain build

```
activation SDP → parse active legs → UdpMedia → build_* (auto nvdsudp mapping) → transport-properties
```

[`transport-properties`](gst-nmos-rs-inner-properties-plan.md) wins over auto-mapping.

#### 5. Configuring vs activation vs synthesis

| Input route | Configuring SDP to `nvnmosd` | `a=inactive` |
|---|---|---|
| **`transport-file*`** (1 or 2 legs) | Passthrough: **same leg count**, preserve `a=inactive` | Preserved; with `auto-activate=true` initial IS-05 state is fully specified |
| **Synthesised** (caps + properties, no file) | `build_sdp` / `from_caps`: **always 1 leg** | Never emitted |
| **Property splice on file** | Passthrough on leg 0 only for transport scalars | See [property overrides](#property-overrides-vs-a-inactive) |

| Stage | NMOS registration | Chain at activation |
|---|---|---|
| **Configuring** 2-`m=` template, leg 2 `a=inactive` | Advertise 2022-7-capable resource with dormant leg 2 | Per activation SDP / auto-activate |
| **Activation** 2-`m=`, 1 active | Unchanged (daemon view) | Real **single-leg** nvdsudp; `nvdsudpsink` uses normalised 1-`m=` SDP |
| **Activation** 2-`m=`, 2 active | Unchanged | Real **ST 2022-7** nvdsudp |
| **`transport=udp` / `udp2` + 2-`m=` file** | **Error at element creation** | — |

#### 6. Property overrides vs `a=inactive`

On the **passthrough** path, IS-05 **transport scalars** spliced onto **leg 0 only** (`destination-ip`, `interface-ip`, `source-ip`, `source-port`, `destination-port`):

- When any of those overrides is set → **remove `a=inactive` from leg 0** (explicit intent to configure a live primary leg).
- Identity / cosmetic overrides (`label`, `name`, `description`, `caps-mode`, …) → **do not** touch `a=inactive`.
- Leg 1 → never receives scalar splices from element properties.

### Chain gating (`a=inactive` and `master_enable`)

Applies at **activation** (including `auto-activate=true` startup) for **all** RTP transports (`udp`, `udp2`, `nvdsudp`):

| Condition | Data path |
|---|---|
| `master_enable: false` | Fake chain (existing deactivation) |
| All legs `a=inactive` / `rtp_enabled: false` with `master_enable: true` | **Fake chain**, activation ack **success** (valid dormant IS-05 state) |
| Single-leg SDP, leg `a=inactive` | **Fake chain** |
| Exactly **1 active** leg (in 1- or 2-leg SDP) | Real chain, **single-leg** wiring |
| **2 active** legs (`transport=nvdsudp` only) | Real chain, **ST 2022-7** nvdsudp |

Inactive legs must **not** be passed to the inner element. A single active leg in a dual-leg activation SDP must **not** enable 2022-7 mode on the inner element.

### Transport matrix

| Transport | Dual-leg `transport-file*` at create | Single-leg + `a=inactive` at activation | Dual-leg, 1 active at activation | Dual-leg, 2 active at activation |
|---|---|---|---|---|
| `udp` / `udp2` | **Error** | Fake | — (cannot configure) | — |
| `nvdsudp` | OK (passthrough preserves legs) | Fake | Real single-leg (+ normalised sink SDP) | Real ST 2022-7 |

### nvdsudp inner mapping

When `transport=nvdsudp` and [`UdpMedia.secondary`](../../rust/gst-nmos-rs/src/session/udp/types.rs) is `Some` (`nvdsudpsrc`):

| Inner property | Source |
|---|---|
| `local-iface-ip` | Comma-separated `interface_ip` from active legs |
| `st2022-7-streams` | Comma-separated `destination_ip:destination_port` per active leg |
| `caps` / essence | Shared `UdpMedia.raw_caps` / `rtp_caps` |

When `secondary` is `None` (single active leg):

| Inner property | Source |
|---|---|
| `address` / `port` (`nvdsudpsrc`) or `host` / `port` (`nvdsudpsink`) | Active `primary` leg only |
| `local-iface-ip` | Primary leg `interface_ip` |
| `sdp-file` (`nvdsudpsink`) | **Normalised** single-`m=` SDP when activation SDP had 2 `m=` but only 1 active |

Do not set `address`/`port` and `st2022-7-streams` redundantly on `nvdsudpsrc`.

Known gaps (document only): per-leg source port in 2022-7 mode; `nvdsudpsink` has no `st2022-7-streams` property (comma `local-iface-ip` only when dual-leg send is needed). `nvdsudpsrc` `source-address` is comma-separated per active leg (`source_ip` or Rivermax wildcard `0.0.0.0`); property omitted when every leg is ASM.

### Implementation phases

1. **Dual-leg validation + chain parse** — [`parse_sdp`](../../rust/gst-nmos-rs/src/sdp.rs): essence equivalence; active-leg → `UdpMedia`; [`count_active_sdp_legs`](../../rust/gst-nmos-rs/src/sdp.rs); reject dual-leg for `udp`/`udp2` in [`decide_inner_config_*`](../../rust/gst-nmos-rs/src/session/udp/mod.rs).
2. **Passthrough dual-leg (`nvdsudp` only)** — [`passthrough_with_overrides`](../../rust/gst-nmos-rs/src/sdp_passthrough.rs): allow 2 same-essence `m=`; leg-0-only transport scalar splice; clear `a=inactive` on leg 0 when scalars set.
3. **Inactive-leg chain gating** — activation planner + startup: 0 active → [`InnerConfig::Fake`](../../rust/gst-nmos-rs/src/session/mod.rs); single-leg inactive → fake for `udp`/`udp2`/`nvdsudp`.
4. **`normalise_to_single_active_leg`** — for `nvdsudpsink` when dual-leg activation has exactly one active leg.
5. **`build_nvdsudp*`** — branch on `media.secondary`; `st2022-7-streams` on `nvdsudpsrc`.
6. **Documentation** — README + cross-link from [`nvnmosd/README.md`](nvnmosd/README.md).

### Test plan

| Test | Assert |
|---|---|
| Dual active `m=`, same essence | `primary` + `secondary` populated |
| Leg 0 active, leg 1 `a=inactive` | `secondary: None` |
| Leg 0 `a=inactive`, leg 1 active | `primary` = leg 1, `secondary: None` |
| Both `a=inactive` | Planner → `InnerConfig::Fake` |
| Single-leg `a=inactive` on `udp` | Planner → `InnerConfig::Fake` |
| Dual-leg file on `udp` / `udp2` at create | Error |
| Dual-leg passthrough (`nvdsudp`) | 2 `m=` preserved including `a=inactive` |
| Passthrough scalar override on leg 0 | Clears `a=inactive` on leg 0 only |
| `normalise_to_single_active_leg` secondary-only | One `m=`, matches active leg |
| 2 active legs `nvdsudpsrc` | `st2022-7-streams` + comma `local-iface-ip` |

### Resolved decisions

- **Passthrough dual-leg**: allowed for `transport=nvdsudp` only; rejected for `udp`/`udp2` at element creation (not merely at passthrough).
- **`cross_check_essence`**: first `m=` essence suffices; dual-leg requires equivalence check between legs.
- **Normalised 1-`m=` SDP**: only for `nvdsudpsink` runtime `sdp-file` when dual-leg activation has one active leg — **not** for configuring SDP sent to `nvnmosd`.
- **Session-level `a=group:DUP`**: infer from two same-essence `m=` lines; do not require `group:DUP` for parse.

### Cross-references

- [`doc/designs/nvnmosd/README.md`](nvnmosd/README.md) — Phase 5
- [`doc/designs/x-nvnmos-iface.md`](x-nvnmos-iface.md)
- [`doc/designs/gst-nmos-rs-inner-properties-plan.md`](gst-nmos-rs-inner-properties-plan.md)
- [`doc/designs/gst-nmos-rs-sdp-passthrough-plan.md`](gst-nmos-rs-sdp-passthrough-plan.md)
- [`rust/gst-nmos-rs/src/session/udp/types.rs`](../../rust/gst-nmos-rs/src/session/udp/types.rs)
- [`src/nvnmos_impl.cpp`](../../src/nvnmos_impl.cpp) — `rtp_enabled` ↔ `a=inactive`
