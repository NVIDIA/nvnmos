<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

**Audience:** contributors and maintainers  
**Status:** plan (not a user guide)  
**Not a user guide:** start from the top-level README “Ways To Use NvNmos”

# User documentation plan

Make NvNmos documentation easier for the three primary consumption models:

| Audience | Entry surface today | Pain |
| --- | --- | --- |
| (a) GStreamer elements | `rust/gst-nmos-rs/README.md`, Hotdoc at `/gstreamer/`, `gst-inspect` blurbs | Property catalogue and implementation mapping come before a minimal pipeline |
| (b) C API (`libnvnmos`) | Top-level README + Doxygen from `nvnmos.h` | Usage jumps into transport-file extensions and API-change history before a minimal embed walkthrough |
| (c) Daemon (`nvnmosd`) | `rust/nvnmosd/README.md` + design record | Operator summary exists; no generated gRPC reference beyond proto comments and a short RPC table |

The first commit of this file captured a GStreamer-centric editorial review. This revision keeps the useful GStreamer guidance, widens scope to all three audiences, and records how generated docs should fit the existing Pages site.

## Goals

1. **Examples first** — every audience gets a minimal working path before catalogues, enums, and edge cases.
2. **“Why” and “how” live in guides** — READMEs and Hotdoc/Doxygen prose explain configuration models, lifecycle, and trade-offs. Property blurbs and short API summaries do not.
3. **Blurbs describe outcomes** — for `gst-nmos-rs`, say what the property achieves for the pipeline / NMOS resource. Do not name daemon RPCs, struct fields, or detailed IS-05/SDP mappings in the blurb. It is enough that the element participates in NMOS; exact field mappings belong in long-form docs.
4. **gRPC API reference** — publish generated docs from `nvnmosd.proto` as a third documentation product (not folded into Doxygen or Hotdoc).

## Non-goals

- Merging C, GStreamer, and gRPC into one documentation generator or one theme.
- Rewriting design records as user guides (keep them contributor-facing; mark status clearly).
- Changing runtime defaults (e.g. `transport=mxl`) in this docs work — call out surprises in guides; decide defaults separately if needed.
- Duplicating the full property catalogue in three places — Hotdoc/`gst-inspect` remain authoritative for element properties; Markdown guides summarise patterns and link out.

## Current documentation map

| Product | Generator / source | Published location |
| --- | --- | --- |
| C API | Doxygen (`Doxyfile`: `README.md` + `nvnmos.h`) | GitHub Pages root (`nvidia.github.io/nvnmos/`) |
| GStreamer plugins | Hotdoc (`rust/docs/gstreamer/`) | Pages `/gstreamer/` |
| Daemon / gRPC | Markdown summary + rich comments in `rust/nvnmos-rpc/proto/nvnmosd.proto` | Not on Pages yet |
| Workspace quick start | `rust/README.md` | GitHub only (already has a UDP sender/receiver example) |
| Element usage | `rust/gst-nmos-rs/README.md` | GitHub; Hotdoc portal deep-links here |
| Daemon operator | `rust/nvnmosd/README.md` | GitHub |
| Design | `doc/designs/**` | GitHub |

CI: `.github/workflows/pages.yml` builds Doxygen then Hotdoc into `public/` and `/public/gstreamer/`.

Important existing assets to build on rather than replace:

- Top-level **Ways To Use NvNmos** already names the three models.
- **`rust/README.md` Quick Start** already runs `nvnmosd` + `nmossink`/`nmossrc` with `transport=udp` and `auto-activate=true`.
- **`nvnmosd.proto`** already has solid service/RPC/message comments suitable for generation.
- Shared blurb strings live in `rust/gst-nmos-rs/src/session/mod.rs` (and a few element-local blurbs) — that is the rewrite surface for `gst-inspect` / Hotdoc property text.

---

# Principles

## Layering

