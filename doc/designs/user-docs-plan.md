## Overall assessment

The documentation is technically strong and unusually complete for a project still evolving quickly. The top-level README now explains the three consumption models—C library, daemon/gRPC, and GStreamer—so a visitor can identify the relevant entry point without first understanding the repository layout. ([GitHub][1])

The main usability problem is no longer missing information. It is **information hierarchy**:

* The first material users encounter is often the complete configuration surface.
* Essential properties, advanced overrides, daemon internals, transport-specific details, and implementation diagnostics are presented at roughly the same level.
* Property blurbs are sometimes trying to serve simultaneously as command-line help, API reference, design documentation, and troubleshooting guidance.

The most valuable changes would therefore be editorial rather than adding much more content.

# Highest-impact changes

## 1. Put a minimal working pipeline before the property catalogue

The `gst-nmos-rs` README currently moves directly from its one-sentence description to “Property Surface.” The first executable guidance appears much later under building, loading, demos, and pipeline examples. ([GitHub][2])

A new user first wants to know:

1. What processes must be running?
2. What is the smallest sender pipeline?
3. What is the smallest receiver pipeline?
4. Does it require an NMOS controller, or can it start immediately?
5. Which transport should they choose?

I would put a **Quick start** immediately after the introduction, perhaps using `transport=udp`, since that has the least NVIDIA-specific infrastructure:

```markdown
## Quick start

`nmossink` creates an NMOS Sender and `nmossrc` creates an NMOS Receiver.
Both connect to a running `nvnmosd`.

Terminal 1:

    cargo run -p nvnmosd

Terminal 2:

    gst-launch-1.0 -v \
      videotestsrc is-live=true ! \
      video/x-raw,format=UYVP,width=1920,height=1080,framerate=25/1 ! \
      nmossink transport=udp \
        node-seed=example-node \
        sender-name=video \
        interface-ip=192.168.1.10 \
        destination-ip=239.100.0.1 \
        destination-port=5004 \
        auto-activate=true

Use `auto-activate=true` for standalone testing. In a managed NMOS
system, leave it false and activate the Sender or Receiver through IS-05.
```

Then link to one equally small receiver example and the full pipeline examples.

This would do more for usability than almost any expansion of the property reference.

## 2. Add a configuration-model explanation before listing properties

The most important conceptual fact is that there are several ways to configure a resource:

* supply a complete transport file;
* supply a transport file path;
* synthesize one from caps and endpoint properties;
* let an IS-05 controller activate it later;
* use `auto-activate` for development.

The current table explains these individually, but users must reconstruct the model themselves. For example, `transport-file` says it may be replaced by `caps`, `mxl-flow-id`, `transport-caps`, and endpoint properties, while `transport-file-path` is described separately. ([GitHub][2])

Add a short section such as:

```markdown
## Choosing how to configure a resource

Most applications should use one of these patterns:

| Pattern | Set initially | Intended use |
|---|---|---|
| Controller-managed | identity, transport, caps | Production NMOS system; IS-05 supplies network parameters |
| Self-starting | identity, caps, endpoint properties, `auto-activate=true` | Development and fixed pipelines |
| Complete transport file | `transport-file-path` | Existing SDP or MXL flow definition |
| Programmatic transport file | `transport-file` | Applications constructing SDP or JSON in memory |
```

Also state precedence once, centrally:

> Explicit element properties override corresponding values from the transport file. `transport-file` and `transport-file-path` are mutually exclusive.

That would eliminate repeated precedence explanations from many blurbs.

## 3. Separate “common”, “transport-specific”, and “advanced” properties

The current property surface begins with low-frequency Node configuration properties such as `http-port`, `host-name`, `domain`, `registration-url`, and `system-url`, before the user reaches the Sender/Receiver identity and media configuration they are much more likely to need. ([GitHub][2])

I would order the documentation around user intent:

### Essential

* `node-seed`
* `sender-name` / `receiver-name`
* `transport`
* `caps`
* `auto-activate`

### RTP/UDP network configuration

* `interface-ip`
* `destination-ip`
* `destination-port`
* `source-ip`
* `source-port`
* `multicast-ip`
* `transport-caps`

### MXL configuration

