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
2. **“Why” and “how” live in guides** — READMEs and Hotdoc/Doxygen prose explain configuration models, lifecycle, and trade-offs. Property blurbs and proto comments do not.
3. **Reference text describes outcomes / contracts** — GStreamer blurbs say what a property achieves; proto comments state the RPC/field contract. Neither should re-tell the guide’s narrative.
4. **No duplication within a layer** — state a policy or mapping once per layer; other entries in that layer link or stay silent. Across layers, a short pointer is fine; copying essays is not.
5. **gRPC API reference** — publish generated docs from a slimmed `nvnmosd.proto` as a third documentation product (not folded into Doxygen or Hotdoc).
6. **Readable prose** — prefer short sentences, lists, and focused sections. Avoid paragraphs made from several clauses.
7. **Publish guides with their reference** — where practical, include each audience's usage guide in the same generated site as its API reference. Do not maintain a copied version for the generator.

## Non-goals

- Merging C, GStreamer, and gRPC into one documentation generator or one theme.
- Rewriting design records as user guides (keep them contributor-facing; mark status clearly).
- Changing runtime behaviour other than the explicitly agreed `transport` default below.
- Duplicating the full property catalogue in three places — Hotdoc/`gst-inspect` remain authoritative for element properties; Markdown guides summarise patterns and link out.
- Keeping proto comments as a second operator manual — generation should amplify a tight contract, not paste repeated GC/lifecycle essays under every RPC.

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

The top-level README has two render contexts:

- GitHub renders it at the repository root.
- Doxygen uses it as the generated main page.

Relative links only work in both contexts when their targets are also available
at the corresponding generated path. Links from the README to repository-only
files must therefore use absolute GitHub URLs.

Important existing assets to build on rather than replace:

- Top-level **Ways To Use NvNmos** already names the three models.
- **`rust/README.md` Quick Start** already runs `nvnmosd` + `nmossink`/`nmossrc` with `transport=udp` and `auto-activate=true`.
- **`nvnmosd.proto`** has detailed comments that are a good *starting* source for generation, but they currently duplicate operator-guide narrative (session GC, Node flavours, `name` rules) across service banners and several RPCs — slim before or with publishing.
- Shared blurb strings live in `rust/gst-nmos-rs/src/session/mod.rs` (and a few element-local blurbs) — that is the rewrite surface for `gst-inspect` / Hotdoc property text.

---

# Principles

## Layering

| Layer | Answers | Examples |
| --- | --- | --- |
| Quick start | How do I see something work in five minutes? | Minimal pipeline, minimal C create/add/destroy, `cargo run -p nvnmosd` |
| User guide | Why these knobs? Which pattern do I pick? | Configuration patterns, lifecycle, activation model, troubleshooting |
| Generated / inspectable reference | Exact surface next to the symbol | Doxygen, Hotdoc/`gst-inspect` blurbs, proto comments → gRPC HTML |
| Design record | Why the implementation looks like this | `doc/designs/nvnmosd/`, plans, audits |

### No duplication within a layer

Within one layer, prefer a single canonical statement:

| Layer | State once | Do not repeat on every entry |
| --- | --- | --- |
| GStreamer guide | Configuration patterns, precedence, lifecycle | Same essay inside each property Note |
| GStreamer blurbs | Outcome + default + applicability | Precedence model, RPC names, full IS-05/SDP maps |
| Daemon operator README | Happy-path sequence, GC *why* and env defaults, Node flavours | Full per-RPC error catalogue (link to generated ref) |
| Proto comments | Conventions (`_id` vs `_handle`), one GC/subscribe policy block | Same GC paragraph on `CloseSession`, `SubscribeActivations`, and resource banners |
| C guide / `nvnmos.h` | Embed walkthrough; extension grammar as reference | Daemon concepts in the C header |

Across layers, a one-line cross-reference is expected (README → proto pages;
blurb → guide section). Copied multi-paragraph explanations are not.

### Writing style

Apply these rules to user guides and generated-reference prose:

- Prefer one idea per sentence.
- Prefer bullets for prerequisites, choices, procedures, and consequences.
- Break long paragraphs into short sections with descriptive headings.
- Avoid sentences that encode a list as several comma-separated clauses.
- Put caveats next to the affected step.
- Use tables only for comparisons. Use bullets for simple lists.
- In reference blurbs, describe string and numeric sentinels in a separate
  sentence starting with `Empty` or `0`. End enum and boolean blurbs with
  `Default: …`. Do not join default behaviour to the outcome with a semicolon.