| Layer | Answers | Examples |
| --- | --- | --- |
| Quick start | How do I see something work in five minutes? | Minimal pipeline, minimal C create/add/destroy, `cargo run -p nvnmosd` |
| User guide | Why these knobs? Which pattern do I pick? | Configuration patterns, lifecycle, activation model, troubleshooting |
| Generated reference | Exact surface | Doxygen symbols, Hotdoc property tables, gRPC message/RPC pages |
| Design record | Why the implementation looks like this | `doc/designs/nvnmosd/`, plans, audits |

## Property / field blurb rules (`gst-nmos-rs`)

A blurb should usually answer only:

1. What outcome does this configure?
2. Default / unset behaviour (briefly).
3. Which element or transport it applies to (when not obvious).
4. Simple exclusivity or override rules that prevent misuse (`transport-file` vs `transport-file-path`).

A blurb should **not**:

- Name `OpenSession`, `AddSender`, `AddReceiver`, `SyncResourceState`, or other gRPC RPCs.
- Lead with IS-05 `transport_params.*` or SDP attribute grammar.
- List inner element chains, fallback tables, or Rivermax Mode details.
- Duplicate the configuration-model essay (that belongs once in the README / Hotdoc intro).

NMOS may be mentioned at outcome level (“joins the same NMOS Node”, “presented to controllers as the Sender label”) without explaining the control-plane mapping.

**Long-form docs** then cover: IS-05/SDP/MXL tag mappings, inner-element wiring, precedence tables, mutability, and troubleshooting.

Illustrative rewrite direction (not final copy):

| Property | Prefer | Avoid in blurb |
| --- | --- | --- |
| `node-seed` | Stable id so elements join the same NMOS Node | `node_config.seed`, session refcounting jargon alone |
| `transport` | Which data-plane family to use (`mxl` / `udp` / `udp2` / `nvdsudp`) | Full payloader matrix and Mode 3 |
| `transport-file` | SDP or MXL flow definition text; exclusive with path | `AddSender` / re-publish behaviour |
| `auto-activate` | Start media without waiting for a controller | `SyncResourceState`, fake-chain swap internals |
| `source-ip` (receiver) | Optional remote source filter for multicast | `udpsrc` vs `udpsrc2` property name mapping |

---

# Workstream A — GStreamer users

## A1. Examples before the property catalogue

`rust/gst-nmos-rs/README.md` currently opens on **Property Surface**. Move a short **Quick start** (or a prominent link) above it:

- Point at the workspace quick start in `rust/README.md` for build + first pipelines, **or** inline one minimal sender and one minimal receiver (UDP + `auto-activate=true` is the lowest-infra path).
- Answer immediately: daemon must be running; controller optional when `auto-activate=true`; which `transport` values exist.

Keep `pipeline-examples.md` as the full catalogue.

## A2. Configuration model once, centrally

Before listing properties, document the patterns:

| Pattern | Set initially | Intended use |
| --- | --- | --- |
| Controller-managed | identity, transport, caps (as needed) | Production; IS-05 supplies network parameters |
| Self-starting | identity, caps / endpoints, `auto-activate=true` | Development and fixed pipelines |
| Complete transport file | `transport-file-path` or `transport-file` | Existing SDP or MXL flow definition |

State precedence once:

> Explicit element properties override corresponding values from the transport file. `transport-file` and `transport-file-path` are mutually exclusive.

Remove repeated precedence essays from individual blurbs where the central section covers them.

## A3. Reorder property documentation by intent

Group Markdown (and Hotdoc intro prose where practical) as:

1. Essential — `node-seed`, names, `transport`, `caps`, `auto-activate`
2. RTP/UDP network endpoints
3. MXL domain / flow
4. Identity presentation (`label`, `description`, `group-hint`, receiver caps mode)
5. Advanced Node/session (`daemon-uri`, `http-port`, host/domain/URLs)
6. Inner-element overrides (`transport-properties`, `pay-properties`, `depay-properties`)

GObject property registration order need not change solely for docs; Hotdoc follows introspection / cache order. Prefer README section order and Hotdoc **plugin intro** pages for hierarchy; blurbs stay short regardless of order.

