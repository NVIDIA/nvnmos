<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Audio Channel Mapping

`nmosaudiochannelmap` exposes AMWA IS-08 routing as an audio matrix in the
pipeline. Request one `sink_%u` pad for each input stream and one `src_%u` pad
for each output stream. Every sink pad becomes an IS-08 Input; every source pad
becomes an IS-08 Output.

Request and configure every pad before the element reaches READY. Channel
counts come from negotiated audio caps by default, or from the pad's `channels`
property when it is set. The important pad properties are:

- Sink pads: `input-id`, `receiver-name`, `label`, and `description`.
- Source pads: `output-id`, `sender-name`, `label`, `description`, and the
  optional initial `active-map`.

`receiver-name` associates an Input with the corresponding NMOS Receiver.
`sender-name` associates an Output with the Source belonging to the
corresponding NMOS Sender. The pad `label` is published as IS-08
`/properties/name`; it is not the caller-chosen Sender or Receiver name.

All elements that use the same `node-seed` contribute to one NMOS Node and one
shared IS-08 Channel Mapping API. This includes multiple
`nmosaudiochannelmap` elements. Each element's required
`channelmapping-name` identifies the subset of Inputs and Outputs that it owns;
it does not create a separate IS-08 API. The name must therefore be unique
within the Node.

By default an Output may advertise unrestricted routable Inputs. Set
`restrict-routable-inputs=true` to limit each Output to the Inputs owned by
that element. The element starts with an identity map where its channel
geometry permits, then applies routing changes requested by an IS-08
Controller.

See the
[`nmosaudiochannelmap` reference](https://nvidia.github.io/nvnmos/gstreamer/nmos/nmosaudiochannelmap.html)
for all element and pad properties and a complete pipeline example.
