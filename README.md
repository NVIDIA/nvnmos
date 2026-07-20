<!--
SPDX-FileCopyrightText: Copyright (c) 2022-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# NVIDIA Networked Media Open Specifications Library

## Ways To Use NvNmos

NvNmos can be integrated in three ways:

1. **C library** (`libnvnmos`) — embed the NMOS control plane directly in a Media Node application.
   - Start with the [C API guide](doc/user/c-api-guide.md), then use the [C API reference](https://nvidia.github.io/nvnmos/nvnmos_8h.html).
2. **Daemon and gRPC API** (`nvnmosd`) — run the control plane out-of-process; Media Node applications attach as clients over gRPC.
   - See [`rust/nvnmosd/README.md`](https://github.com/NVIDIA/nvnmos/blob/main/rust/nvnmosd/README.md), the Rust workspace quick start in [`rust/README.md`](https://github.com/NVIDIA/nvnmos/blob/main/rust/README.md), and the design record in [`doc/designs/nvnmosd/README.md`](https://github.com/NVIDIA/nvnmos/blob/main/doc/designs/nvnmosd/README.md).
3. **GStreamer elements** (`gst-nmos-rs`) — `nmossrc` and `nmossink` talk to `nvnmosd` and set up the data path (MXL, RTP/UDP, DeepStream).
   - See the [published element reference](https://nvidia.github.io/nvnmos/gstreamer/), the usage guide in [`rust/gst-nmos-rs/README.md`](https://github.com/NVIDIA/nvnmos/blob/main/rust/gst-nmos-rs/README.md), and the Rust workspace quick start in [`rust/README.md`](https://github.com/NVIDIA/nvnmos/blob/main/rust/README.md).

## Introduction

The [Networked Media Open Specifications (NMOS)](https://www.amwa.tv/nmos-overview) enable the registration, discovery and management of Media Nodes.

The NvNmos library provides APIs to create, destroy and internally manage an [NMOS](https://specs.amwa.tv/nmos) Node for a Media Node application.
It is intended to be integrated with data plane libraries for ST 2110 and/or MXL, such as [NVIDIA Rivermax](https://developer.nvidia.com/networking/rivermax) or the [MXL SDK](https://github.com/dmf-mxl/mxl).

The library can automatically discover and register with an NMOS Registry on the network using the [AMWA IS-04](https://specs.amwa.tv/is-04/) Registration API.

The library provides callbacks for NMOS events such as [AMWA IS-05](https://specs.amwa.tv/is-05/) Connection API requests from an NMOS Controller.
These callbacks can be used to update a running data plane with new transport parameters, for example.

NvNmos currently supports Senders and Receivers for video, audio, and ancillary data flows over RTP/UDP (i.e., SMPTE ST 2110-20, -22, -30, and -40 streams) and over the Media eXchange Layer (MXL).

## C Library Documentation

- [C API guide](doc/user/c-api-guide.md)
- [Building and installation](doc/user/building.md)
- [Configuring transport files](doc/user/transport-files.md)
- [C API reference](https://nvidia.github.io/nvnmos/nvnmos_8h.html)
- [Migration guide](doc/user/migration.md)

## Container Images

Container definitions live under [`docker/`](https://github.com/NVIDIA/nvnmos/tree/main/docker). Build context is always the repository root.

- [`nvnmos` library package image](https://github.com/NVIDIA/nvnmos/blob/main/docker/nvnmos/README.md)
- [`gst-nmos-rs` operator image](https://github.com/NVIDIA/nvnmos/blob/main/docker/gst-nmos-rs/README.md)

## Supported Specifications

The NvNmos library supports the following specifications, using the [Sony nmos-cpp](https://github.com/sony/nmos-cpp) implementation internally:
- [AMWA IS-04 NMOS Discovery and Registration Specification](https://specs.amwa.tv/is-04/) v1.3
- [AMWA IS-05 NMOS Device Connection Management Specification](https://specs.amwa.tv/is-05/) v1.1 and v1.2-dev (for MXL)
- [AMWA IS-09 NMOS System Parameters Specification](https://specs.amwa.tv/is-09/) v1.0
- [AMWA BCP-002-01 Natural Grouping of NMOS Resources](https://specs.amwa.tv/bcp-002-01/) v1.0
- [AMWA BCP-002-02 NMOS Asset Distinguishing Information](https://specs.amwa.tv/bcp-002-02/) v1.0
- [AMWA BCP-004-01 NMOS Receiver Capabilities](https://specs.amwa.tv/bcp-004-01/) v1.0
- [AMWA BCP-006-01 NMOS With JPEG XS](https://specs.amwa.tv/bcp-006-01/) v1.0
- [AMWA BCP-007-03 NMOS With MXL](https://specs.amwa.tv/bcp-007-03/) v1.0-dev
- Session Description Protocol conforming to SMPTE ST 2110-20, -22, -30, -40, and ST 2022-7
- MXL flow definition JSON as consumed by the [MXL SDK](https://github.com/dmf-mxl/mxl)

## Supported Platforms

The library is intended to be portable to different environments.
The following operating systems and compilers have been tested.

* Ubuntu 24.04 with GCC 13
* Windows 10 with Visual Studio 2022

## Contributing

- How to contribute: [CONTRIBUTING.md](https://github.com/NVIDIA/nvnmos/blob/main/CONTRIBUTING.md)
- How to report a vulnerability: [SECURITY.md](https://github.com/NVIDIA/nvnmos/blob/main/SECURITY.md)