The editing pass should reduce density, not merely move the same dense prose
between files.

## Property / field blurb rules (`gst-nmos-rs`)

A blurb should usually answer only:

1. What outcome does this configure?
2. Default / unset behaviour (briefly).
3. Which element or transport it applies to (when not obvious).
4. Simple exclusivity or override rules that prevent misuse (`transport-file` vs `transport-file-path`).
5. The standard NMOS API field it affects when that mapping clarifies the
   outcome (for example IS-08 `/properties/name`).

A blurb should **not**:

- Name `OpenSession`, `AddSender`, `AddReceiver`, `SyncResourceState`, or other gRPC RPCs.
- Lead with IS-05 `transport_params.*` or SDP attribute grammar. A short NMOS
  field mapping may follow the user-facing outcome.
- List inner element chains, fallback tables, or Rivermax Mode details.
- Duplicate the configuration-model essay (that belongs once in the README / Hotdoc intro).

Standard NMOS API effects are part of the public contract. Daemon RPCs, inner
elements, and SDP/MXL implementation mappings are not.

At the GStreamer layer, blurbs should name the element surface the user sets
(`transport-file*`, `caps`, and so on). Reserve “configuring transport file”
for guides that explain the artifact passed to NvNmos / nvnmosd.

**Long-form docs** then cover: IS-05/SDP/MXL tag mappings, inner-element wiring, precedence tables, mutability, and troubleshooting.

### Check every removed detail

Before shortening a blurb, classify each removed fact:

- **Keep in the blurb** when it is required to use the property safely or
  identifies an observable standard NMOS API effect.
- **Move to the user guide** when it explains configuration interactions,
  detailed NMOS mappings, syntax, lifecycle, or troubleshooting.
- **Move to contributor documentation** when it describes inner elements,
  daemon RPCs, or implementation rationale.
- **Discard deliberately** only when it duplicates a canonical explanation or
  no longer describes current behaviour.

Do not delete information from its only documentation location. During the
blurb pass, keep an audit of removed facts and their destination. Complete the
corresponding guide changes before treating the pass as finished.

Current blurb-pass audit:

- The gst-nmos-rs README already contains the detailed endpoint-to-IS-05/SDP
  mappings, transport selection, MXL domain rules, bit-rate interactions,
  inner-property syntax, and activation behaviour removed from the blurbs.
- Audio channel-mapping pad behaviour currently lives only in the IS-08 design
  document. Add a user-facing channel-mapping section before completing the
  blurb pass.
- Explain channel-mapping aggregation in that section. Multiple NvNmos channel
  mappings, including multiple `nmosaudiochannelmap` elements on the same Node,
  contribute Inputs and Outputs to the Node's shared IS-08 Channel Mapping API.
  `channelmapping-name` identifies the subset owned and managed by one caller;
  it does not create a separate IS-08 API.
- Keep exact IS-08 `/properties/name` and `/properties/description` mappings in
  the pad blurbs because they disambiguate `label` from the several meanings of
  `name`.
- Sender and Receiver names are separate uniqueness namespaces on a Node. The
  same string may name both a Sender and a Receiver. Capture that in the shared
  identity guide, not in the name blurbs.

Illustrative rewrite direction (not final copy):

| Property | Prefer | Avoid in blurb |
| --- | --- | --- |
| `node-seed` | Stable id so elements join the same NMOS Node | `node_config.seed`, session refcounting jargon alone |
| `transport` | Which data-plane family to use (`mxl` / `udp` / `udp2` / `nvdsudp`) | Full payloader matrix and Mode 3 |
| `transport-file` | SDP or MXL flow definition text; exclusive with path | `AddSender` / re-publish behaviour |
| `auto-activate` | Start media without waiting for a controller | `SyncResourceState`, fake-chain swap internals |
| `source-ip` (receiver) | Optional remote source filter for multicast | `udpsrc` vs `udpsrc2` property name mapping |

## Proto comment rules (`nvnmosd.proto`)

Proto comments are the gRPC analogue of property blurbs. They feed generated
reference pages and are read next to the symbol.

