<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# `gst-nmos-rs` — `transport=nvdsudp` (DeepStream `nvdsudpsrc` / `nvdsudpsink`)

Design and implementation record for Phase 2 of the NMOS daemon + GStreamer plugin work: `nmossrc` / `nmossink` wired to DeepStream's Rivermax-backed `nvdsudp*` elements when `transport=nvdsudp`. The software path is landed in `gst-nmos-rs`; Rivermax hardware soak remains outstanding. This document is scoped to that increment only; it assumes Phase 1 (`mxl`, anchor + block-probe chain swap, `transport-properties`, OSS `udp` / `udp2`) is already landed.

Parent context: [`nvnmosd/README.md`](nvnmosd/README.md) (architecture, transport table, essence-on-pad contract), [`gst-nmos-rs-inner-properties-plan.md`](gst-nmos-rs-inner-properties-plan.md) (`transport-properties` lifecycle).

## Goal

When the user selects `transport=nvdsudp`, the inner data path uses:

| Side | Inner element | Operating mode |
|---|---|---|
| `nmossink` (sender) | `nvdsudpsink` | **Mode 3** — uncompressed essence frames in, built-in RTP packetization + Rivermax Media API traffic shaping |
| `nmossrc` (receiver) | `nvdsudpsrc` | **Mode 3** — built-in ST 2110-20 / ST 2110-30 depacketization, complete essence frames out |

No external `rtp*pay` / `rtp*depay` elements. The outer elements continue to expose **essence** on their single pads (`video/x-raw`, `audio/x-raw`, `meta/x-st-2038`), matching the contract in the parent design.

**Initial scope constraints (explicit):**

- **Single-leg only** — one `m=` line, one NIC, no ST 2022-7. Multi-leg `UdpMedia` and `st2022-7-streams` / comma-separated `local-iface-ip` are deferred.
- **Built-in (de)payloaders only** — Modes 1 and 2 of `nvdsudp*` (RTP packets on the pad, external depay/pay) are out of scope; we do not add a parallel `nvdsudp+rtpvrawpay` chain.
- **No new `nmossrc` / `nmossink` properties** for Rivermax tuning — advanced users set `transport-properties` (and ignore `pay-properties` / `depay-properties`, which have no leaf on this path). Packetization defaults are auto-calculated from essence caps / SDP; users override via `transport-properties` when the defaults are wrong (jumbo MTU, custom `gpu-id`, PTP source, thread affinity, etc.).
- **System memory and `memory:NVMM` (GPU Direct)** — selected by essence caps and/or `transport-properties`, not bespoke NMOS properties (see *Memory: system vs NVMM*).

## Reference material

### DeepStream 9.0 plugin docs

