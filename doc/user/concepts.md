<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Core NvNmos Concepts

NvNmos connects an application-owned data plane to NMOS control APIs. The same
concepts appear in the C API, the `nvnmosd` gRPC API, and the GStreamer
elements. This guide defines the common terms once.

## Configuring Transport Files

A **configuring transport file** is a document supplied by an application to
NvNmos. NvNmos uses it to create NMOS resources and their initial connection
state.

The format depends on the transport:

- RTP/UDP uses SDP.
- MXL uses an MXL flow definition in JSON.

These documents also carry
[NvNmos extensions](transport-files.md#nvnmos-extensions-to-the-transport-file)
for information that the standard formats do not contain. Examples include the
caller's resource name and an MXL domain ID.

Several related documents use the same syntax but have different roles:

- The **configuring transport file** supplies the details NvNmos needs to
  create a Sender or Receiver.
- An IS-05 Sender **`/transportfile`** is the result exposed to NMOS
  controllers.
- An activation callback or event carries the effective active transport file
  for the requested state.

For RTP/UDP, an activation carries effective active SDP. A Receiver activation
may use SDP supplied in an IS-05 `PATCH` instead of the configuring SDP. NvNmos
applies the active IS-05 `transport_params` before delivering the SDP to the
application.

For MXL, an activation returns the configuring MXL flow definition with the
active `mxl_domain_id` and `mxl_flow_id` applied. The application can then use
those to discover the `flow_def.json` and configure its MXL data plane.

See [Configuring Transport Files](transport-files.md) for extension syntax and
complete minimal Receiver examples.

## Activation Direction

An activation can begin on either side of NvNmos.

### Controller-Originated Activation

Not every IS-05 `PATCH` causes an activation. A `PATCH` may only update staged
state. When a controller requests an immediate or scheduled activation:

1. NvNmos delivers the requested active state to the application at the
   activation time.
2. The application applies or rejects it.
3. The application reports success or failure to NvNmos.

For an immediate activation, this happens while the controller's request is in
progress, so success or failure can be reported to the controller. A scheduled
activation occurs after that request has been accepted and responded to.

The surface APIs are:

- C: `nmos_connection_activation_callback`.
- gRPC: `SubscribeActivations`, followed by `AckActivation`.
- GStreamer: the element handles the activation and updates its inner data
  path.

### Application-Originated State Change

1. The application changes its data plane independently of an IS-05 request.
2. The application reports the resulting state to NvNmos.
3. NvNmos updates its IS-04 and IS-05 model.
4. When the Node is registered, NvNmos sends the resulting IS-04 resource
   changes to the Registry. Controllers with matching Query API WebSocket
   subscriptions receive the update.

The surface APIs are:

- C: `nmos_connection_activate`.
- gRPC: `SyncResourceState`.
- GStreamer: `auto-activate=true` applies the element's configured data path
  without waiting for a controller.

An activation callback and an activate or sync call therefore have opposite
directions. The callback asks the application to handle a controller-originated
request. The call reports a state change that originated in the application.

## Identity and Resource Layering

NMOS controllers and NvNmos applications identify resources differently.

NMOS controllers use the standard NMOS APIs. These APIs identify resources with
NMOS UUIDs.

An application using the C API or the `nvnmosd` gRPC API identifies a Sender or
Receiver using Node seed, resource side, and caller-chosen name.

The resource side is Sender or Receiver. Names must be unique among resources
on the same side of a Node. A Sender and a Receiver on the same Node may share
the same name.

One application resource can correspond to several NMOS resources:

- A Node seed determines the stable NMOS Node and Device resource identity.
- One application Sender creates an NMOS Source, Flow, and Sender.
- One application Receiver creates an NMOS Receiver.
- NvNmos derives stable NMOS UUIDs from the seed, side, and caller-chosen name.
- Controllers continue to address these resources by their NMOS UUIDs.

Keep the seed and names stable to preserve the derived UUIDs across restarts.
Changing either changes the corresponding resource UUIDs.

Labels, descriptions, group hints, and similar values are human-readable
metadata. They do not uniquely identify a resource.

### Worked Identity Example

For the Node seed `example-node` and caller-chosen name `camera-video`, NvNmos
derives:

```text
NMOS Node ID:     c4db3b6e-8040-56f6-9dab-8dcd9c520ac2
NMOS Device ID:   3140a82e-99f7-5e26-a950-5ec0e0c8df91
NMOS Source ID:   0407ee23-6ce6-543f-858f-43e70c809067
NMOS Flow ID:     fba225a6-dde7-5dfe-9ae7-c51719610b41
NMOS Sender ID:   3b69e32b-4535-5396-9fd5-c38f1a0b2776
NMOS Receiver ID: 658502d5-ae3e-5a4e-ae39-b803ec2468e4
```

The Node and Device UUIDs depend only on the seed. The Source, Flow, and Sender
UUIDs use the name on the Sender side. The Receiver UUID uses the same name on
the Receiver side. All six UUIDs are distinct and deterministic.

The corresponding `nmos_make_*_id` functions can compute each UUID before
starting a server.

The NMOS Flow ID is not the MXL `mxl_flow_id` transport parameter. The NMOS
Flow ID identifies the IS-04 Flow resource. `mxl_flow_id` identifies the MXL
data-plane flow selected through IS-05.

### API-Specific Identifiers

#### C API

The C API can compute or query Node, Device, Source, Flow, Sender, and Receiver
UUIDs.

#### gRPC API

The gRPC API:

- returns the NMOS Node UUID from `AddNode` and `OpenSession`;
- returns the NMOS Sender or Receiver UUID from `AddSender` or `AddReceiver`;
- issues opaque session and resource handles for later gRPC calls.

Handles are allocated by a running `nvnmosd` instance. They remain valid only
while the referenced object and daemon process exist. They are not NMOS UUIDs
and are not visible to controllers.

#### gst-nmos-rs Properties

The `sender-name`, `receiver-name`, and `channelmapping-name` properties provide
the caller-chosen names used for NvNmos identity. A GStreamer element's name and
a pad's name identify objects within the pipeline only. They do not provide
NvNmos identity or affect NMOS UUIDs.

NvNmos also publishes the caller-chosen name as `urn:x-nvnmos:tag:name` on the
corresponding NMOS Sender or Receiver. This helps correlate application
configuration with controller-visible resources during debugging and
diagnostics. Controllers must continue to use the NMOS UUID as resource
identity.