A service-, RPC-, or field-level comment should usually answer only:

1. What does this call or field do in the contract?
2. What preconditions are unique to this symbol?
3. Which notable errors can this symbol return?
4. Is an identifier opaque or an NMOS UUID?

A proto comment should **not**:

- Re-explain session GC policy, Node flavour trade-offs, or the full client
  sequence.
- Restate service-level conventions on every message.
- Duplicate transport-file grammar on every related RPC or field.
- Refer to GStreamer elements or their design documents.

Put handles versus IDs and the subscribe / GC policy in one service-level
conventions block. Per-RPC comments should add only what differs. The operator
README keeps the narrative and environment-variable table.

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

Add one transport-applicability map:

- MXL means `transport=mxl`.
- RTP/UDP means `transport=udp`, `transport=udp2`, or
  `transport=nvdsudp`.
- Call out narrower exceptions such as JPEG XS bit-rate and RTP
  payloader/depayloader properties, which apply only to `udp` and `udp2`.

Use “Used only with MXL” or “Used only with RTP/UDP” in transport-specific
blurbs. Put the exact `transport=…` mapping in each element's top-level
description. Document narrower exceptions in the transport guide rather than
repeating the implementation comparison in each blurb. In particular, record
that JPEG XS bit-rate properties and RTP payloader/depayloader properties do
not currently apply to `nvdsudp`.

Remove repeated precedence essays from individual blurbs where the central section covers them.

## A3. Default to `transport=udp`

Change the `nmossink` and `nmossrc` `transport` property default from `mxl` to
`udp`.

- `udp` has the lowest setup cost and matches the examples-first quick start.
- It uses the broadly available gst-plugins-good elements.
- It does not require MXL, gst-plugins-rs, DeepStream, or Rivermax.
- It aligns with the C API, where zero-initialised configurations default to
  `NVNMOS_TRANSPORT_RTP`.
- A required sentinel would make every pipeline spell out the property without
  adding useful safety.

Call this out as a default change. MXL users must set `transport=mxl`
explicitly. Update both the Rust `Default` implementation and the
`ParamSpecEnum` defaults so runtime state, `gst-inspect`, and Hotdoc agree.

## A4. Reorder property documentation by intent

Group Markdown (and Hotdoc intro prose where practical) as:

1. Essential — `node-seed`, names, `transport`, `caps`, `auto-activate`
2. RTP/UDP network endpoints
3. MXL domain / flow
4. Identity presentation (`label`, `description`, `group-hint`, receiver caps mode)
5. Advanced Node/session (`daemon-uri`, `http-port`, host/domain/URLs)
6. Inner-element overrides (`transport-properties`, `pay-properties`, `depay-properties`)

GObject property registration order need not change solely for docs; Hotdoc follows introspection / cache order. Prefer README section order and Hotdoc **plugin intro** pages for hierarchy; blurbs stay short regardless of order.

## A5. Shorten blurbs; move mappings to guides

Rewrite constants in `session/mod.rs` (and element-local blurbs) per the principles above. Refresh `rust/docs/gstreamer/plugins/gst_plugins_cache.json` after blurb changes so Hotdoc stays in sync.

Put IS-05 / SDP / inner-element mapping tables in:

- `rust/gst-nmos-rs/README.md` subsections, and/or
- Hotdoc markdown under `rust/docs/gstreamer/plugins/nmos/` (preferred for “long form next to the element”).

## A6. Lifecycle, mutability, troubleshooting

Add compact user-facing sections (verify against implementation when writing):

| Transition | User-visible effect |
| --- | --- |
| NULL → READY | Read transport file path; connect to daemon; add NMOS resource (as applicable) |
| READY → PAUSED / activation | Build or swap real data path |
| READY → NULL | Remove resource; leave Node when last user |

Document mutability (“many settings only until READY”) in the guide, not only via `gst-inspect` flags.

Troubleshooting starters: plugin not found, cannot connect to daemon, Node without Sender/Receiver, no media until activation, `nmossrc` caps negotiation, registry discovery, MXL domain/flow, Rivermax prerequisites checklist.

## A7. Move contributor-only material

Relocate **Sync Testing** detail from the main gst-nmos-rs README to a testing doc (`tests/README.md` or `doc/testing/…`). Leave a one-line link for contributors.