* `mxl-domain-path`
* `mxl-domain-id`
* `mxl-flow-id`

### Identity and controller presentation

* `label`
* `description`
* `group-hint`
* receiver capabilities mode

### Advanced Node/session configuration

* `daemon-uri`
* `http-port`
* `host-name`
* `domain`
* `registration-url`
* `system-url`

### Inner-element overrides

* `transport-properties`
* `pay-properties`
* `depay-properties`

This ordering should also be reflected in the generated element reference where practical.

## 4. Make each property blurb answer only four questions

A useful `gst-inspect` blurb should usually answer:

1. What does this configure?
2. What is the default or unset behaviour?
3. Which transports or element does it apply to?
4. What overrides what?

Several current blurbs go considerably beyond that. For example, the receiver `source-ip` description explains the IS-05 field, SSM behaviour, generated SDP, and the different property mappings for `udpsrc2` and `udpsrc`. ([GitHub][3])

That information is correct and valuable, but the implementation mapping belongs in detailed reference documentation, not necessarily in `gst-inspect`.

I would use a two-layer approach:

**Blurb**

> Remote source address used for source-specific multicast reception. Empty accepts any source. Applies to `udp`, `udp2`, and `nvdsudp`; ignored for `mxl`.

**Long-form documentation**

> Maps to IS-05 `source_ip`, the SDP `a=source-filter` attribute, and the selected UDP source element’s corresponding source-filter property.

The same principle applies to `destination-port`, whose current sink blurb includes the generated SDP location, `udpsink` mapping, transport-file fallback, the canonical port 5004, and the `nmos-cpp` symbolic name. ([GitHub][4])

The `nmos-cpp` implementation name is useful for developers debugging behaviour, but it is not needed in normal element help.

# Property blurb recommendations

## `node-seed`

Current wording is approximately:

> NvNmos Node seed; sessions sharing this seed share a Node.

That is accurate but assumes users understand both “seed” and “session.” The daemon design explains that this value is the daemon lookup key and deterministically controls the NMOS Node ID. ([GitHub][5])

Suggested blurb:

> Stable identifier used to derive the NMOS Node ID. Elements using the same value and daemon join the same NMOS Node.

This tells users both why it matters and when values should match.

The detailed docs should add:

* It need not necessarily be written in UUID syntax, if that is true.
* It should remain stable across restarts when stable NMOS resource identity is desired.
* Sender and Receiver names must be unique per side within that Node.

## `transport`

The current description is thorough but too dense for one enum blurb, listing internal element chains, fallback behaviour, Rivermax capabilities, hardware requirements, and Mode 3. ([GitHub][2])

Suggested blurb:

> Data-plane implementation: `mxl` for MXL shared memory, `udp` for gst-plugins-good RTP/UDP, `udp2` for gst-plugins-rs RTP/UDP where available, or `nvdsudp` for DeepStream/Rivermax ST 2110.

Move these details to the transport section:

* precise payloader/depayloader selection;
* per-element fallback in `udp2`;
* Mode 3;
* ConnectX and Rivermax prerequisites.

Also document prominently that the enum currently defaults to `mxl`, as shown by the property declaration. ([GitHub][3]) This default could surprise users without MXL installed. Consider whether `udp` would be a friendlier default, or whether forcing the user to set `transport` explicitly would be safer.

## `transport-file`

Suggested blurb:

> Literal SDP (`udp`, `udp2`, `nvdsudp`) or MXL flow-definition JSON (`mxl`). Mutually exclusive with `transport-file-path`. Explicit element properties override corresponding values in the file.

Avoid discussing `AddSender` and `AddReceiver` in the blurb. Those are daemon implementation details.

## `transport-file-path`

Suggested blurb:

> Path to an SDP or MXL flow-definition file, read when the element changes from NULL to READY. Mutually exclusive with `transport-file`.

The NULL-to-READY timing is important because it tells the user when file changes are observed. The explanation about the `gst-launch` parser belongs in the README, perhaps as a note:

> Prefer this property in `gst-launch-1.0`, because literal multiline SDP is awkward to quote.

## `caps`

The nickname “Essence caps” is good. ([GitHub][3])

The blurb should distinguish its two roles:

> GStreamer caps describing the unpacketized media. Used to advertise or constrain the NMOS resource and, when no complete transport file is supplied, to synthesize its transport description.

It should also answer:

* Is it required for both elements when a complete file is supplied?
* For `nmossink`, can it be inferred from incoming negotiated caps?
* For `nmossrc`, must it be set before activation to expose a useful source pad template or placeholder caps?
* What happens if controller-supplied SDP disagrees with it?

The current source code warns that downstream negotiation may fail until either `caps` or a transport file is set. ([GitHub][3]) That consequence should be stated directly in the user documentation, not merely logged.

## `auto-activate`

This property needs especially clear language because it changes the operating model.

Suggested blurb:

> Activate immediately from the configured transport parameters instead of waiting for an IS-05 controller. Intended mainly for development, fixed pipelines, and tests.

The README should explain whether the resource is still exposed through IS-05 afterward and whether subsequent controller activations replace the startup configuration.

## `mxl-domain-path`

The current blurbs contain useful but highly detailed behaviour around `domain_def.json`, cross-checking IDs, application-resolved tags, and the inner `mxlsrc` or `mxlsink` property. ([GitHub][3])

Suggested blurb:

> Local path to the MXL domain. If it contains `domain_def.json`, the domain ID is loaded from that file and checked against `mxl-domain-id` when both are set.

The remaining data-plane wiring details belong in the MXL section.

## `mxl-flow-id`

The source and sink descriptions are asymmetrical. The source version gives a detailed explanation of controller activation and development use; the sink version simply describes the inner flow target and precedence. ([GitHub][3])

Use parallel language:

For `nmossink`:

> MXL flow UUID produced by this Sender. Overrides the flow ID in the transport file.

For `nmossrc`:

> MXL flow UUID consumed by this Receiver. Normally supplied by IS-05; set it with `auto-activate=true` for a fixed or development pipeline. Overrides the flow ID in the transport file.

## Endpoint IP and port properties

These should use consistently user-oriented terminology.

For a Sender:

* `interface-ip`: local interface from which packets are sent;
* `source-ip`: local source address advertised in IS-05/SDP, if distinct;
* `destination-ip`: remote unicast address or multicast group;
* `destination-port`: remote RTP port.

For a Receiver:

* `interface-ip`: local interface on which packets are received;
* `source-ip`: optional remote source filter for SSM;
* `multicast-ip` or `destination-ip`: multicast group to join;
* `destination-port`: local RTP listen port.

Avoid starting each blurb with:

> IS-05 sender transport_params `destination_ip`...

That is specification-first rather than task-first. Put the IS-05 field at the end:

> Corresponds to IS-05 `destination_ip`.

## `transport-properties`, `pay-properties`, and `depay-properties`

These are powerful escape hatches and need:

* one concrete syntax example;
* a warning that property names depend on the selected inner element;
* the point at which they are applied;
* behaviour for unknown properties;
* whether automatically calculated values override these fields or vice versa.

The README gives one `transport-properties` example for `gpu-id` and `sync`, but only in the DeepStream section. ([GitHub][2]) Put a generic example adjacent to the property:

```text
transport-properties="properties,buffer-size=4194304"
pay-properties="properties,pt=96"
```

Also state whether the structure name must literally be `properties`.

# Markdown structure and discoverability

## 5. Give every README a declared audience

There are several documentation layers:

* repository README;
* Rust workspace README;
* daemon README;
* GStreamer README;
* Docker READMEs;
* design documents;
* pipeline examples.

The repository links these reasonably well, but some documents still mix user guide, implementation reference, test specification, and design record. The Rust README, for example, contains detailed discussion of every RPC exercised by an example, internal index keys, activation acknowledgement, and HTTP API construction. ([GitHub][6])

Add a small header to each substantial document:

```markdown
**Audience:** users building GStreamer NMOS pipelines  
**Status:** functional preview; interfaces may change  
**Start here if:** you want to run `nmossrc` or `nmossink`  
```

For design documents:

```markdown
**Audience:** contributors and maintainers  
**Not a user guide:** see ...
```

That prevents users from treating a design plan as current operational documentation.

## 6. Create a single “Which guide do I need?” page or table