- [Gst-nvdsudpsrc](https://docs.nvidia.com/metropolis/deepstream/9.0/text/DS_plugin_gst-nvdsudpsrc.html) — Mode 3 activated by `caps=video/x-raw` or `audio/x-raw` (optionally `video/x-raw(memory:NVMM)`). Requires `header-size`, `payload-size`; video also needs consistent line packetization assumptions. `use-rtp-timestamp` + `adjust-leap-seconds` for TAI→UTC when reconstructing sender timestamps. `source-address` for SSM. `gpu-id` for GPU Direct (ConnectX-6+).
- [Gst-nvdsudpsink](https://docs.nvidia.com/metropolis/deepstream/9.0/text/DS_plugin_gst-nvdsudpsink.html) — Mode 3 activated when `sdp-file` is set **and** input is uncompressed essence. Video requires `payload-size` + `packets-per-line` + `sdp-file`. Audio requires `payload-size` + `sdp-file` + `a=ptime:` in the SDP. GPU Direct: `video/x-raw(memory:NVMM)` input + `gpu-id`. `pass-rtp-timestamp` only in Mode 3 without upstream payloader.

### Prior art in-tree

- `nvds_nmos_bin/src/gstnvdssdpsink.cpp` — `configure_nvdsudpsink_for_media_api()` auto-derives `payload-size` / `packets-per-line` from `video/x-raw` caps (stride from format + width; tries divisors 6…2). Documents the sink vs src `payload-size` semantics.
- `nvds_nmos_bin/example-gst-launch-commands/*.sh` — worked examples and inline commentary for 1080p60 video and 2ch/48k audio packetization math.
- `nvds_nmos_bin/ds-patches/rel-25.9/sources/gst-plugins/gst-nvdsudp/` — patched plugin sources (ST 2110-40 / ANC hooks, ST 2022-7 properties). Useful for property names and behaviour when reading code without a local install; **not** a runtime dependency of `gst-nmos-rs`.
- `rust/gst-nmos-rs/src/session/udp/` — existing `UdpMedia` / `UdpLeg` model, SDP parse/splice, IS-05 property mapping. `nvdsudp` reuses this transport file path rather than a parallel config type.
- `rust/gst-nmos-rs/src/inner.rs` — `build_udpsink` / `build_udpsrc` + typed `*Chain` structs + `apply_*_inner_properties`. Mirrored by depth-1 `NvDsUdp*Chain { bin, transport }`.

### Implementation status

Phase 2 software path is **landed** in `gst-nmos-rs`:

- `Transport::NvDsUdp` resolves via `decide_inner_config_nvdsudp` in `validate_and_open` and `make_activation_plan` (same activation semantics as `udp` / `udp2`).
- `activation_nvdsudp_parses_sdp_success` exercises session resolution.
- End-to-end wire send/recv on Rivermax hardware remains a manual soak item (see *How far can we get without installed elements?*).

Proto mapping treats `NvDsUdp` as RTP (`transport_to_proto` → `ProtoTransport::Rtp`), which is correct for the daemon.

## Inner chain design

### Topology

Unlike OSS UDP, there is **no wrapper sub-bin with a payloader/depayloader**. Each side is depth-1, analogous to MXL:

```text
nmossink ghost(sink) → anchor → nvdsudpsink
nmossrc: nvdsudpsrc → anchor → ghost(src)
```

`pay-properties` / `depay-properties` log the existing "no payloader/depayloader in this chain" warning when non-empty (same as MXL).

### Mode-3 activation contract

| Property | `nvdsudpsink` (sender) | `nvdsudpsrc` (receiver) |
|---|---|---|
| Network | `host`, `port`, `local-iface-ip` | `address`, `port`, `local-iface-ip` |
| Mode select | `sdp-file` (path) + uncompressed sink caps | `caps` = essence (`video/x-raw` / `audio/x-raw` / `meta/x-st-2038,alignment=frame` [+ `(memory:NVMM)` on video]) |
| Video packetization | `payload-size`, `packets-per-line` | `header-size` (=20), `payload-size` (RTP payload only, **excludes** 20-byte ST 2110 header) |
| Audio packetization | `payload-size` (= src payload + 12) | `header-size` (=12), `payload-size`, `payload-multiple` |
| ANC packetization | plugin default `payload-size` (variable per RTP packet) | `header-size` (=20) only; plugin default `payload-size`; no `packets-per-line` / `payload-multiple` |
| SSM | (in SDP `a=source-filter:`) | `source-address` when `UdpLeg.source_ip` set |
| Sync / clock | `sync=false` (Rivermax paces egress; not pipeline clock) | `use-rtp-timestamp` + `adjust-leap-seconds` — see *Timestamp defaults* |
| GPU Direct | `gpu-id` via `transport-properties`; input caps carry `memory:NVMM` | same |

**Critical semantic difference (document clearly):** for video, `nvdsudpsink.payload-size` includes the 20-byte RTP + ST 2110 payload header; `nvdsudpsrc.payload-size` is the raw RTP payload size only. Example for 1080p10 4:2:2: sink `1220` / src `1200`, `packets-per-line=4`.

### `sdp-file` handling (sender)

`nvdsudpsink` requires a **filesystem path** (`sdp-file`), not inline SDP text. At chain-build time the activation manager must:

1. Take the resolved configuring SDP string (already held on `TransportConfig` / session).
2. Write it to a unique temp file (e.g. `std::env::temp_dir()/nvnmos-{resource_id}-{activation_epoch}.sdp`).
3. Set `nvdsudpsink.sdp-file` to that path.
4. Delete the file when the inner `nvdsudpsink` is finalized (after `rebuild_chain` sets it NULL and removes it from the bin). Attach a small `SdpFileGuard` to the element via GObject qdata — **not** on the Rust chain struct, because callers keep only `chain.bin` and `nvdsudpsink` re-reads the path from disk in `start` (NULL→READY).

Rationale: we do not fork `nvdsudpsink`; temp files are the pragmatic bridge between NMOS's in-memory transport file and the element's path-based API. Log the path at `GST_DEBUG` for field debugging.

### Property mapping from `UdpMedia` / IS-05

Reuse the existing per-leg field mapping documented on [`UdpLeg`](../../rust/gst-nmos-rs/src/session/udp/types.rs):

| `UdpLeg` / IS-05 field | `nvdsudpsink` | `nvdsudpsrc` |
|---|---|---|
| `destination_ip` | `host` | `address` |
| `destination_port` | `port` | `port` |
| `interface_ip` | `local-iface-ip` | `local-iface-ip` |
| `source_ip` | (SDP only today) | `source-address` |
| `source_port` | not supported by `nvdsudpsrc` (noted in `gstnvdssdpsrc.cpp` TODO) | — |

`source_port` on senders: NMOS may advertise it in SDP (`a=x-nvnmos-src-port:`) but Rivermax bind-port semantics may differ from `udpsink.bind-port`. **Phase 2.0:** document as best-effort / ignored unless we confirm an `nvdsudpsink` property; do not block initial bring-up on it.

### Memory: system vs NVMM

External pad contract: essence shape, not wire form. For GPU Direct:

- User sets `caps` (or supplies a transport file whose derived `raw_caps` carry) `video/x-raw(memory:NVMM), …` on `nmossrc`, or feeds NVMM frames into `nmossink`.
- User sets `gpu-id` (and any other Rivermax knobs) via `transport-properties="properties,gpu-id=0,…"`.
- The builder passes `raw_caps` through to `nvdsudpsrc.caps` / negotiates on the `nvdsudpsink` sink pad without stripping the `memory:NVMM` feature.

We **do not** add `nmos-gpu-id` or `nmos-use-nvmm` element properties. If `gpu-id` is set in `transport-properties` but caps lack `memory:NVMM`, log a warning and proceed (matches DeepStream doc: GPU Direct needs both).

### Timestamp defaults

DeepStream defaults: `nvdsudpsink` generates TAI RTP timestamps from system/PTP clock; `nvdsudpsrc` ignores RTP timestamps unless `use-rtp-timestamp=true`.

**Defaults for NMOS paths (overrideable via `transport-properties`):**

| Side | Default | Rationale |
|---|---|---|
| Sender | `ptp-src` set to `local-iface-ip` / `interface_ip` when the effective SDP's `a=ts-refclk:` declares PTP (`ptp=…` — traceable, GMID, domain; not a bind address); otherwise unset (system clock) | NMOS ST 2110 senders advertise PTP clock identity via `ts-refclk`; Rivermax hardware timestamping uses the egress NIC's PTP clock |
| Receiver | `use-rtp-timestamp=true`, `adjust-leap-seconds=true` | Aligns with DeepStream's recommended ST 2110 recv pipeline and `nvdsnmosbin` flipper examples |

Sender timestamp regeneration (`pass-rtp-timestamp` on `nmossink`) remains **out of scope** for this increment (parent design defers sender timestamp modes).

**Clock provider note:** when `use-rtp-timestamp` is enabled, `nvdsudpsrc` may become the pipeline clock provider (REALTIME). Phase 1's anchor swap avoids the old `input-selector` + top-level-pause dance, but we should add a **hardware soak test** item validating multi-receiver pipelines don't regress. No code change planned until soak evidence says otherwise.

## Auto-calculated packetization

Module: `rust/gst-nmos-rs/src/nvdsudp/packetization.rs`, unit-tested without GStreamer installed.

### Video (ST 2110-20)

Port the algorithm from `configure_nvdsudpsink_for_media_api()` and the example-script commentary:

1. Compute **line stride** from width + GStreamer format + ST 2110 sampling/depth:
   - `UYVY` / 8-bit 4:2:2: `stride = 2 * width`
   - `UYVP` / 10-bit 4:2:2: `stride = (5 * width) / 2` (40 bits per 4 pixels)
   - `RGB` / 8-bit: `stride = 3 * width`
   - Extend using the same table as DeepStream docs (422/420/RGB 8/10 at 1920/3840) and parent scope formats (`UYVP`, `UYVY`, RGB).
2. Choose **packets-per-line** (`ppl`): smallest divisor of `stride` such that `(stride / ppl) + 20 <= mtu_payload_budget`, where `mtu_payload_budget` defaults to ~1452 (1500 − IP/UDP/RTP overhead) but is overrideable if user sets `payload-size` in `transport-properties` (if both set, cross-check stride equality like `nvds_nmos_bin`).
3. Derive sizes:
   - `sink_payload_size = (stride / ppl) + 20`
   - `src_payload_size = sink_payload_size - 20`
4. Set `header-size=20` on src.

If no valid `ppl` exists, chain build fails with a clear error (essence unsupported for Rivermax Media API).

### Audio (ST 2110-30)

From SDP / caps:

1. Read `ptime` (ms) from `a=ptime:` hoisted on `rtp_caps`, or default **1 ms** (matches nvds_nmos_bin examples).
2. `src_payload_size = rate × (ptime_ms / 1000) × bytes_per_sample × channels`
3. `sink_payload_size = src_payload_size + 12` (audio RTP+payload header)
4. `src.header-size = 12`
5. `src.payload-multiple`: default to `max(1, round(16 ms / ptime_ms))` (16 ms buffer cadence from 2ch/48k example) unless user overrides in `transport-properties`.

Supported formats: `S24BE` (L24), `S16BE` (L16) — matching current OSS UDP scope.

### ANC / ST 2110-40

**Landed** in `gst-nmos-rs`: `meta/x-st-2038,alignment=frame` selects Mode 3 on
`nvdsudpsrc` / `nvdsudpsink` (DeepStream `gst-nvdsudp` in `deepstreamsdk`). SDP
uses `m=video` + `encoding-name=SMPTE291` per RFC 8331. Auto packetization:
`header-size=20` on `nvdsudpsrc` only (HDS for RFC 8331); ANC RTP packet sizes
are variable — `payload-size` is left at plugin defaults. No `packets-per-line`.
GPU Direct is disabled for ANC in the plugin. Override via
`transport-properties` when needed. OSS `udp` / `udp2` use `rtpsmpte291pay` /
`rtpsmpte291depay` from `gst-plugins-rs`.

### Interaction with `transport-properties`

Apply order on each chain build:

1. Construct element at factory defaults.
2. Set **programmatic** properties (network, caps/sdp-file, auto-calculated packetization).
3. Apply `transport-properties` via existing `apply_properties_to_element` — user overrides win.

If user pre-sets `payload-size` / `packets-per-line` in `transport-properties`, run the stride **cross-check** from `nvds_nmos_bin` (warn + recalculate only when mismatch, or fail hard — prefer **warn + recalculate** to match prior art).

## SDP / transport-file integration

### Reuse `UdpMedia`

`nvdsudp` shares the RTP transport file route with `udp` / `udp2`:

- Parse / splice / synthesise SDP through existing `session/udp` + `sdp.rs`.
- `decide_inner_config_udp` logic largely applies; `decide_inner_config_nvdsudp` reuses validation without duplicating the full path.

### Synthesis defaults specific to `nvdsudp`

When building SDP from caps (`from_caps`), set:

- `TP=2110TPN` (narrow traffic profile) — parent design and `sdp.rs` comment; today omitted for OSS transports.
- Keep `PM=2110GPM`, `SSN=…`, `colorimetry`, etc. as today.

Cross-check `caps` vs `transport-file` unchanged.

### Essence formats (initial)

| Essence | Send | Recv | Notes |
|---|---|---|---|
| `video/x-raw` UYVP / UYVY, 1920×1080 / 3840×2160, common frame rates | yes | yes | Progressive + interlaced per parent scope |
| `audio/x-raw` S24BE / S16BE, 48/96 kHz, 1–16 ch | yes | yes | `ptime` from `transport-caps` or default 1 ms |
| `video/x-raw(memory:NVMM)` | yes | yes | ConnectX-6+; `gpu-id` via `transport-properties` |
| `video/x-jxsv` | no | no | Placeholder until nvdsudp JXSV lands |
| `meta/x-st-2038,alignment=frame` | yes | yes | ST 2110-40 / RFC 8331 SMPTE291 |

## Session / activation wiring

### `TransportConfig` extension

```rust
pub(crate) enum TransportConfig {
    Mxl { … },
    Udp { variant: UdpVariant, media: UdpMedia, transport_file: … },
    NvDsUdp { media: UdpMedia, transport_file: … },
}
```

`InnerConfig::Real(TransportConfig::NvDsUdp { … })` flows through the same activation manager as MXL/UDP.

### Factory presence

At `validate_and_open` (NULL→READY), after config resolution:

```rust
ensure_factory("nvdsudpsink") // or nvdsudpsrc on nmossrc
```

Clear error: install DeepStream gst-nvdsudp plugin + Rivermax SDK; ConnectX-5+; `CAP_NET_RAW` on the host binary. Do **not** silently fall back to OSS `udp`.

### `nmossink/imp.rs` / `nmossrc/imp.rs`

Mirror MXL branches:

- `build_nvdsudpsink(&media, &sdp_path, &auto_pkt)` → `NvDsUdpSinkChain`
- `build_nvdsudpsrc(&media, &auto_pkt, advertise_caps)` → `NvDsUdpSrcChain`
- `apply_nvdsudp_*_inner_properties` — only `transport-properties` (warn on non-empty `pay`/`depay`)

Activation / re-activation uses existing `rebuild_chain` / `rebuild_chain_with_opts` (receiver live swap may still pass `drain_downstream: true`).

## How far can we get without installed elements?

Almost all **software** work can land without ConnectX / Rivermax / DeepStream on the dev machine:

| Work | Without `nvdsudp*` installed |
|---|---|
| `TransportConfig::NvDsUdp` + session resolution | yes — unit tests with mocked `UdpMedia` |
| Packetization calculator | yes — pure Rust tests against known 1080p/4K tables from DeepStream docs + nvds_nmos_bin |
| `build_nvdsudpsrc` / `build_nvdsudpsink` structure | partial — `gst::ElementFactory::make("nvdsudpsink")` fails at runtime; chain tests gated on `ElementFactory::find` |
| Property mapping / apply order | yes — inspect pspec names against patched headers in `nvds_nmos_bin/ds-patches` |
| Temp SDP file lifecycle | yes — unit test write/delete without instantiating sink |
| Session open + IS-05 activation for `nvdsudp` | **Done** (unit tests; hardware soak pending) |
| README + `gst-inspect` property blurbs | yes |
| End-to-end wire send/recv | **no** — needs Rivermax license, ConnectX NIC, `CAP_NET_RAW`, plugin on `GST_PLUGIN_PATH` |
| GPU Direct validation | **no** — needs ConnectX-6+ and NVMM pipeline |
| Soak: clock provider / multi-receiver | **no** |
| ST 2022-7 | **no** — explicitly deferred |

**Recommended CI strategy:** default `cargo test` uses pure unit tests; add `#[ignore]` integration tests `nvdsudp_chain_roundtrip` and `nvdsudp_activation_smoke` documented in README (same pattern as `multi_flow_video_data.rs`). Optional GitLab job on a Rivermax-equipped runner when available.

## Out of scope (this document)

- ST 2022-7 (`st2022-7-streams`, secondary `UdpLeg`, `group:DUP` SDP)
- Modes 1/2 (external RTP pay/depay in front of `nvdsudp*`)
- `video/x-jxsv` (ST 2110-22)
- Sender `pass-rtp-timestamp` / regeneration modes
- Modifying `nvdsudpsrc` / `nvdsudpsink` sources (wrap only — parent non-negotiable)
- `nvdsudpsrc` `source_port` bind until element supports it
- Jumbo MTU auto-tuning (user sets via `transport-properties`; calculator can accept `max_packet_payload` parameter later)

## Migration commits (landed)

Implemented as `gst-nmos-rs: implement transport=nvdsudp via DeepStream Mode 3`:

1. **Packetization calculator** — `nvdsudp/packetization.rs` + unit tests. **Done**
2. **Chain factories** — `NvDsUdp{Src,Sink}Chain`, `SdpFileGuard`, factory `find` guard. **Done**
3. **Session resolution** — `TransportConfig::NvDsUdp`, `decide_inner_config_nvdsudp`, `TP=2110TPN` synthesis. **Done**
4. **Activation hooks** — `nmossink` / `nmossrc` `imp.rs` branches, `apply_nvdsudp_*_inner_properties`. **Done**
5. **README** — `transport=nvdsudp` section. **Done**

## Test plan

### Unit (no hardware)

- `video_stride_uyvp_1920` / `_3840` / `uyvy` / `rgb` — matches DeepStream published tables.
- `packets_per_line_selection` — 1080p10 → ppl=4, sink=1220, src=1200.
- `audio_l24_48k_2ch_ptime_1ms` — src=288, sink=300, `payload-multiple=16`.
- `no_valid_packetization_returns_err` — odd stride / unsupported format.
- `transport_properties_override_wins` — user `payload-size` + cross-check warning path.
- `sdp_temp_file_created_and_removed` — guard deletes on drop.
- `nvdsudp_maps_to_rtp_proto` — already exists; keep.

### Integration (gated / `#[ignore]`)

- `factory_find_nvdsudpsink` — skip if not installed.
- `build_nvdsudpsink_sets_host_port_local_iface` — read back GObject properties.
- `nmossink_nvdsudp_transport_properties_roundtrip` — `buffer-size` or `gpu-id` if set.
- `activation_single_leg_video` — `nmossink` + `nmossrc`, IS-05 PATCH, essences match OSS smoke layout but `transport=nvdsudp`.
- `receiver_use_rtp_timestamp_default` — read back from built `nvdsudpsrc`.

### Manual soak (Rivermax host)

- 1080p60 UYVP sender → receiver, NVMM and system memory legs.
- Multicast SSM with `source-address`.
- Pipeline with two `nmossrc` on same node (clock provider stress).
- `CAP_NET_RAW` documented setup for `gst-launch-1.0`.

## SDP synthesis: `a=ts-refclk:` defaults (cross-transport)

Today `build_sdp` emits `a=mediaclk:direct=0` on the caps-only synthesis path but **no** `a=ts-refclk:` (test fixtures and hand-written `transport-file*` carry it instead). Proposed transport-aware defaults:

| Transport | Synthesised `a=ts-refclk:` (senders) | Synthesised (receivers) | Rationale |
|---|---|---|---|
| `nvdsudp` | `a=ts-refclk:ptp=IEEE1588-2008:traceable` | omit | Rivermax / ST 2110-10 expect PTP on broadcast send paths; drives `nvdsudpsink.ptp-src` (decision #2) and registers a traceable PTP node clock in `libnvnmos`. |
| `udp` / `udp2` | **omit** (preferred) or `localmac=<resolved port_id>` when `interface_ip` resolves locally | omit | Software `udpsink` demos rarely need PTP in the configuring SDP. Omitting maps to an internal IS-04 clock at registration (`make_node_clock` with empty ts-refclk). Controllers that fetch IS-05 `/transportfile` get `localmac` regenerated from the node's IS-04 clock + interface bindings anyway (`nvnmos_impl.cpp` → `nmos::details::make_ts_refclk`). Optional `localmac` on synthesis is only useful if you want the configuring SDP to advertise MAC timing before the first `/transportfile` fetch — use real `iface::port_id` from `interface_ip`, not a placeholder MAC. |

**Receivers:** match existing nvnmos examples — no `ts-refclk` on synthesis or in typical receiver SDPs. Receivers follow the sender's clock; the attribute is sender-side in ST 2110 practice.

### How users override without a new element property

Three layers, in precedence order:

1. **`transport-file*` (passthrough path)** — put the desired `a=ts-refclk:` line(s) in the SDP text. `passthrough_with_overrides` preserves clock attributes verbatim (only IS-05 endpoint / identity slots are rewritten). This is the primary override today and needs no new `nmossink` / `nmossrc` property. Works for PTP, `localmac`, multiple `ts-refclk` lines (as in `main.c` when `CLK_PTP`), or deliberate omission (simply don't include the attribute).

2. **Caps-only synthesis defaults** — when no `transport-file*` is supplied, `build_sdp` applies the transport-aware table above. To get PTP on `udp` without a new property, supply a `transport-file` with `a=ts-refclk:ptp=…` (or extend synthesis later if we add a clock knob).

3. **Daemon `/transportfile` regeneration (senders only)** — after registration, `libnvnmos` rebuilds sender transport files from IS-04 node/source/sender state and **replaces** `ts-refclk` via `make_ts_refclk` (derived from the node clock that was established at registration). So the configuring SDP's `ts-refclk` mainly seeds the node clock on first register; controllers polling `/transportfile` see the authoritative clock expression from IS-04, not necessarily the literal line from gst-nmos-rs synthesis.

**Not an SDP override:** `transport-properties` `ptp-src` on `nvdsudpsink` affects the Rivermax element only; it does not rewrite SDP. Keep element knobs and SDP clock declaration aligned by driving both from the same effective SDP (decision #2).

**Deferred / out of scope for now:** a dedicated `clock-refclk` (or IS-04-clocks) element property, container-time interface-name adaptation, env-var fallbacks. Revisit when we support wholesale pipeline-description arguments where IP/MAC are unknown until runtime.

**Status:** **Done** — `build_sdp` emits PTP traceable when `narrow_traffic_profile && Side::Sender`; udp/udp2 omit; receivers omit.

## Resolved decisions (was: open questions)

| # | Topic | Decision | Implementation status |
|---|---|---|---|
| 1 | Temp SDP file location & lifetime | Per-activation file under `std::env::temp_dir()`, unlinked when the inner `nvdsudpsink` is finalized. Atomic create via `tempfile` (create-new / `O_EXCL`); path logged at `GST_DEBUG`. `SdpFileGuard` attached to the element via GObject qdata. | **Done** (`SdpFileGuard`) |
| 2 | `ptp-src` default on sender | **Derive from `a=ts-refclk:` in the effective SDP** (activation file / configuring transport file written to `nvdsudpsink.sdp-file`), not left blindly unset. When the SDP declares PTP (`a=ts-refclk:ptp=…` — traceable, GMID, domain; RFC 7273 clock identity, not a bind address), set `nvdsudpsink.ptp-src` to `local-iface-ip` / `interface_ip`. Mirror `nvds_nmos_bin`'s `gstnvdssdpsink.cpp` (`ptp-src` preset that is not itself a literal IP → fall back to egress NIC). When the SDP uses `localmac:` or omits `ts-refclk` entirely, leave `ptp-src` unset (system clock). User overrides via `transport-properties` still win (applied last). | **Done** (`nvdsudp::ts_refclk::ptp_src_from_sdp` in `build_nvdsudpsink`) |
| 3 | `sync` on `nvdsudpsink` | Default `false` — Rivermax paces from buffer timestamps / PTP, not `GstBaseSink` pipeline-clock sync. | **Done** |
| 4 | Fail vs warn on packetization mismatch | Warn + recalculate when user pre-sets mismatched `payload-size` / `packets-per-line` in `transport-properties` (match `nvds_nmos_bin`). | **Done** (`reconcile_sink_video_packetization` after `transport-properties` apply) |
| 5 | Interlaced video stride | **Same line stride as progressive** for packetization purposes — use `width` + `format` only; `interlace-mode` / field height do not halve the stride. Matches how we already compute stride today and matches nvds_nmos_bin's `configure_nvdsudpsink_for_media_api()` (width-only). Hardware soak still useful to confirm Rivermax agrees. | **Done** (calculator behaviour; no separate interlaced branch) |
| 6 | 4K packetization | Calculator handles 3840 width; pin with a unit test (UYVP 3840×2160: stride 9600 → `packets-per-line=8`, `src payload-size=1200`, `sink payload-size=1220`). | **Done** (unit test) |
| 7 | `LOCAL_IFACE_IP` env vs property | Prefer explicit `local-iface-ip` from `interface_ip` on the NMOS property route; do not silently depend on the `LOCAL_IFACE_IP` environment variable. **Deferred:** container / pipeline-description ergonomics where the interface IP is unknown until the container starts — prior art passes interface *name* and adapts at runtime, or uses env vars. Out of scope for this increment; revisit when we support wholesale pipeline-description properties or container entrypoint hooks. | **Done** (property route only) |

## Risks

- **Hardware dependency** — CI cannot prove wire correctness; calculator drift vs DeepStream internals is the main software risk. Mitigate by porting nvds_nmos_bin tables verbatim and checking against DS 9.0 doc appendix.
- **`sdp-file` path races** — mitigated by atomic `tempfile` creation; multi-activation soak on one host still useful to validate end-to-end.
- **Essence caps vs NVMM** — downstream of `nmossrc` must tolerate NVMM or user inserts `nvvideoconvert`; document, don't auto-insert convert (keeps chain minimal).
- **ANC hardware soak** — Mode 3 ANC is unit-tested against `deepstreamsdk` property semantics; Rivermax wire validation pending.
- **Clock provider** — receiver-only pipelines fine; multi-source may need Phase 2+ clock policy (monitor in soak).