## A4. Shorten blurbs; move mappings to guides

Rewrite constants in `session/mod.rs` (and element-local blurbs) per the principles above. Refresh `rust/docs/gstreamer/plugins/gst_plugins_cache.json` after blurb changes so Hotdoc stays in sync.

Put IS-05 / SDP / inner-element mapping tables in:

- `rust/gst-nmos-rs/README.md` subsections, and/or
- Hotdoc markdown under `rust/docs/gstreamer/plugins/nmos/` (preferred for “long form next to the element”).

## A5. Lifecycle, mutability, troubleshooting

Add compact user-facing sections (verify against implementation when writing):

| Transition | User-visible effect |
| --- | --- |
| NULL → READY | Read transport file path; connect to daemon; add NMOS resource (as applicable) |
| READY → PAUSED / activation | Build or swap real data path |
| READY → NULL | Remove resource; leave Node when last user |

Document mutability (“many settings only until READY”) in the guide, not only via `gst-inspect` flags.

Troubleshooting starters: plugin not found, cannot connect to daemon, Node without Sender/Receiver, no media until activation, `nmossrc` caps negotiation, registry discovery, MXL domain/flow, Rivermax prerequisites checklist.

## A6. Move contributor-only material

Relocate **Sync Testing** detail from the main gst-nmos-rs README to a testing doc (`tests/README.md` or `doc/testing/…`). Leave a one-line link for contributors.

## A7. Hotdoc portal polish

Portal pages today mostly redirect (C API → Doxygen root, usage → GitHub README). After guides improve, consider short in-portal intros (still examples-first) instead of bare redirects, and add a link to the future gRPC docs from the GStreamer portal and top-level README.

---

# Workstream B — C API users

## B1. C quick start before the encyclopaedia

The top-level README **Usage** section currently leads with transport enums and `x-nvnmos-*` extensions. Restructure toward:

1. **Minimal embed** — create Node, add one Sender or Receiver, run until callback or destroy (point at `nvnmos-example` symbols / steps).
2. **When to use the C API** vs daemon vs GStreamer (one short paragraph; Ways To Use already frames this).
3. **Transports and transport files** — keep the existing tables, but after the minimal path.
4. **Extensions** — retain as reference, not cold open.
5. **API changes** — keep as appendix / changelog style, not the first reading path.

Doxygen already ingests `README.md` + `nvnmos.h`. Prefer README restructuring over inventing a parallel guide, unless the README becomes too long — then split `doc/user/c-api.md` (or similar) and point Doxygen `INPUT` at it.

## B2. Align C and daemon vocabulary carefully

C API docs must not grow daemon/gRPC concepts. Shared ideas (Node seed, caller-chosen resource `name`, transport file, activation callback) should use the same user terms as GStreamer guides where accurate, without mentioning `nvnmosd` RPCs in `nvnmos.h` comments.

## B3. Example application as the tutorial spine

Treat `nvnmos-example` output steps as the authoritative walkthrough; ensure the README’s “Running the Example Application” section stays early in the C journey (link from the new quick start).

---

# Workstream C — Daemon users and gRPC API docs

## C1. Operator guide stays Markdown

Keep `rust/nvnmosd/README.md` as the operator entry: build/run, UDS, env vars, session GC contract, Node flavours. Lead with a **minimal client sequence** (open session → subscribe activations → add resource → ack loop → close) before the env-var catalogue.

Link out to design docs for lock ordering and history; do not require reading the design record to run the daemon.

## C2. Generated gRPC reference (third documentation product)

Integrating protobuf HTML into Doxygen or Hotdoc is a poor fit (different object model, separate toolchain, little shared navigation). Prefer a **third generator** publishing beside the existing Pages artifacts, e.g. `public/grpc/` or `public/nvnmosd/`.

**Recommended approach (to confirm at implementation time):**