The top-level “Ways To Use NvNmos” is already a good foundation. ([GitHub][1]) Expand it slightly into a task-oriented table:

| Goal                                    | Start here                |
| --------------------------------------- | ------------------------- |
| Embed NMOS in a C/C++ application       | C API quick start         |
| Run NMOS out of process                 | `nvnmosd` user guide      |
| Add NMOS Senders/Receivers to GStreamer | `gst-nmos-rs` quick start |
| Run in Docker or Kubernetes             | container guide           |
| Understand implementation decisions     | design documents          |
| Run an end-to-end controller demo       | interactive demo          |

Users should not need to understand the repository’s C/Rust division before selecting a guide.

## 7. Move test-suite detail out of the main GStreamer guide

The `gst-nmos-rs` README contains a substantial “Sync Testing” section describing the test media, caption alignment, ST 2038 extraction and combination, test names, transport-specific formats, skip conditions, and plugin versions. ([GitHub][7])

This is valuable contributor documentation, but it interrupts the user journey.

Move it to:

* `tests/README.md`, or
* `CONTRIBUTING.md`, or
* `doc/testing/gst-nmos-rs.md`.

Leave a short link in the main README:

> For end-to-end A/V and ancillary-data integration tests, see the gst-nmos-rs testing guide.

## 8. Separate supported behaviour from design plans

The main README links to files named `*-plan.md` for ST 2022-7 and `nvdsudp`. ([GitHub][2]) A plan may contain obsolete assumptions after implementation lands.

Either:

* rename completed plans to `design.md` or `implementation-notes.md`;
* add a prominent status heading inside them;
* or keep the plan but link users to a concise current-behaviour document first.

The user-facing README should be authoritative about supported behaviour. Design records should explain why it works that way.

# Missing user-oriented material

## 9. Add a lifecycle section

Users need a simple explanation of what happens at each GStreamer state transition:

* when the daemon connection opens;
* when the NMOS Node is created or joined;
* when Sender/Receiver resources are registered;
* when transport files are read;
* when inner elements are created;
* when activation can occur;
* what happens on PAUSED/PLAYING;
* when resources and Nodes are removed.

The property declarations reveal that many sink properties are mutable only up to READY. ([GitHub][4]) That is a crucial practical constraint, but users should not have to infer it from `gst-inspect` flags.

A compact table would suffice:

| Transition       | Action                                           |
| ---------------- | ------------------------------------------------ |
| NULL → READY     | Read files, connect to daemon, register resource |
| READY → PAUSED   | Construct or prepare active data path            |
| IS-05 activation | Reconfigure and enable data path                 |
| READY → NULL     | Remove resource and close session                |

Obviously, adjust this to match the exact implementation.

## 10. Add a property mutability column

The current README table has Property, Type, Required, and Notes. ([GitHub][2]) Add:

* Default
* Applies to
* Mutable until

For example:

| Property         | Type   | Default | Applies to | Mutable until |
| ---------------- | ------ | ------: | ---------- | ------------- |
| `transport`      | enum   |   `mxl` | both       | READY         |
| `destination-ip` | string |   empty | RTP Sender | READY         |
| `auto-activate`  | bool   |   false | both       | READY         |

This is much more useful than “required?” because many properties are conditionally required depending on the configuration pattern.

Replace “Required?” with **When needed**.

For example:

* `node-seed`: always;
* `caps`: unless fully described by transport file, subject to source/sink inference;
* `destination-ip`: self-activated RTP Sender;
* `mxl-domain-path`: MXL data path;
* `transport-file`: one configuration option, not inherently required.

## 11. Add a troubleshooting section based on likely first failures

The most useful entries would be:

### Plugin not found

Commands:

```bash
gst-inspect-1.0 nmos
gst-inspect-1.0 nmossink
echo "$GST_PLUGIN_PATH"
```

### Cannot connect to daemon

Explain expected Unix socket and `daemon-uri`.

### Node appears but Sender/Receiver does not

Explain names, state transition, daemon logs, and transport configuration.

### Resource exists but no packets flow

Explain `auto-activate` versus IS-05 activation.

### `nmossrc` fails caps negotiation

State that it needs `caps`, a usable transport file, or activation-provided media information. This matches the existing source warning. ([GitHub][3])