## A8. Publish the GStreamer usage guide with Hotdoc

The Hotdoc portal currently redirects its usage page to the GitHub README.
Decide how to publish the improved usage guide inside Hotdoc instead:

- Prefer one Markdown source consumed by Hotdoc and readable on GitHub.
- Keep `rust/gst-nmos-rs/README.md` as a short repository entry page if Hotdoc
  needs the full guide elsewhere.
- Do not copy the same guide into a Hotdoc-only file.
- Keep generated property tables in Hotdoc. The guide should link to them.

Add links from the Hotdoc portal to the C API and future gRPC docs.

---

# Workstream B — C API users

## B1. Decide the Doxygen document boundaries

The documentation topology is agreed as:

| Document | Purpose | Publication owner |
| --- | --- | --- |
| Top-level `README.md` | Project landing page and choice of C / daemon / GStreamer entry point | Doxygen main page and GitHub repository landing page |
| `doc/user/concepts.md` | Shared transport file, activation direction, and identity model | Doxygen; linked from every integration layer |
| `doc/user/c-api-guide.md` | C API usage, example application, transports, callbacks, and troubleshooting | Doxygen |
| `doc/user/building.md` | Prerequisites, Conan/CMake, local nmos-cpp checkout, and runtime requirements | Doxygen |
| `doc/user/transport-files.md` | SDP and MXL extension grammar and minimal unconstrained Receiver transport files | Doxygen |
| `doc/user/migration.md` | API changes and current migration guidance | Doxygen |
| `rust/nvnmosd/README.md` | Running and using `nvnmosd` | Daemon/gRPC documentation product; GitHub until that product is published |
| `rust/gst-nmos-rs/README.md` | Pipelines and element configuration | Hotdoc/GStreamer documentation product; GitHub until the guide is published there |
| `docker/**/README.md` | Container build and runtime guidance | GitHub container documentation |
| `doc/designs/**` | Contributor plans, design records, and rationale | GitHub; not user-reference input |

The top-level README remains short and links to the Doxygen-published shared
concepts and C pages, and to the separately owned daemon, GStreamer, and
container guides.

## B2. Link rules for the dual-rendered README

Audit every link after deciding the split:

- A document included in Doxygen may use a relative link only when its
  generated target is verified.
- A repository file not included in Doxygen must use an absolute
  `https://github.com/NVIDIA/nvnmos/blob/main/...` link.
- A published GStreamer or gRPC reference should use its stable Pages URL.
- Build both renderings and click-test the navigation.

Apply the same rule to Markdown rendered both on GitHub and by Hotdoc or the
gRPC generator.

## B3. C quick start before the encyclopaedia

The C guide currently leads with transport enums and `x-nvnmos-*` extensions.
Restructure it toward:

1. **Minimal embed** — create a Node; add one Sender or Receiver; run; destroy.
2. **When to use the C API** — contrast it briefly with daemon and GStreamer.
3. **Transports and transport files** — keep existing tables after the example.
4. **Extensions** — retain as reference, not as the opening section.
5. **API changes** — keep as migration guidance or release notes.

## B4. Align C and daemon vocabulary carefully

C API docs must not grow daemon/gRPC concepts. Shared ideas (Node seed, caller-chosen resource `name`, transport file, activation callback) should use the same user terms as GStreamer guides where accurate, without mentioning `nvnmosd` RPCs in `nvnmos.h` comments.

## B5. Example application as the tutorial spine

Treat `nvnmos-example` output steps as the authoritative walkthrough; ensure the README’s “Running the Example Application” section stays early in the C journey (link from the new quick start).

---

# Workstream C — Daemon users and gRPC API docs

## C1. Operator guide stays Markdown

Keep `rust/nvnmosd/README.md` as the operator entry: build/run, UDS, env vars, session GC contract, Node flavours. Lead with a **minimal client sequence** (open session → subscribe activations → add resource → ack loop → close) before the env-var catalogue.

Own the narrative here once. Link to generated gRPC pages for per-RPC detail.
Link to design docs for lock ordering and history.

## C2. Slim `nvnmosd.proto` comments

Apply the proto comment rules before or with generated documentation:

- State session GC and resubscribe policy once.
- State name uniqueness and handle-versus-ID conventions once.
- State transport-file embedding rules once.
- Remove GStreamer and design-document asides.
- Keep per-RPC error codes and symbol-specific preconditions.