| Option | Pros | Cons |
| --- | --- | --- |
| **sabledocs** | Built for gRPC + messages; Markdown in comments; static HTML | Extra Python tool in Pages CI |
| **protoc-gen-doc** | Common, simple HTML/Markdown from comments | Historically weaker gRPC-service presentation unless templated |
| **Buf / BSR** | Hosted polish | External dependency / org setup; less “in-tree Pages” |

Default recommendation: **sabledocs** (or protoc-gen-doc if CI wants a single `protoc` plugin and no Python), fed by `nvnmosd.proto` with `--include_source_info`. The proto comments are already the source of truth — generation should not require rewriting them into a second manual.

Pages job sketch:

1. Existing Doxygen → `public/`
2. Existing Hotdoc → `public/gstreamer/`
3. New: generate gRPC HTML → `public/grpc/` (name TBD)
4. Cross-link from top-level README, `rust/nvnmosd/README.md`, and optionally Hotdoc portal / Doxygen header

## C3. What the gRPC docs should emphasise

- Session lifecycle and the subscribe-before-add rule (already in proto + operator README).
- Persistent vs session-refcounted Nodes.
- `name` vs NMOS UUID (`resource_id`) vs daemon `*_handle`.
- Activation stream + `AckActivation`.
- Error / precondition expectations that clients hit first.

Hand-written “why” stays in the operator README; generated pages are the RPC/message reference.

---

# Cross-cutting

## Discoverability

Expand the top-level Ways To Use table into an explicit task map:

| Goal | Start here |
| --- | --- |
| Embed NMOS in a C/C++ application | C API quick start (README) + Doxygen |
| Run NMOS out of process | `nvnmosd` README + gRPC reference |
| Add NMOS to a GStreamer pipeline | `rust/README.md` Quick Start → gst-nmos-rs guide → Hotdoc |
| Run in Docker / Kubernetes | container sections |
| Understand implementation decisions | `doc/designs/` |

Add a one-line audience/status header to substantial Markdown guides and to design docs (as on this file).

## Design plans vs supported behaviour

Files named `*-plan.md` linked from user READMEs confuse “what works today.” Prefer:

- status heading inside the plan, and/or
- user README links only to a short “current behaviour” note, with the plan as further reading.

## Terminology

Use consistently across user-facing docs:

- **Node**, **Sender**, **Receiver** for NMOS resources
- lowercase **element** / **pipeline** for GStreamer
- **transport file** as the generic term; **SDP** / **MXL flow definition** when specific
- **data plane** (pick one; avoid alternating with “data path” unless distinguishing intentionally)
- **ID** in prose (“MXL domain ID”)
- Explain **name** vs **label** vs **description** once (programmatic resource name vs controller-facing text)
- Prefer “configuration-dependent” over unexplained “route-dependent”
- Reserve **registration** for NMOS Registry (IS-04), not for adding resources to the Node

---

# Suggested priority order

Ship value in this order (can be separate PRs):

1. **GStreamer blurb pass** in `session/mod.rs` — immediate `gst-inspect` / Hotdoc improvement; refresh plugin cache.
2. **gst-nmos-rs README** — Quick start + configuration patterns above the property catalogue; trim RPC names from Notes.
3. **Top-level C Usage reshape** — minimal embed before extensions / API-change history.
4. **nvnmosd README** — minimal client sequence first; keep env/RPC summary.
5. **gRPC Pages product** — wire sabledocs (or chosen tool) into `pages.yml`; cross-link.
6. **Lifecycle / mutability / troubleshooting** on the GStreamer guide.
7. **Move Sync Testing** and other contributor sections out of the primary user path.
8. **Design-plan status** / link hygiene and terminology sweep.

Items 1–4 are mostly editorial and do not require a new generator. Item 5 is the main tooling addition.

## Success criteria

- A new GStreamer user can run sender + receiver from docs without reading property tables first.
- A new C embedder can create a Node and one resource from docs without first reading extension attribute grammar.
- A new daemon client can implement the happy-path RPC sequence from the operator guide + generated reference without reading the design record.
- `gst-inspect-1.0 nmossink` blurbs describe outcomes; daemon/IS-05 mapping detail appears only in guides or gRPC docs.
)
