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

Related upstream tracker: [sony/nmos-cpp#456](https://github.com/sony/nmos-cpp/issues/456). nmos-cpp [#534](https://github.com/sony/nmos-cpp/pull/534) (merged 2026-11-13) added the TR-10-1 fmtp parameters, TR-10-8 Link Offset Delay and TR-10-9 default multicast addresses; §6.4 and §7.1 describe what it covers.

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
| **TR-10-9** | System environment / device behaviour (default multicast addresses §17.1, DNS-SD browse order, non-baseband measured-parameter defaults) |
| TR-10-5 / -13 | HKEP / PEP — **out of scope** for the first slices (nmos-cpp already parses `a=hkep:`, and validates the registered `ext_privacy_*` transport parameters against their schema) |
| TR-10-7 / -11 / -15 | Compressed / JPEG XS — follow existing BCP-006-01 / jxsv work separately |
| TR-10-6, -10, -12, -14, -16 | FEC, HDMI InfoFrame, AES3, USB, HDR Info Block — later |

AMWA companions (not VSF, but referenced by TR-10-8 and related BCPs):

- [BCP-004-01](https://specs.amwa.tv/bcp-004-01/) Receiver Capabilities — **already used** by NvNmos / gst-nmos-rs
- [BCP-004-02](https://specs.amwa.tv/bcp-004-02/) Sender Capabilities — TR-10-8:2026 **shall** for IPMX Senders
- [IS-11](https://specs.amwa.tv/is-11/) Stream Compatibility Management — TR-10-8:2026 **shall** for IPMX Devices; nmos-cpp PR [#474](https://github.com/sony/nmos-cpp/pull/474) is still open, not merged (supersedes [#271](https://github.com/sony/nmos-cpp/pull/271))
- [NMOS Parameter Registers, Transport Parameters](https://github.com/AMWA-TV/nmos-parameter-registers/tree/main/transport-parameters) — registers `ext_link_offset_delay`; nmos-cpp bundles its schemas and validates registered `ext_` parameters in `/staged`
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
│  • SDP incl. typed IPMX fmtp (done, sony/nmos-cpp#534)      │
│  • ext_link_offset_delay field + register schema (done)     │
│  • TR-10-9 default multicast address helper (done)          │
│  • DNS-SD browse mode (TR-10-9 §15) already present         │
│  • IPMX demo in nmos-cpp-node ("ipmx": true) (done)         │
│  • Later, if required: BCP-004-02,                          │
│    IS-11 (sony/nmos-cpp#474 open)                           │
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

TR-10-9 §10 gives defaults for **non-baseband** senders (e.g. pattern generators, file playout): video measured pixel clock ≈ `width * height * exactframerate`; audio measured sample rate ≈ rtpmap clock rate. Software gst-nmos-rs senders typically fall in this bucket unless wired to a real baseband clock measurement. nmos-cpp-node computes video values differently (see §6.4); open question 2 covers which to follow.

**Current path: nmos-cpp typed fields.** Since [nmos-cpp#534](https://github.com/sony/nmos-cpp/pull/534), the IPMX fmtp parameters are fields of the nmos-cpp parameter structs (§6.4). libnvnmos sets them on the struct before calling `make_sdp_parameters`, and reads them back from `get_*_parameters`. libnvnmos does not need its own fmtp inject/parse helpers. Because the typed rebuild `get_*_parameters` → `make_*_sdp_parameters` now carries these parameters, it no longer drops them.

`mediaclk` / `ts-refclk` are already first-class on `sdp_parameters`; those are set/preserved separately (see §6.5), not in fmtp.

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

Already present before [#534](https://github.com/sony/nmos-cpp/pull/534):

- `sdp_parameters::mediaclk`, with `sdp::mediaclk_sources::sender` and `direct`; the convenience constructor defaults to `direct=0`
- `ts_refclk` including `local_mac`; `nmos::details::make_ts_refclk` emits `localmac=<port_id>` for each interface binding when the Source's clock is `internal`, and PTP otherwise
- Grammar for `a=hkep:` (TR-10-5)

Added by [#534](https://github.com/sony/nmos-cpp/pull/534):

- `bool ipmx` on `video_raw_parameters`, `video_jxsv_parameters`, `audio_L_parameters` and `video_smpte291_parameters`; `measuredpixclk`, `vtotal` and `htotal` on the two video structs; `measuredsamplerate` on `audio_L_parameters`. Zero means omitted. `make_*_sdp_parameters` emits them in the order of the TR-10-1 §10 examples, and `get_*_parameters` parses them. The existing ST 2110 constructors are kept, so existing callers compile unchanged.
- `sdp::fields::IPMX`, `measuredpixclk`, `vtotal`, `htotal` and `measuredsamplerate` in `sdp/json.h`.
- `nmos::ipmx::make_default_multicast_address_v4(interface_address, stream_number)` in `nmos/ipmx.h`, which returns 239.S.C.D for media network port address A.B.C.D and stream number S (1–127), per TR-10-9 §17.1.
- `nmos::fields::ext_link_offset_delay` and Connection API handling for it (§7.1).
- An `"ipmx": true` setting in nmos-cpp-node, which marks the SDP of its video, audio and data Senders as IPMX. It also numbers its RTP Senders from 1 as IPMX stream numbers to resolve `destination_ip` `"auto"` to the default multicast address, and gives every RTP Receiver a Link Offset Delay. For video it sets the nominal `htotal` / `vtotal` of 720, 1080 and 2160 rasters at 24, 25, 30, 50 and 60 Hz (and the 1/1.001 rates), and `measuredpixclk = htotal * vtotal * exactframerate`. For other video formats it omits all three. For audio it sets `measuredsamplerate` to the sample rate.

Still open at the nmos-cpp layer:

- `make_sdp_parameters` always sets `mediaclk:direct=0`. The application overrides `sdp_params.mediaclk` for `mediaclk:sender`, and nmos-cpp-node does not demonstrate that.
- The library does not resolve `ext_link_offset_delay` `"auto"`, or maintain its constraints. The application does both (§7.1).
- BCP-004-02 Sender Capabilities and IS-11 (§7).
- Whether SDP→IS-04 Flow attribute mapping should ignore or surface measured parameters.

**Packaging:** NvNmos builds against Conan Center `nmos-cpp/cci.20261005` (`src/conanfile.py`), which predates [#534](https://github.com/sony/nmos-cpp/pull/534). Phase A needs either a newer Conan Center nmos-cpp version or the `nmos_cpp_from_source` build.

### 6.5 libnvnmos / gst-nmos-rs today

- Configuring SDP **passthrough** preserves `ts-refclk` / `mediaclk` / unknown fmtp tokens for registration.
- **Synthesis** (`from_caps` → `build_sdp`) follows ST 2110-oriented defaults (`mediaclk:direct=0`, PTP-oriented ts-refclk when clocks exist).
- On activation, libnvnmos builds the Sender `/transportfile` from the parsed configuring SDP. It keeps that SDP's fmtp and `mediaclk`, apart from the `x-nvnmos-` parameters, and replaces `ts-refclk` with `nmos::details::make_ts_refclk` from the Node clock. libnvnmos derives that Node clock from the configuring SDP's `ts-refclk`, making it `internal` when there is no PTP reference. So `IPMX` and `mediaclk:sender` survive activation, and a `localmac` configuring SDP gives a `localmac` `/transportfile` with the MAC address of each bound Node interface.

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
| IS-11 | **Not done** | Optional per §4.1; upstream PR [nmos-cpp#474](https://github.com/sony/nmos-cpp/pull/474) still open |
| BCP-005-01 EDID→caps | Out of scope | Needs baseband EDID path |
| **`ext_link_offset_delay`** | **Done in nmos-cpp; not done in NvNmos** | nmos-cpp validates the value and permits `"auto"`; NvNmos still needs constraints, `auto` resolve, active value, min/max when receiving (§7.1) |

### 7.1 Link Offset Delay (DLO)

TR-10-8 §8:

- Parameter name: `ext_link_offset_delay` on each Receiver RTP transport_params object
- Units: microseconds (examples use numeric µs)
- Staged may be `"auto"` → on activation set to the **minimum** constraint for the current stream
- When inactive: active value `0`; do not advertise min/max constraints
- When active: publish min/max from earliest reconstructable packet vs max buffer; clamp staged value into range

What nmos-cpp does since [#534](https://github.com/sony/nmos-cpp/pull/534):

- Validates each `/staged` `transport_params` leg against the bundled NMOS Parameter Registers schema, which allows `ext_link_offset_delay` to be an integer of at least 0 or `"auto"`.
- Permits `"auto"` for `ext_link_offset_delay` in the schema it builds from the Receiver's constraints. It also permits `"auto"` and null for any unregistered `ext_` parameter.
- Does not resolve `"auto"` in `nmos::resolve_rtp_auto`, and does not change the constraints or the active value.

So the remaining DLO work is in the application, which for NvNmos is libnvnmos. On creation it adds an empty `ext_link_offset_delay` constraint and stages `"auto"`. Its connection auto resolver resolves `"auto"` to the constraint minimum, or 0 when there is none. After each activation it sets the min/max constraints and clamps the active value while receiving, and removes the constraints and sets the active value to 0 when inactive. It also feeds the active value to the data plane. nmos-cpp-node's `impl::update_link_offset_delay` is a working example of the constraint and active-value updates, using a fixed example range of 15–52 µs.

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

### Phase A — Control/SDP vocabulary

Done in nmos-cpp by [#534](https://github.com/sony/nmos-cpp/pull/534): typed IPMX fmtp fields, `ext_link_offset_delay` validation and `"auto"`, the TR-10-9 default multicast address helper, and the nmos-cpp-node `"ipmx"` demo (§6.4, §7.1).

Remaining:

1. Build NvNmos against an nmos-cpp that includes [#534](https://github.com/sony/nmos-cpp/pull/534). That needs a Conan Center nmos-cpp version newer than `cci.20261005`; until then, `nmos_cpp_from_source`.
2. libnvnmos reads `ipmx` and the measured parameters with `get_*_parameters`, and sets them on the typed structs wherever it builds SDP from them (§6.2). It sets `sdp_params.mediaclk` for `mediaclk:sender`.
3. No IS-11 / BCP-004-02.

### Phase B — NvNmos / nvnmosd IPMX profile

1. Opt-in IPMX profile on add-sender / add-receiver (or detect from configuring SDP).
2. Test that the Sender `/transportfile` keeps the configuring SDP's `IPMX` fmtp, measured parameters, `mediaclk:sender` and `localmac` `ts-refclk` across activation. The current code path is expected to keep them (§6.5).
3. Wire DLO into Receiver connection resources (constraints lifecycle per TR-10-8 §8, following nmos-cpp-node's `impl::update_link_offset_delay`); surface changes to clients via existing activation callbacks / gRPC.
4. Proposal: resolve Sender `destination_ip` `"auto"` with `nmos::ipmx::make_default_multicast_address_v4` when the profile is on (TR-10-9 §17.1). This needs a stream number for each Sender (open question 7).
5. Align DNS-SD defaults with TR-10-8 when profile enabled (if not already).

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
2. **Measured parameters without baseband:** always use TR-10-9 §10 defaults for gst-nmos-rs synthesis, or allow overrides via `transport-properties` / caps fields? The defaults differ between sources. §6.2 gives `width * height * exactframerate` for TR-10-9 §10. nmos-cpp-node uses nominal baseband `htotal * vtotal * exactframerate`, and omits all three video parameters for unknown rasters. Check TR-10-9 §10 before choosing.
3. **DLO data plane:** minimum useful behaviour before Rivermax IPMX RTCP sync is wired (e.g. store-and-report only)?
4. **nmos-cpp ownership:** resolved. nmos-cpp has typed IPMX fmtp fields ([#534](https://github.com/sony/nmos-cpp/pull/534)), so NvNmos uses those (§6.2).
5. **BCP-004-02 / IS-11:** confirm product can ship Phases A–C without them; if a partner controller requires them, Phase D priority changes.
6. **nvdsudp ↔ Rivermax IPMX:** can the existing Media API path in `gst-nvdsudp` grow IPMX SR send/receive, or must it integrate RDK `IPMXStreamSender` / receiver timeline tracker as a separate mode? What Rivermax SDK version / API surface does current nvdsudp require?
7. **IPMX stream numbers:** TR-10-9 §17.1 default multicast addresses need a stream number from 1 to 127 for each Sender on a media network port. nmos-cpp-node numbers its RTP Senders in creation order. libnvnmos adds and removes Senders at run time, so should it allocate the lowest free number, take one from the caller, or leave `destination_ip` to the caller?

## 12. References

- [VSF TR-10-0:2026 Document Organization](https://static.vsf.tv/download/technical_recommendations/VSF_TR-10-0_2026-01-13.pdf)
- [VSF TR-10-1:2024 System Timing and Definitions](https://static.vsf.tv/download/technical_recommendations/VSF_TR-10-1_2024-02-23.pdf)
- [VSF TR-10-8:2026 NMOS Requirements](https://static.vsf.tv/download/technical_recommendations/VSF_TR-10-8_2026-01-06.pdf)
- [VSF TR-10-9 v2 System Environment](https://static.vsf.tv/download/technical_recommendations/VSF_TR-10-9_v2_2025-05-13.pdf)
- [AMWA BCP-004-01](https://specs.amwa.tv/bcp-004-01/), [BCP-004-02](https://specs.amwa.tv/bcp-004-02/), [IS-11](https://specs.amwa.tv/is-11/)
- [nmos-cpp#456 IPMX support?](https://github.com/sony/nmos-cpp/issues/456)
- [nmos-cpp#534 Add IPMX TR-10-1 SDP parameters, TR-10-8 Link Offset Delay and TR-10-9 default multicast addresses](https://github.com/sony/nmos-cpp/pull/534) (merged)
- [nmos-cpp#474 Add IS-11 support](https://github.com/sony/nmos-cpp/pull/474) (open; supersedes [#271](https://github.com/sony/nmos-cpp/pull/271))
- [NMOS Parameter Registers, Transport Parameters](https://github.com/AMWA-TV/nmos-parameter-registers/tree/main/transport-parameters)
- Rivermax Dev Kit IPMX apps / IO: [NVIDIA/rivermax-dev-kit](https://github.com/NVIDIA/rivermax-dev-kit/) — [`rdk_ipmx_sender`](https://github.com/NVIDIA/rivermax-dev-kit/tree/main/source/apps/ipmx_sender), [`rdk_ipmx_receiver`](https://github.com/NVIDIA/rivermax-dev-kit/tree/main/source/apps/ipmx_receiver), [`ipmx_sender_io_node`](https://github.com/NVIDIA/rivermax-dev-kit/tree/main/source/io_node/senders), [`services/media/ipmx`](https://github.com/NVIDIA/rivermax-dev-kit/blob/main/source/services/media/include/rdk/services/media/ipmx.h) (RTCP SR + Info Block `0x5831`)
- Rivermax SDK SDP `IPMX` fmtp keyword parse lives in the SDK tree (`src/utils/sdp/smpte2110_sdp_parser.c`); not linked here (private SDK repo)
- Existing stack docs: [`doc/user/transport-files.md`](../user/transport-files.md), [`doc/designs/nvnmosd/README.md`](nvnmosd/README.md), [`gst-nmos-rs-st2022-7-dual-leg-plan.md`](gst-nmos-rs-st2022-7-dual-leg-plan.md) (SDP layering / passthrough)