## C3. Generated gRPC reference (third documentation product)

Integrating protobuf HTML into Doxygen or Hotdoc is a poor fit (different object model, separate toolchain, little shared navigation). Prefer a **third generator** publishing beside the existing Pages artifacts, e.g. `public/grpc/` or `public/nvnmosd/`.

**Recommended approach (to confirm at implementation time):**

| Option | Pros | Cons |
| --- | --- | --- |
| **sabledocs** | Built for gRPC + messages; Markdown in comments; static HTML | Extra Python tool in Pages CI |
| **protoc-gen-doc** | Common, simple HTML/Markdown from comments | Historically weaker gRPC-service presentation unless templated |
| **Buf / BSR** | Hosted polish | External dependency / org setup; less “in-tree Pages” |

Default recommendation: **sabledocs** (or protoc-gen-doc if CI wants a single
`protoc` plugin and no Python), fed by the slimmed `nvnmosd.proto` with
`--include_source_info`. Generated pages are the RPC/message reference. Do not
restore guide narrative in generated comments.

Pages job sketch:

1. Existing Doxygen → `public/`
2. Existing Hotdoc → `public/gstreamer/`
3. New: generate gRPC HTML → `public/grpc/` (name TBD)
4. Cross-link from top-level README, `rust/nvnmosd/README.md`, and optionally Hotdoc portal / Doxygen header

## C4. Split of emphasis

| Concern | Where |
| --- | --- |
| Minimal client sequence; GC why + env defaults; Node flavours | Operator README |
| `_id` vs `_handle`; subscribe-before-add; per-RPC errors | Proto → generated pages |
| Lock ordering, history | Design docs |

## C5. Publish daemon usage with the gRPC reference

Test whether the selected generator can use the operator guide as its landing
page. Prefer one `rust/nvnmosd/README.md` source followed by generated RPC and
message reference.

If that is not possible, publish a short landing page that links to the
operator guide. Do not copy the operator guide into generator-specific content.

---

# Cross-cutting

## Shared NvNmos concepts guide

Some concepts apply to all three user surfaces and already cause confusion.
Explain them once in the shared project-level guide at
`doc/user/concepts.md`. Publish it in the Doxygen site and link to its stable
Pages URL from the C, daemon, and GStreamer guides.

Each audience guide should add only its API-specific names and examples. It
should not copy the shared explanation.

### Configuring transport files

Distinguish three related artifacts:

1. A **configuring transport file** is a southbound NvNmos configuration
   document.
2. An IS-05 Sender `/transportfile` is a northbound NMOS API result.
3. Runtime SDP or an MXL flow definition describes an actual media transport
   or flow.

NvNmos uses SDP and MXL flow-definition syntax because those formats are
familiar and already carry much of the required information. Shared syntax
does not make the artifacts identical.

Explain:

- Which values configure identity, capabilities, and initial transport
  parameters
- Which `x-nvnmos-*` attributes and `urn:x-nvnmos:tag:*` values are NvNmos
  extensions
- How configuration differs from the IS-05 transport file
- What transport document is delivered after controller activation

Use “configuring transport file” only for the southbound input. Use “IS-05
transport file,” “SDP,” or “MXL flow definition” for the other artifacts.

### Activation direction

Explain the direction before naming APIs:

- The **activation callback** handles an activation initiated through the
  northbound IS-05 API. NvNmos asks the application to apply the requested
  data-plane state.
- `nmos_connection_activate` reports an application-originated state change.
  It updates the NvNmos model after the application changes its data plane.
- `nmos_connection_activate` neither performs data-plane activation nor invokes
  the callback.

Map that model to each surface:

| Surface | Controller-originated change | Application-originated change |
| --- | --- | --- |
| C API | `nmos_connection_activation_callback` | `nmos_connection_activate` |
| gRPC | `SubscribeActivations` + `AckActivation` | `SyncResourceState` |
| GStreamer | Controller activation handled by the element | `auto-activate=true` |

### Identity and resource layering

Define the boundary:

- **Northbound** means NMOS APIs used by controllers. Identity is standard NMOS
  UUIDs.
- **Southbound** means the NvNmos C API or `nvnmosd` gRPC API. Identity uses a
  Node seed and caller-chosen names.