### Registry does not discover the Node

Explain DNS-SD/mDNS, `domain=local`, `registration-url`, host-name resolution, and container networking.

### MXL starts but cannot find a flow/domain

Explain domain mount, `domain_def.json`, UUID matching, and shared-host requirements.

### Rivermax path fails

The current prerequisite material—DeepStream, Rivermax SDK, ConnectX-5+, `CAP_NET_RAW`, and plugin/library paths—is useful and should become a checklist rather than prose. ([GitHub][2])

# Smaller consistency improvements

## Terminology

Choose and apply consistently:

* **Node**, **Sender**, and **Receiver** when referring to NMOS resources;
* lowercase “element” and “pipeline” for GStreamer;
* “transport file” as the generic term;
* “SDP” and “MXL flow definition” for specific representations;
* “data plane” rather than alternating between “data path” and “data plane,” unless the distinction is intentional;
* “ID” rather than “id” in prose and nicknames: “MXL domain ID,” “MXL flow ID.”

## Explain `name`, `label`, and `description` together

This is a common source of confusion:

* `sender-name` / `receiver-name` is the stable programmatic resource name used internally and in activation routing;
* `label` is user-facing controller text;
* `description` is longer user-facing text.

The design notes show that name is operationally significant to the daemon’s resource lookup. ([GitHub][5]) Put this distinction in one visible callout.

## Define “route-dependent”

The current table uses “route-dependent” under Required. ([GitHub][2]) That is not immediately clear terminology for users. Prefer:

* “configuration-dependent,”
* “required for this configuration pattern,” or
* a direct condition such as “required unless caps and endpoint properties are supplied.”

## Avoid back-end RPC names in normal user docs

Terms such as `OpenSession`, `OpenSessionResponse`, `AddSender`, and `AddReceiver` help daemon-client developers, but GStreamer users mostly need observed behaviour. The existing `http-port` and transport-file descriptions expose these RPCs. ([GitHub][2])

Move that detail to the daemon API reference and say, for example:

> Only the first element that creates a Node controls this value; later elements joining the same Node do not change it.

That communicates the behaviour directly.

# Suggested priority order

I would implement the documentation changes in this order:

1. **Add a minimal Quick start before the property table.**
2. **Explain the four configuration patterns and precedence rules.**
3. **Reorganize properties into essential, transport-specific, and advanced groups.**
4. **Shorten `gst-inspect` blurbs and move implementation mappings into generated long-form reference.**
5. **Document lifecycle, mutability, and activation behaviour.**
6. **Add troubleshooting for daemon connection, activation, caps negotiation, discovery, MXL, and Rivermax.**
7. **Move integration-test detail out of the primary user guide.**
8. **Mark design plans clearly and rationalize terminology.**

The first four would produce the largest immediate improvement. The current documentation already contains most of the required facts; it mainly needs to guide users through them in the order they encounter problems, rather than in the order the implementation exposes configuration fields.

[1]: https://github.com/NVIDIA/nvnmos "GitHub - NVIDIA/nvnmos: NVIDIA NMOS (Networked Media Open Specifications) Library · GitHub"
[2]: https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/README.md "nvnmos/rust/gst-nmos-rs/README.md at main · NVIDIA/nvnmos · GitHub"
[3]: https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/src/nmossrc/imp.rs "nvnmos/rust/gst-nmos-rs/src/nmossrc/imp.rs at main · NVIDIA/nvnmos · GitHub"
[4]: https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/src/nmossink/imp.rs "nvnmos/rust/gst-nmos-rs/src/nmossink/imp.rs at main · NVIDIA/nvnmos · GitHub"
[5]: https://github.com/NVIDIA/nvnmos/blob/main/doc/designs/nvnmosd/README.md?utm_source=chatgpt.com "nvnmos/doc/designs/nvnmosd/README.md at main - GitHub"
[6]: https://github.com/NVIDIA/nvnmos/blob/main/rust/README.md?utm_source=chatgpt.com "nvnmos/rust/README.md at main · NVIDIA/nvnmos · GitHub"
[7]: https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/README.md?utm_source=chatgpt.com "nvnmos/rust/gst-nmos-rs/README.md at main - GitHub"
