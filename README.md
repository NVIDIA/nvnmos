<!--
 SPDX-FileCopyrightText: Copyright (c) 2022-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 SPDX-License-Identifier: Apache-2.0

 Licensed under the Apache License, Version 2.0 (the "License");
 you may not use this file except in compliance with the License.
 You may obtain a copy of the License at

 http://www.apache.org/licenses/LICENSE-2.0

 Unless required by applicable law or agreed to in writing, software
 distributed under the License is distributed on an "AS IS" BASIS,
 WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 See the License for the specific language governing permissions and
 limitations under the License.
-->

# NVIDIA Networked Media Open Specifications Library

## Introduction

The [Networked Media Open Specifications (NMOS)](https://www.amwa.tv/nmos-overview) enable the registration, discovery and management of Media Nodes.

The NVIDIA NMOS control plane library, NvNmos, provides the APIs to create, destroy and internally manage an [NMOS](https://specs.amwa.tv/nmos) Node for a Media Node application.
It is intended to be integrated with an ST 2110 data plane library such as [NVIDIA Rivermax](https://developer.nvidia.com/networking/rivermax) or [NVIDIA DeepStream](https://developer.nvidia.com/deepstream-sdk).

The library can automatically discover and register with an NMOS Registry on the network using the [AMWA IS-04](https://specs.amwa.tv/is-04/) Registration API.

The library provides callbacks for NMOS events such as [AMWA IS-05](https://specs.amwa.tv/is-05/) Connection API requests from an NMOS Controller.
These callbacks can be used to update running DeepStream pipelines with new transport parameters, for example.

NvNmos currently supports Senders and Receivers for video, audio, and ancillary data flows over RTP (i.e., SMPTE ST 2110-20, -22, -30, and -40 streams) and over the Media eXchange Layer (MXL).

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

## Usage

NvNmos consists of a single shared library (_libnvnmos.so_ on Linux, _nvnmos.dll_ on Windows).
The API is specified by the _nvnmos.h_ header file.

The nvnmos-example application demonstrates use of the library.

### Transports

Each `NvNmosSenderConfig` and `NvNmosReceiverConfig` includes a `transport` field (an `NvNmosTransport` enum) that selects the transport. A zero-initialised configuration defaults to RTP. The `transport_file` field then holds the transport file as the appropriate text:

| `transport`             | `transport_file` format                                              | Reference  |
| ---                     | ---                                                                  | ---        |
| `NVNMOS_TRANSPORT_RTP`  | Session Description Protocol (SDP) per SMPTE ST 2110 / IETF RFCs     | RFC 4566   |
| `NVNMOS_TRANSPORT_MXL`  | MXL flow definition (JSON) as consumed by the MXL SDK                | MXL SDK    |

### NvNmos extensions to the transport file

NvNmos uses a small set of extensions in the transport file to convey configuration that the standard transport file format does not carry. The same conceptual extensions are carried differently in the two transport file formats:

- For RTP (SDP), as custom `a=x-nvnmos-*:<value>` attributes.
- For MXL flow definitions (JSON), as entries in the standard `tags` property keyed by `urn:x-nvnmos:tag:*` URN strings. The tag's value is an array of strings; the first element is used.

| Concept                  | SDP attribute (RTP)        | MXL flow_def tag key (MXL)              | Applies to                                | Description                                                                                                                |
| ---                      | ---                        | ---                                     | ---                                       | ---                                                                                                                        |
| Name                     | `a=x-nvnmos-name:<v>`      | `urn:x-nvnmos:tag:name`                 | Senders and Receivers (required)          | The application's caller-chosen name for the Sender or Receiver, unique within the Node for the given side (Sender or Receiver). A Sender and a Receiver may share the same name. Used in all NvNmos API callbacks (paired with the `NvNmosSide`) |
| Group hint               | `a=x-nvnmos-group-hint:<v>`| standard `urn:x-nmos:tag:grouphint/v1.0`| Senders and Receivers (optional)          | A group hint tag advertised via `urn:x-nmos:tag:grouphint/v1.0` on the NMOS resource                                       |
| Suppress narrow Receiver Caps | `a=x-nvnmos-caps:<v>` (media-level) | `urn:x-nvnmos:tag:caps` | Receivers (optional) | An empty string value selects a fully-flexible Receiver, with format-derived Capabilities omitted. Non-empty strings are reserved for future capability; today any value is treated the same.                                                                                                                                  |
| Interface IP             | `a=x-nvnmos-iface-ip:<v>`  | n/a                                     | Receivers (RTP only)                      | The interface IP address on which the stream is received                                                                   |
| Source port              | `a=x-nvnmos-src-port:<v>`  | n/a                                     | Senders (RTP only)                        | The source port from which the stream is transmitted                                                                       |
| MXL domain id            | n/a                        | `urn:x-nvnmos:tag:mxl-domain-id`        | Senders and Receivers (MXL only, required)| The MXL domain identity (UUID) for the Sender or Receiver; the IS-05 `mxl_domain_id` transport parameter defaults to `"auto"` and is resolved at activation time from this value |

For an MXL flow definition, the tag entries are stored alongside (and follow the same shape as) the standard `urn:x-nmos:tag:grouphint/v1.0` tag, e.g.:

```json
"tags": {
  "urn:x-nmos:tag:grouphint/v1.0": [ "video-sender-1:Video" ],
  "urn:x-nvnmos:tag:name": [ "video-sender-1" ],
  "urn:x-nvnmos:tag:mxl-domain-id": [ "1ac254d9-c9be-475a-93a7-f80b9c1063a8" ]
}
```

NvNmos also publishes the `urn:x-nvnmos:tag:name` tag on the corresponding NMOS resources (visible to controllers via IS-04), so the URN is shared between the two artifacts.

For MXL Senders, the top-level `id` field of the flow definition (if present, a UUID) is used as the MXL flow identity (i.e. the `mxl_flow_id` IS-05 transport parameter); if absent, the generated NMOS Flow id is used in its place. The NMOS Flow id itself is always derived from the `seed` and the name (`urn:x-nvnmos:tag:name` value) and is independent of the flow definition's `id` field. For MXL Receivers, the MXL flow identity is supplied dynamically through IS-05 Connection Management, so the `id` field of the flow definition is ignored.

### Connection activations

When an IS-05 Connection API activation occurs, the library invokes the application's `connection_activated` callback with an `NvNmosSide` (Sender or Receiver), the application's `name`, and an updated `transport_file` reflecting the new active transport parameters. For an RTP Sender or Receiver, the callback receives an SDP file; for an MXL Sender or Receiver, the callback receives an MXL flow definition (JSON) with the new active `mxl_domain_id` and `mxl_flow_id` spliced in (as the `urn:x-nvnmos:tag:mxl-domain-id` tag value and the top-level `id` field, respectively). The application is expected to dispatch on `(side, name)` to identify the Sender or Receiver and react accordingly (for example, by reconfiguring its data plane). Conversely, if an activation (or deactivation) has already occurred in the application's data plane by some other means, outside the NMOS API, the application calls `nmos_connection_activate` (also passing the `side`) to update the IS-04 and IS-05 model to reflect it. The library does not initiate any activation on the application's behalf.

## Docker-Based Build

A _Dockerfile_ is provided which builds, packages and tests the library and application from source.

```sh
docker build -t nvnmos .
```

The package can then be copied directly to the host system.

```sh
docker create --name nvnmos-test nvnmos
docker cp nvnmos-test:/nvnmos-ubuntu-24.04.tar.gz .
docker rm nvnmos-test
```

The container also has an _entrypoint.sh_ which demonstrates how to install the run-time requirements and run the application.

```sh
docker run -it nvnmos /bin/bash
```

### Dockerfile Build Arguments

The following build arguments are available.

| Argument | Explanation |
| --- | --- |
| BASE_IMAGE | Controls the base container image and therefore the compatibility of the created package. Default is `ubuntu:24.04`. | 
| PACKAGE_SUFFIX | Controls the package filename, which will be _nvnmos\<suffix\>.tar.gz_. Default is based on the base image, e.g. `-ubuntu-24.04`. |
| USE_CONAN_LOCK | Controls whether the _conan.lock_ file is used to ensure reproducible dependencies, even when new versions are available. Default is `1` (on). |

If this isn't sufficient for your purposes, read on for manual build instructions.

## Pre-Build Requirements

### Python

Having Python 3 isn't an absolute requirement but it makes the subsequent steps to install the dependencies easier.

**Linux**

Use the system package manager to install Python 3 and the `venv` module (on Debian and Ubuntu the package is `python3-venv`). A system-wide `python3-pip` install is optional if you follow the recommended virtual environment below: the virtual environment gets its own `pip`, which avoids touching the distribution's managed environment (PEP 668).

> 💬 **Note:**
> The `-y` option allows `apt install` to run non-interactively.

```sh
sudo apt install -y python3 python3-venv
```

**Windows**

Download the Python 3 installer from [python.org](https://www.python.org/downloads/windows/) and run it manually, or use the following PowerShell commands.
Python 3.14.4 (64-bit Windows, latest stable release at the time) has been tested.

> 💬 **Note:**
> The `` ` `` is the PowerShell line continuation character.

```PowerShell
Invoke-WebRequest `
  https://www.python.org/ftp/python/3.14.4/python-3.14.4-amd64.exe `
  -OutFile python-3.14.4-amd64.exe

./python-3.14.4-amd64.exe /quiet PrependPath=1 Include_tcltk=0 Include_test=0
```

Confirm this Python is available on the `PATH`. Try closing and reopening the terminal if not.

```PowerShell
python --version
```

### Python Virtual Environment

For manual builds, a **virtual environment** in the repository root (for example, `.venv`) is recommended. It isolates the Python environment used for Conan and other `pip`-installed build tools from the system interpreter, pins compatible tool versions on `PATH`, and on Linux avoids PEP 668 "externally managed" errors when using the system Python.

**Linux**

```sh
python3 -m venv .venv
source .venv/bin/activate
pip install --upgrade pip
```

**Windows**

From PowerShell, use the following commands.

```PowerShell
python -m venv .venv
.\.venv\Scripts\Activate.ps1
# If PowerShell reports that "running scripts is disabled on this system"
# you can adjust the execution policy as follows and try again.
# Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser
python -m pip install --upgrade pip
```

Or use Command Prompt instead.

```bat
python -m venv .venv
.\.venv\Scripts\activate.bat
python -m pip install --upgrade pip
```

To exit the virtual environment later, run `deactivate` (same on Linux and Windows).

### CMake

The project requires CMake 3.17 or higher; dependencies may require a higher version.

There are x86_64 and arm64 packages for CMake 3.30.0 on the [Python Package Index (PyPI)](https://pypi.org/) which have been tested.

**Linux**

```sh
pip install cmake~=3.17
```

**Windows**

```PowerShell
pip install cmake~=3.17
```

### Conan

Using [Conan](https://conan.io/) simplifies fetching, building, and installing the required C++ dependencies from [Conan Center](https://conan.io/center/).

The project requires Conan 2.2 or higher; dependencies may require a higher version.
Conan 2.28.0 (latest release at the time) has been tested.

**Linux**

```sh
pip install conan~=2.2 --upgrade
conan profile detect
```

The detected profile is displayed, along with some `WARN` messages.
For example, the following profile has been tested.

```ini
[settings]
arch=x86_64
build_type=Release
compiler=gcc
compiler.cppstd=gnu17
compiler.libcxx=libstdc++11
compiler.version=13
os=Linux
```

**Windows**

```PowerShell
pip install conan~=2.2 --upgrade
conan profile detect
```

The detected profile is displayed, along with some `WARN` messages.
For example, the following profile has been tested.

```ini
[settings]
arch=x86_64
build_type=Release
compiler=msvc
compiler.cppstd=14
compiler.runtime=dynamic
compiler.version=193
os=Windows
```

## Building the NvNmos Library

**Linux**

Prepare a _build_ directory adjacent to the _src_ directory.

```sh
mkdir build
```

To install the dependencies using Conan, use the following command.

> 💬 **Note:**
> Replace `<Release-or-Debug>` with the necessary value.
> Passing `--lockfile=src/conan.lock` pins dependency versions per _src/conan.lock_, matching the default _Dockerfile_ and GitHub Actions behaviour.
> Do not pass `-g CMakeToolchain` or `-g CMakeDeps`; `src/conanfile.py` already generates them, and Conan fails if they are duplicated (for example when copying older command lines).

```sh
conan install src \
  --settings:all build_type=<Release-or-Debug> \
  --build=missing \
  --output-folder=src/conan \
  --lockfile=src/conan.lock
```

Use the following CMake command to configure the build.

> 💬 **Note:**
> Replace `<Release-or-Debug>` with the necessary value.
> The `CMAKE_TOOLCHAIN_FILE` path is resolved relative to the top-level _src_ directory passed as the last argument.

```sh
cmake -B build \
  -DCMAKE_TOOLCHAIN_FILE=conan/conan_toolchain.cmake \
  -DCMAKE_BUILD_TYPE=<Release-or-Debug> \
  src
```

Build the library and example application.

```sh
cmake --build build --parallel
```

**Windows**

Prepare a _build_ directory adjacent to the _src_ directory.

```PowerShell
mkdir build
```

To install the dependencies using Conan, use the following command.

> 💬 **Note:**
> The `` ` `` is the PowerShell line continuation character. In the Windows command prompt, use `^` instead.
> Replace `<Release-or-Debug>` with the necessary value.
> Passing `--lockfile=src/conan.lock` pins dependency versions per _src/conan.lock_, matching the default _Dockerfile_ and GitHub Actions behaviour.
> Do not pass `-g CMakeToolchain` or `-g CMakeDeps`; `src/conanfile.py` already generates them, and Conan fails if they are duplicated (for example when copying older command lines).

```PowerShell
conan install src `
  --settings:all build_type=<Release-or-Debug> `
  --build=missing `
  --output-folder=src/conan `
  --lockfile=src/conan.lock
```

Repeat the command for both `Debug` and `Release` if required.

Use the following CMake command to configure the build.

```PowerShell
cmake -B build `
  -G "Visual Studio 17 2022" `
  -DCMAKE_TOOLCHAIN_FILE="conan/conan_toolchain.cmake" `
  -DCMAKE_CONFIGURATION_TYPES="Debug;Release" `
  src
```

Build the library and application with the following command or manually using the generated Visual Studio solution.

> 💬 **Note:**
> Replace `<Release-or-Debug>` with the necessary value.

```sh
cmake --build build --config <Release-or-Debug> --parallel
```

### Local `nmos-cpp` checkout (Conan for dependencies only)

To build against a clone of [nmos-cpp](https://github.com/sony/nmos-cpp) while using Conan to resolve Boost, cpprestsdk, Avahi or mDNSResponder, and other dependencies:

1. Clone it into a directory `nmos-cpp` in the same parent directory as `nvnmos`.

2. Install Conan dependencies with the consumer option `nmos_cpp_from_source=True` and **without** the default lockfile, which is for the packaged `nmos-cpp` graph:

   **Linux**

   ```sh
   conan install src \
     --settings:all build_type=<Release-or-Debug> \
     --build=missing \
     --output-folder=src/conan \
     --lockfile="" \
     -o "&:nmos_cpp_from_source=True"
   ```

   **Windows**

   ```PowerShell
   conan install src `
     --settings:all build_type=<Release-or-Debug> `
     --build=missing `
     --output-folder=src/conan `
     --lockfile="" `
     -o "&:nmos_cpp_from_source=True"
   ```

   > 💬 **Note:**
   > With `--lockfile=""`, versions of dependencies such as Boost and OpenSSL are not pinned. You may choose to create a lockfile for `nmos_cpp_from_source=True` installs (`conan lock create` per profile, then `conan lock merge`), then pass `--lockfile=<path>` on `conan install`.

3. Configure and build as in **Building the NvNmos Library** above, adding `-DUSE_ADD_SUBDIRECTORY=ON` to the `cmake` configure step. (If you followed step 1, you do not need `-DNMOS_CPP_DIRECTORY=<path-to-nmos-cpp/Development>`.)

## Run-Time Requirements

**Linux**

Install and run the Avahi Daemon.

```sh
apt update
apt install -y dbus avahi-daemon

/etc/init.d/dbus start
/etc/init.d/avahi-daemon start
```

> 💬 **Note:**
> Since Ubuntu 24.04, an init script is not provided for the Avahi daemon; run `avahi-daemon --daemonize` instead.

**Windows**

Install and start the Bonjour Service.

See [Download Bonjour Print Services for Windows v2.0.2](https://support.apple.com/kb/DL999).

## Running the Example Application

### Starting the Example Application

Run the nvnmos-example app specifying host name, port, IP address, and optionally a log level.

For example:
```sh
nvnmos-example nmos-api.local 8080 192.0.2.0
```

The host name can be a .local name, in which case the Node will attempt to discover a Registry being advertised via multicast DNS-SD (mDNS).
When a fully-qualified domain name is specified, e.g. "api.example.com", the NMOS Node will instead use unicast DNS-SD discovery in the relevant domain, e.g. "example.com".

The port is used to serve the HTTP APIs.

The IP address identifies the interface to be used for the mock Senders and Receivers created by the nvnmos-example application.

The log level ranges between -40 (most verbose) and 40 (least verbose), as per the NvNmos API.
Values greater than zero are warnings and errors. Values less than zero are debugging or trace messages.

The nvnmos-example app runs through the following steps which are output independent of the log level:
```
Creating NvNmos server...
Removing some senders and receivers...
Adding back some senders and receivers...
Activating senders and receivers...
Deactivating senders and receivers...
Destroying NvNmos server...
Finished
```

After each step, the app prompts before moving on to the next step:
```
Continue ([y]/n)?
```

If the app runs successfully to completion, the process exits with code 0.
If any step fails, or the user responds negatively to a prompt, the process exits immediately with code 1.

The example application also creates two MXL Senders and two MXL Receivers (uncompressed `video/v210` and `audio/float32`) and exercises the same add/remove/activate/deactivate cycle for them, alongside the RTP Senders and Receivers.

### Accessing the NMOS APIs

While the app is running, the IS-04 Node API, the IS-05 Connection API, etc., are available for an NMOS Controller to use.
The HTTP APIs can be accessed at:
```
http://<host-address>:<port>/
```

## Troubleshooting

### Address already in use

When running multiple NMOS Node instances, each process must be configured to use different ports, i.e., with a unique `port` value.
When the port is already in use, at start-up, the application may show a message like the following:

```
asio listen error: system:98 (Address already in use)
```

### Apple Bonjour compatibility warnings

When using Avahi for DNS-SD, shortly after start-up the following lines may be displayed in the log.
They do not indicate a problem and can be ignored.

```
*** WARNING *** The program 'nvnmos-example' uses the Apple Bonjour compatibility layer of Avahi.
*** WARNING *** Please fix your application to use the native API of Avahi!
*** WARNING *** For more information see <http://0pointer.de/blog/projects/avahi-compat.html>
```

### DNSServiceRegister and DNSServiceBrowse errors

The application may show messages like the following shortly after start-up:

```
DNSServiceRegister reported error: -65537 while registering advertisement for: nmos-cpp_node_192-168-1-194:12345._nmos-node._tcp
DNSServiceBrowse reported error: -65537
```

In this case, the NMOS Node will not be able to discover or register with the NMOS Registry.

One reason for these errors is that the DNS-SD daemon/service is not running.

**Linux**

When using Avahi, check that the `avahi-daemon` is running.

**Windows**

When using mDNSResponder/Bonjour, check that the Bonjour Service is running.