A southbound Sender or Receiver is addressed by:

```text
Node seed + resource side + caller-chosen name
```

Names are unique per side within a Node. A Sender and Receiver may share the
same name.

Explain the one-to-many mapping:

- A Node seed identifies an NvNmos Node and its NMOS Node and Device resources.
- One southbound Sender creates an NMOS Source, Flow, and Sender.
- One southbound Receiver creates an NMOS Receiver.
- NvNmos derives stable NMOS UUIDs from the seed, side, and name as applicable.
- Controllers continue to use only standard NMOS UUIDs.
- `label`, `description`, group hints, and similar values are human-readable
  metadata. They are not southbound identity.

Document the mappings exposed by each API:

- The C API can make or query Node, Sender, Receiver, Source, and Flow UUIDs.
- gRPC returns the Node UUID and the added Sender or Receiver UUID. It does not
  currently return Source and Flow UUIDs.
- NvNmos publishes the caller-chosen name as `urn:x-nvnmos:tag:name` on the
  corresponding NMOS Sender or Receiver. This helps diagnostics. Controllers
  must not treat it as resource identity.

Include one worked example that shows one seed and Sender name producing
several NMOS resource UUIDs.

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

## Doxygen typography

The current Doxygen prose is visually dense. The small font size makes the
README main page harder to scan.

Add a small stylesheet through `HTML_EXTRA_STYLESHEET` rather than editing
Doxygen's generated CSS. Prototype:

- A larger prose font size
- More line height
- More space around headings and list items
- A sensible maximum line width
- Responsive behaviour on narrow screens

Scope these rules to user-guide and main-page content where possible. Do not
accidentally enlarge signatures, source listings, navigation, or compact API
tables. A wrapper class in Markdown may provide a stable selector.

Compare:

- The main README page
- A split C usage page
- A normal API symbol page
- Desktop and mobile widths

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
2. **Default `transport` to `udp`** in Rust state and GObject metadata; document the default change.
3. **Documentation topology and link audit** — decide the README split, Doxygen `INPUT`, and where daemon/GStreamer guides are published.
4. **Shared concepts guide** — explain transport documents, activation direction, and identity/resource layering once.
5. **Doxygen typography** — add and validate a scoped prose stylesheet.
6. **gst-nmos-rs guide** — publish it with Hotdoc; add Quick start and configuration patterns; trim dense prose.
7. **C guide split and rewrite** — put minimal embedding before extensions and migration notes.
8. **nvnmosd README** — put the minimal client sequence first and publish it with the gRPC reference if supported.
9. **Slim `nvnmosd.proto` comments** — apply the same within-layer rule as blurbs.
10. **gRPC Pages product** — generate from the slimmed proto and cross-link it.
11. **Lifecycle / mutability / troubleshooting** on the GStreamer guide.
12. **Move Sync Testing** and other contributor sections out of the primary user path.
13. **Readability and link pass** — shorten sentences, use bullets, verify links, and mark design-plan status.

Item 3 prevents document moves from creating broken Doxygen links or duplicate
guides. Item 10 is the main tooling addition. Do not publish generated gRPC
docs before slimming the proto comments.

## Success criteria

- A new GStreamer user can run sender + receiver from docs without reading property tables first.
- A new C embedder can create a Node and one resource from docs without first reading extension attribute grammar.
- A new daemon client can implement the happy-path RPC sequence from the operator guide + generated reference without reading the design record.
- Every top-level README link works in both GitHub and Doxygen.
- Usage guides are published with their respective references where one source
  can feed the generator.
- Shared concepts have one canonical explanation linked from all user surfaces.
- Users can distinguish southbound names from NMOS UUIDs.
- Users can distinguish activation callbacks from application-originated
  activate/sync calls.
- “Configuring transport file” is not presented as synonymous with an IS-05
  `/transportfile` or a runtime MXL flow definition.
- User guides use short sentences and scannable lists.
- Doxygen guide prose is readable without making API pages oversized.
- `gst-inspect-1.0 nmossink` blurbs describe outcomes; daemon/IS-05 mapping detail appears only in guides.
- A pipeline that omits `transport` uses RTP/UDP; MXL remains explicit.
- Proto comments state each policy once; the operator README owns lifecycle
  narrative.
