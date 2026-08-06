<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# IPMX (VSF TR-10) support — Design

Status: proposed design (initial slice)  
Scope: nmos-cpp, NvNmos / nvnmosd, gst-nmos-rs; IPMX RTCP via Rivermax / nvdsudp

## 1. Summary

Add the **NMOS and SDP** pieces needed so senders and receivers can interoperate as IPMX Media Nodes for common uncompressed video, PCM audio, and ancillary RTP flows. IPMX is ST 2110 plus a defined set of differences (VSF TR-10). Those differences include **relaxed system timing** (async sources, optional PTP, RTCP-based media-clock recovery). Full TR-10-1 data-plane timing (IPMX RTCP Sender Reports) is expected to come from **Rivermax / `nvdsudp`**, which already has reference IPMX sender/receiver support in the Rivermax Dev Kit; OSS `udp`/`udp2` can carry IPMX **signalling** but are not the path for compliant RTCP.

This document maps TR-10 requirements onto layers, records what already exists, and proposes phased work. Full TR-10-8 "IPMX Device" conformance is **not** required for the first useful slice.

Related upstream tracker: [nmos-cpp#456](https://github.com/sony/nmos-cpp/issues/456).

## 2. Motivation

| Concern | ST 2110 (today's default) | IPMX (TR-10) |
|---------|---------------------------|--------------|
| Reference clock | PTP (ST 2059) expected | PTP optional; `a=ts-refclk:localmac=…` when no common clock |
| Media clock | Typically `a=mediaclk:direct=0` | Also `a=mediaclk:sender` for async baseband |
| Timing recovery | Receiver uses ST 2110-10 TRTP model | Receivers use RTCP Sender Reports + IPMX Info Block |
| Traffic shaping | ST 2110-21 strict | IPMX traffic model (TR-10-1); still RTP/UDP |
| Control plane | NMOS recommended | TR-10-8 mandates IS-04/05 (+ more; see §5) |

IPMX SDP/NMOS lets ProAV-style controllers talk to the stack without a full ST 2110 timing story. Compliant IPMX **RTCP** is expected from Rivermax (already demonstrated in the RDK); OSS `udp`/`udp2` remain useful for signalling and light software paths, not as a full IPMX data-plane substitute.

## 3. Spec landscape (what matters here)

Authoritative index: [VSF TR-10-0](https://static.vsf.tv/download/technical_recommendations/VSF_TR-10-0_2026-01-13.pdf) (2026-01-13).

| Part | Role for this work |
|------|--------------------|
| **TR-10-1** | System timing, `a=ts-refclk` / `a=mediaclk`, mandatory `IPMX` in `a=fmtp`, measured rates, RTCP SR + IPMX Info Block |
| **TR-10-2 / -3 / -4** | Uncompressed video / PCM audio / ST 291 ANC deltas vs ST 2110-20/-30/-40 (Media Info Block types, essence rules) |
| **TR-10-8** | Minimum NMOS for an IPMX Device (IS-04/05, caps, DLO, …) |
| **TR-10-9** | System environment / device behaviour (multicast addressing, DNS-SD browse order, non-baseband measured-parameter defaults) |
| TR-10-5 / -13 | HKEP / PEP — **out of scope** for the first slices (nmos-cpp already parses `a=hkep:`) |
| TR-10-7 / -11 / -15 | Compressed / JPEG XS — follow existing BCP-006-01 / jxsv work separately |
| TR-10-6, -10, -12, -14, -16 | FEC, HDMI InfoFrame, AES3, USB, HDR Info Block — later |

AMWA companions (not VSF, but referenced by TR-10-8 and related BCPs):

- [BCP-004-01](https://specs.amwa.tv/bcp-004-01/) Receiver Capabilities — **already used** by NvNmos / gst-nmos-rs
- [BCP-004-02](https://specs.amwa.tv/bcp-004-02/) Sender Capabilities — TR-10-8:2026 **shall** for IPMX Senders
- [IS-11](https://specs.amwa.tv/is-11/) Stream Compatibility Management — TR-10-8:2026 **shall** for IPMX Devices; long-running nmos-cpp draft: [#474](https://github.com/sony/nmos-cpp/pull/474) (supersedes [#271](https://github.com/sony/nmos-cpp/pull/271))
- [BCP-005-02](https://specs.amwa.tv/bcp-005-02/) / [BCP-005-03](https://specs.amwa.tv/bcp-005-03/) HKEP / PEP over NMOS — deferred with HKEP/PEP

## 4. Compliance posture

### 4.1 Full TR-10-8 vs useful interoperability

TR-10-8:2026 section 7 requires, among other things:

- IS-04 v1.3 Node API; IS-05 v1.1 Connection API
- Unicast + multicast DNS-SD (unicast browse first when both enabled)
- BCP-002-01 natural grouping; BCP-002-02 distinguishing tags
- BCP-004-01 on Receivers; **BCP-004-02 on Senders**; **IS-11 on Devices**
- Receivers: `ext_link_offset_delay` (Link Offset Delay / DLO) on IS-05 RTP transport params
- Multicast IPv4 RTP `transport_params`

Several of those (IS-04/05 baseline, DNS-SD modes, BCP-004-01, grouping/tags where configured) are already in nmos-cpp / NvNmos. **BCP-004-02 and IS-11 are large control-plane features** and are not prerequisites for exchanging IPMX SDP or for software RTP timing via RTCP.

**Decision for this design:** treat BCP-004-02 and IS-11 as **optional follow-ons**. First slices must not claim "TR-10-8 IPMX Device" certification. Controllers that only need IPMX-marked SDP + RTCP timing can still connect; controllers that insist on IS-11 / sender constraint_sets will not be satisfied until those land.

If product requirements later demand a checked TR-10-8 claim, BCP-004-02 + IS-11 move onto the critical path (and likely need substantial nmos-cpp work first).

### 4.2 What "IPMX mode" means in our stack

An opt-in **IPMX profile** (name TBD: property, setting, or configuring-SDP detection) that:

1. Emits or accepts SDP with the TR-10-1 fmtp / clock attributes (§6).
2. Uses `ts-refclk:localmac` when no PTP clock is configured.
3. Sets `mediaclk:sender` or `mediaclk:direct=0` according to sync vs async policy.
4. (Later) Sends / interprets IPMX RTCP Sender Reports.
5. (Later) Exposes DLO on Receivers.

Passthrough of a user-supplied IPMX configuring SDP already preserves many attributes today (`a=ts-refclk:`, `a=mediaclk:`, vendor lines) via gst-nmos-rs passthrough; synthesis and activation round-trips are where gaps show up.

## 5. Layer responsibilities

```text
┌─────────────────────────────────────────────────────────────┐
│ gst-nmos-rs + nvdsudp (Rivermax)                            │
│  • Synthesise / passthrough IPMX SDP attributes             │
│  • IPMX RTCP via Rivermax (spike: wire into nvdsudp)        │
│  • udp/udp2: IPMX signalling only (no full TR-10 RTCP)      │
│  • Opt-in IPMX profile on element or from configuring SDP   │
└────────────────────────────┬────────────────────────────────┘
                             │ gRPC / transport file
┌────────────────────────────▼────────────────────────────────┐
│ nvnmosd + libnvnmos                                         │
│  • Register IS-04/05 resources; activation callbacks        │
│  • Preserve IPMX SDP on sender transportfile                │
│  • Receiver caps / unconstrained vs constrained (existing)  │
│  • DLO: constraints + staged/active ext_link_offset_delay   │
└────────────────────────────┬────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────┐
│ nmos-cpp                                                    │
│  • SDP parse/generate (fmtp tokens, mediaclk, ts-refclk)    │
│  • IS-05 ext_* already allowed by schema patternProperties  │
│  • DNS-SD browse mode (TR-10-9 §15) already present         │
│  • Demo / helpers in nmos-cpp-node (see #456)               │
│  • Later: BCP-004-02, IS-11 (if required)                   │
└─────────────────────────────────────────────────────────────┘
```

**Principle (same as other designs):** put protocol vocabulary in nmos-cpp; keep libnvnmos a thin wrap; keep GStreamer-specific synthesis and data-plane RTCP in gst-nmos-rs. Do not invent parallel SDP models in the daemon.

## 6. SDP and timing (TR-10-1) — requirements checklist

### 6.1 Mandatory for any IPMX Sender SDP

From TR-10-1:

1. **`IPMX` declaration** in `a=fmtp:` (keyword among format-specific parameters).
2. **`a=ts-refclk:`** at media level — Common Reference Clock; use `localmac=<MAC>` when no PTP/common clock.
3. **`a=mediaclk:`** — `direct=0` when Media Clock is locked to Internal Clock; `sender` when asynchronous to Internal Clock (e.g. async baseband).

### 6.2 Baseband-derived measured parameters

When the Sender converts from a baseband signal:

| Essence | Extra fmtp parameters |
|---------|------------------------|
| Video | `measuredpixclk`, `vtotal`, `htotal` |
| Audio | `measuredsamplerate` |

TR-10-9 §10 gives defaults for **non-baseband** senders (e.g. pattern generators, file playout): video measured pixel clock ≈ `width * height * exactframerate`; audio measured sample rate ≈ rtpmap clock rate. Software gst-nmos-rs senders typically fall in this bucket unless wired to a real baseband clock measurement.

**Preferred starting path (NvNmos, not nmos-cpp typed fields):** `sdp_parameters::fmtp` is already an open name/value list. Bare keywords use an empty value (same pattern as `interlace` / `segmented`), so libnvnmos can own small helpers that:

- **Generate:** after `make_video_raw_sdp_parameters` / `make_audio_L_sdp_parameters` (or after parsing a configuring SDP), append `{U("IPMX"), {}}` plus the measured tokens above (TR-10-9 §10 defaults when not baseband-measured).
- **Parse:** `nmos::details::find_fmtp` when needed; otherwise leave tokens untouched in `sdp_params.fmtp`.

That matches the app-side option in [nmos-cpp#456](https://github.com/sony/nmos-cpp/issues/456). No changes to `video_raw_parameters` / `audio_L_parameters` are required for round-trip **as long as** IPMX tokens stay on `sdp_params.fmtp` (or are re-appended after any typed rebuild). Hazard: `get_*_parameters` → `make_*_sdp_parameters` rebuilds fmtp from ST 2110 fields only and drops unknowns — every such path in IPMX mode must re-inject.

`mediaclk` / `ts-refclk` are already first-class on `sdp_parameters`; those are set/preserved separately (see §6.5), not via fmtp stuffing.

### 6.3 Examples (informative, from TR-10)

Audio async:

```text
a=rtpmap:97 L24/48000/8
a=fmtp:97 channel-order=SMPTE2110.(U08); IPMX; measuredsamplerate=47952
a=ts-refclk:localmac=00-20-FC-32-2F-40
a=ptime:0.12
a=mediaclk:sender
```

Video baseband-style:

```text
a=fmtp:96 sampling=YCbCr-4:2:2; width=1920; height=1080; …; IPMX; measuredpixclk=…; vtotal=1125; htotal=2200
a=ts-refclk:localmac=…
a=mediaclk:sender
```

### 6.4 nmos-cpp today

Already present:

- `sdp_parameters::mediaclk` (default `direct=0` in the convenience constructor)
- `ts_refclk` including `local_mac`
- Generic `fmtp` as name/value pairs (app can already insert `IPMX`, measured fields)
- Grammar for `a=hkep:` (TR-10-5)

Open (from #456 and this design):

- Whether / when to promote IPMX fmtp tokens into nmos-cpp typed fields / `make_*` helpers (optional later; not required for NvNmos — see §6.2)
- Whether `mediaclk:sender` is exercised end-to-end in nmos-cpp-node demos
- Whether SDP→IS-04 Flow attribute mapping should ignore or surface measured parameters

**Recommendation:** start with **NvNmos fmtp inject/parse helpers** (§6.2). Promote into nmos-cpp only if multiple consumers (e.g. nmos-cpp-node demos + NvNmos) need shared spelling, or if typed rebuild round-trips become too easy to get wrong.

### 6.5 libnvnmos / gst-nmos-rs today

- Configuring SDP **passthrough** preserves `ts-refclk` / `mediaclk` / unknown fmtp tokens for registration.
- **Synthesis** (`from_caps` → `build_sdp`) follows ST 2110-oriented defaults (`mediaclk:direct=0`, PTP-oriented ts-refclk when clocks exist).
- Activation may rewrite SDP via nmos-cpp paths that re-emit ts-refclk from Node clocks — IPMX `localmac` + `mediaclk:sender` must survive activation round-trips when in IPMX profile.

## 7. NMOS control plane (TR-10-8) — gap analysis

| Requirement | Status | Notes |
|-------------|--------|-------|
| IS-04 v1.3 Node | Done | NvNmos baseline |
| IS-05 v1.1 Connection | Done | + v1.2-dev MXL |
| Unicast + mDNS; unicast first | Done in nmos-cpp | `dns_sd_browse_mode` (TR-10-9 §15); ensure NvNmos defaults match TR-10-8 c–e when IPMX profile on |
| Immutable UUIDs | Done | Seed-based |
| BCP-002-02 tags | Done | If the application supplies asset tags |
| BCP-002-01 grouping | Done | If the application supplies group hints |
| IS-08 when offering channel map | Done (separate design) | |
| Sender `/transportfile` SDP | Done | |
| RTP multicast transport_params | Done | |
| BCP-004-01 Receivers | Done | Constrained vs unconstrained |
| BCP-004-02 Senders | **Not done** | Optional per §4.1 |
| IS-11 | **Not done** | Optional per §4.1; upstream draft [nmos-cpp#474](https://github.com/sony/nmos-cpp/pull/474) |
| BCP-005-01 EDID→caps | Out of scope | Needs baseband EDID path |
| **`ext_link_offset_delay`** | **Not done** | Schema allows `ext_*`; need app constraints, `auto` resolve, active value, min/max when receiving |

### 7.1 Link Offset Delay (DLO)

TR-10-8 §8:

- Parameter name: `ext_link_offset_delay` on each Receiver RTP transport_params object
- Units: microseconds (examples use numeric µs)
- Staged may be `"auto"` → on activation set to the **minimum** constraint for the current stream
- When inactive: active value `0`; do not advertise min/max constraints
- When active: publish min/max from earliest reconstructable packet vs max buffer; clamp staged value into range

IS-05 JSON Schema already permits `^ext_[a-zA-Z0-9_]+$` on receiver RTP params and constraints. nmos-cpp Connection API validates staged against the resource's constraints schema — so DLO is primarily **application / libnvnmos** work: seed constraints, resolve `auto`, update constraints on activation, feed the data plane.

gst-nmos-rs must eventually honour DLO for software receivers (buffering / playout offset). Until RTCP timing exists, DLO may be exposed as a control-plane stub or limited to "store and report".

## 8. Data plane — RTCP and IPMX Info Block

TR-10-1 §8.7+: IPMX Senders **shall** send RTCP Sender Reports to the same destination IP as media, destination port **media port + 1**, on a per-essence schedule (e.g. once per video frame, before the first media packet of that frame). Each SR carries an **IPMX Info Block** (tag `0x5831` / `"X1"`) embedding:

- `ts-refclk` string (64 bytes, padded)
- `mediaclk` string (12 bytes, padded)
- zero or more **Media Info Blocks** (types allocated in TR-10-0 Table 1; video 0x0001, PCM 0x0002, …)

ST 2110-only receivers ignore unknown RTCP; IPMX receivers use SRs to recover TRTP-equivalent timing without PTP.

### 8.1 Where the data plane actually lives

**Rivermax already implements IPMX RTCP.** The Rivermax Dev Kit ships `rdk_ipmx_sender` / `rdk_ipmx_receiver` with compound RTCP Sender Reports carrying the IPMX Info Block (`ipmx_tag = 0x5831`) and uncompressed-video Media Info Block (`0x0001`), including per-frame SR scheduling before media of that frame (`dev-kit/source/io_node/senders/ipmx_sender_io_node.*`, `dev-kit/source/services/media/ipmx.*`). Rivermax SDP utilities also parse the bare `IPMX` fmtp keyword (`src/utils/sdp/smpte2110_sdp_parser.c`).

**`gst-nvdsudp` does not wire any of that today.** Grep of `DeepStream/src/gst-plugins/gst-nvdsudp` finds no `IPMX` / `RTCP` / Info Block handling. Docs claim `nvdsudpsrc` can *receive* RTCP packets as UDP, but that is not IPMX Info Block parse/apply, and `nvdsudpsink` does not emit IPMX SRs. So the gap for `transport=nvdsudp` is **plugin integration** (and any Rivermax Media API surface nvdsudp uses), not inventing the packet format from scratch. Spike: can nvdsudp call into RDK-style IPMX sender/receiver paths, or must equivalent logic be added beside the existing ST 2110 Media API usage?

**`udp` / `udp2` (OSS):** control-plane IPMX SDP (fmtp `IPMX`, `mediaclk`, `ts-refclk`, measured params) is fine. Full TR-10-1 data-plane compliance (per-frame SR *before* first media packet of that frame, correct NTP↔RTP mapping, Media Info Blocks) is **unlikely to be worth pursuing** on plain GStreamer UDP + software RTP. `rtpbin2` has generic RTCP, but not IPMX Info Blocks or the TR-10 schedule, and software pacing will not match Rivermax. Treat OSS transports as **SDP/NMOS IPMX signalling only** unless a later product requirement forces a software RTCP experiment.

| Transport | IPMX SDP / NMOS | IPMX RTCP data plane |
|-----------|-----------------|----------------------|
| `nvdsudp` (Rivermax) | Via NvNmos / gst-nmos-rs (Phases A–B) | **Primary path** — spike wiring RDK/Rivermax IPMX into nvdsudp |
| `udp` / `udp2` (OSS) | Same control plane | **Out of scope** for full TR-10-1 RTCP (signalling-only IPMX) |
| `mxl` | N/A (not RTP) | N/A |

### 8.2 Receiver side

- Detect IPMX via fmtp `IPMX` keyword (TR-10-9 §11).
- Support both `mediaclk:direct=0` and `mediaclk:sender`.
- On `nvdsudp`: use Rivermax/IPMX SR handling once wired; honour DLO when Phase B exposes it.
- On `udp`/`udp2`: accept IPMX-marked SDP; do not promise TR-10 RTCP-driven sync.

Exact playout / DLO algorithm on the Rivermax path is part of the nvdsudp spike; do not over-specify here.

## 9. Proposed phases

### Phase A — Control/SDP vocabulary (NvNmos first; optional nmos-cpp demos)

1. NvNmos helpers to inject/parse `IPMX` + measured fmtp tokens on `sdp_params.fmtp` (§6.2); set `mediaclk` / `ts-refclk` via existing `sdp_parameters` fields.
2. Ensure every typed SDP rebuild path re-appends IPMX fmtp tokens when the IPMX profile is on.
3. Optional: nmos-cpp-node example advertising an IPMX Sender SDP (addresses #456) — can stuff fmtp the same way without library API changes.
4. Confirm `ext_*` staged params round-trip; sketch DLO `auto` resolve (bullet 2 of #456).
5. No IS-11 / BCP-004-02. No requirement for nmos-cpp typed IPMX fields in this phase.

### Phase B — NvNmos / nvnmosd IPMX profile

1. Opt-in IPMX profile on add-sender / add-receiver (or detect from configuring SDP).
2. Ensure transportfile generation and activation preserve IPMX attributes (fmtp inject path + no silent rewrite of `ts-refclk` / `mediaclk` to ST 2110-only clocks).
3. Wire DLO into Receiver connection resources (constraints lifecycle per TR-10-8 §8); surface changes to clients via existing activation callbacks / gRPC.
4. Align DNS-SD defaults with TR-10-8 when profile enabled (if not already).

### Phase C — gst-nmos-rs IPMX SDP on all RTP transports; data plane via nvdsudp

1. Synthesis / passthrough: emit or preserve `IPMX` fmtp + measured defaults (TR-10-9 §10) + `ts-refclk` / `mediaclk` policy for RTP transports (`nvdsudp`, and signalling-only on `udp`/`udp2`).
2. **Spike:** wire Rivermax IPMX RTCP (RDK `rdk_ipmx_*` / `IPMXStreamSender` patterns) into `nvdsudpsink` / `nvdsudpsrc` (or document why a parallel path is required).
3. gst-nmos-rs: enable that path when `transport=nvdsudp` and IPMX profile is on; honour DLO when Phase B exposes it.
4. Tests: golden SDP fixtures; Rivermax IPMX interop against RDK apps where possible. No commitment to OSS software IPMX RTCP.

### Phase D — Optional certification path

1. BCP-004-02 Sender `constraint_sets` (nmos-cpp + NvNmos).
2. IS-11 Stream Compatibility Management ([nmos-cpp#474](https://github.com/sony/nmos-cpp/pull/474)).
3. HKEP/PEP (BCP-005-02/03) only if product needs content protection.
4. Compressed / JPEG XS IPMX Media Info Blocks as needed.

## 10. Explicit non-goals (initial slices)

- Claiming full TR-10-8 IPMX Device compliance without Phase D
- Replacing ST 2110 mode; IPMX is opt-in / detected
- Perfect ST 2110-21 pacing on software UDP
- Full TR-10-1 IPMX RTCP on `udp` / `udp2` (signalling-only unless requirements change)
- HKEP, PEP, FEC, USB, HDMI InfoFrame
- Changing mxlsrc/mxlsink behaviour for IPMX RTP semantics

## 11. Open questions

1. **Profile switch:** explicit element/daemon property (`ipmx=true`) vs infer-from-configuring-SDP (`IPMX` in fmtp) vs both (infer for receivers, explicit for synthesised senders)?
2. **Measured parameters without baseband:** always use TR-10-9 §10 defaults for gst-nmos-rs synthesis, or allow overrides via `transport-properties` / caps fields?
3. **DLO data plane:** minimum useful behaviour before Rivermax IPMX RTCP sync is wired (e.g. store-and-report only)?
4. **nmos-cpp ownership:** keep IPMX fmtp on the NvNmos inject/parse path (§6.2) unless/until demos or other apps justify promoting tokens into nmos-cpp typed helpers?
5. **BCP-004-02 / IS-11:** confirm product can ship Phases A–C without them; if a partner controller requires them, Phase D priority changes.
6. **nvdsudp ↔ Rivermax IPMX:** can the existing Media API path in `gst-nvdsudp` grow IPMX SR send/receive, or must it integrate RDK `IPMXStreamSender` / receiver timeline tracker as a separate mode? What Rivermax SDK version / API surface does current nvdsudp require?

## 12. References

- [VSF TR-10-0:2026 Document Organization](https://static.vsf.tv/download/technical_recommendations/VSF_TR-10-0_2026-01-13.pdf)
- [VSF TR-10-1:2024 System Timing and Definitions](https://static.vsf.tv/download/technical_recommendations/VSF_TR-10-1_2024-02-23.pdf)
- [VSF TR-10-8:2026 NMOS Requirements](https://static.vsf.tv/download/technical_recommendations/VSF_TR-10-8_2026-01-06.pdf)
- [VSF TR-10-9 v2 System Environment](https://static.vsf.tv/download/technical_recommendations/VSF_TR-10-9_v2_2025-05-13.pdf)
- [AMWA BCP-004-01](https://specs.amwa.tv/bcp-004-01/), [BCP-004-02](https://specs.amwa.tv/bcp-004-02/), [IS-11](https://specs.amwa.tv/is-11/)
- [nmos-cpp#456 IPMX support?](https://github.com/sony/nmos-cpp/issues/456)
- [nmos-cpp#474 Add IS-11 support](https://github.com/sony/nmos-cpp/pull/474) (draft; supersedes [#271](https://github.com/sony/nmos-cpp/pull/271))
- Rivermax Dev Kit IPMX apps / IO: [NVIDIA/rivermax-dev-kit](https://github.com/NVIDIA/rivermax-dev-kit/) — [`rdk_ipmx_sender`](https://github.com/NVIDIA/rivermax-dev-kit/tree/main/source/apps/ipmx_sender), [`rdk_ipmx_receiver`](https://github.com/NVIDIA/rivermax-dev-kit/tree/main/source/apps/ipmx_receiver), [`ipmx_sender_io_node`](https://github.com/NVIDIA/rivermax-dev-kit/tree/main/source/io_node/senders), [`services/media/ipmx`](https://github.com/NVIDIA/rivermax-dev-kit/blob/main/source/services/media/include/rdk/services/media/ipmx.h) (RTCP SR + Info Block `0x5831`)
- Rivermax SDK SDP `IPMX` fmtp keyword parse lives in the SDK tree (`src/utils/sdp/smpte2110_sdp_parser.c`); not linked here (private SDK repo)
- Existing stack docs: [`doc/user/transport-files.md`](../user/transport-files.md), [`doc/designs/nvnmosd/README.md`](nvnmosd/README.md), [`gst-nmos-rs-st2022-7-dual-leg-plan.md`](gst-nmos-rs-st2022-7-dual-leg-plan.md) (SDP layering / passthrough)
