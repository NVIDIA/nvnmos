<!--
 SPDX-FileCopyrightText: Copyright (c) 2022-2024 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
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

NvNmos currently supports Senders and Receivers for uncompressed Video and Audio, i.e., SMPTE ST 2110-20 and SMPTE ST 2110-30 streams.

The NvNmos library supports the following specifications, using the [Sony nmos-cpp](https://github.com/sony/nmos-cpp) implementation internally:
- [AMWA IS-04 NMOS Discovery and Registration Specification](https://specs.amwa.tv/is-04/) v1.3
- [AMWA IS-05 NMOS Device Connection Management Specification](https://specs.amwa.tv/is-05/) v1.1
- [AMWA IS-09 NMOS System Parameters Specification](https://specs.amwa.tv/is-09/) v1.0
- [AMWA BCP-002-01 Natural Grouping of NMOS Resources](https://specs.amwa.tv/bcp-002-01/) v1.0
- [AMWA BCP-002-02 NMOS Asset Distinguishing Information](https://specs.amwa.tv/bcp-002-02/) v1.0
- [AMWA BCP-004-01 NMOS Receiver Capabilities](https://specs.amwa.tv/bcp-004-01/) v1.0
- Session Description Protocol conforming to SMPTE ST 2110-20 and -30

## Supported Platforms

The library is intended to be portable to different environments.
The following operating systems and compilers have been tested.

* Ubuntu 22.04 with GCC 11
* Windows 10 with Visual Studio 2022

## Usage

NvNmos consists of a single shared library (_libnvnmos.so_ on Linux, _nvnmos.dll_ on Windows).
The API is specified by the _nvnmos.h_ header file.

The nvnmos-example application demonstrates use of the library.

## Docker-Based Build

A _Dockerfile_ is provided which builds, packages and tests the library and application from source.

```sh
docker build -t nvnmos .
```

The package can then be copied directly to the host system.

```sh
docker create --name nvnmos-test nvnmos
docker cp nvnmos-test:/nvnmos-ubuntu-22.04.tar.gz .
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
| BASE_IMAGE | Controls the base container image and therefore the compatibility of the created package. Default is `ubuntu:22.04`. | 
| PACKAGE_SUFFIX | Controls the package filename, which will be _nvnmos\<suffix\>.tar.gz_. Default is based on the base image, e.g. `-ubuntu-22.04`. |
| USE_CONAN_LOCK | Controls whether the _conan.lock_ file is used to ensure reproducible dependencies, even when new versions are available. Default is `1` (on). |

If this isn't sufficient for your purposes, read on for manual build instructions.

## Pre-Build Requirements

### Python Package Installer

Having Python 3 isn't an absolute requirement but it makes the subsequent steps to install the dependencies easier.

**Linux**

Use the system package manager to install Python 3 and the [Package Installer for Python (pip)](https://pypi.org/project/pip/).

> 💬 **Note:**
> The `-y` option allows `apt install` to run non-interactively.

```sh
sudo apt install -y python3-pip

pip3 install --upgrade pip
```

**Windows**

Download the Python 3 installer and run it manually or use the following PowerShell script.

> 💬 **Note:**
> The `` ` `` is the PowerShell line continuation character.

```PowerShell
Invoke-WebRequest `
  https://www.python.org/ftp/python/3.10.9/python-3.10.9-amd64.exe `
  -OutFile python-3.10.9-amd64.exe

./python-3.10.9-amd64.exe /quiet PrependPath=1 Include_tcltk=0 Include_test=0

pip3 install --upgrade pip
```

### CMake

The project requires CMake 3.17 or higher. (The system-provided CMake 3.10 on the Jetson is not sufficient.)

There are x86_64 and arm64 packages for CMake 3.30.0 on the [Python Package Index (PyPI)](https://pypi.org/) which have been tested.

**Linux**

```sh
pip3 install cmake~=3.17
```

> 💬 **Note:**
> Using `sudo` would overwrite an existing CMake package in _/usr/local/bin_.
> Avoiding this is recommended; without `sudo` the installer puts binaries in a per-user directory, _/home/\<userid\>/.local/bin_.
> On the Jetson, this isn't in the user's `PATH` by default.
> To add it for the current session, use the following command.
> Replace `<userid>` with the necessary value.
>
> ```sh
> export PATH=/home/<userid>/.local/bin:${PATH}
> ```

**Windows**

```sh
pip3 install cmake~=3.17
```

### Conan

Using [Conan](https://conan.io/) simplifies fetching, building, and installing the required C++ dependencies from [Conan Center](https://conan.io/center/).

The project requires Conan 2.2 or higher. Conan 2.5.0 has been tested.

**Linux**

```sh
pip3 install conan~=2.2 --upgrade
conan profile detect
```

> 💬 **Note:**
> As per the CMake instructions, on the Jetson a warning is reported that the per-user install directory _/home/\<userid\>/.local/bin_ is not on the `PATH` if it hasn't yet been added.

> On some platforms with Python 2 and Python 3 both installed this may need to be `pip3 install --upgrade conan~=2.2`

> Conan 2.2 or higher is required; dependencies may require a higher version; version 2.5.0 (latest release at the time) has been tested

**Windows**

```sh
pip3 install conan~=2.2 --upgrade
conan profile detect
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

```sh
conan install src \
  -g CMakeToolchain \
  --settings:all build_type=<Release-or-Debug> \
  --build=missing \
  --output-folder=src/conan
```

Use the following CMake command to configure the build.

> 💬 **Note:**
> Replace `<Release-or-Debug>` with the necessary value.

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

```sh
mkdir build
```

To install the dependencies using Conan, use the following command.

> 💬 **Note:**
> The `` ` `` is the PowerShell line continuation character. In the Windows command prompt, use `^` instead.
> Replace `<Release-or-Debug>` with the necessary value.

```PowerShell
conan install src `
  -g CMakeToolchain `
  --settings:all build_type=<Release-or-Debug> `
  --build=missing `
  --output-folder=src/conan
```

Repeat the command for both `Debug` and `Release` if required.

Use the following CMake command to configure the build.

```PowerShell
cmake -B build `
  -G "Visual Studio 17 2022" `
  -DCMAKE_TOOLCHAIN_FILE=conan/conan_toolchain.cmake `
  -DCMAKE_CONFIGURATION_TYPES="Debug;Release" `
  src
```

Build the library and application with the following command or manually using the generated Visual Studio solution.

> 💬 **Note:**
> Replace `<Release-or-Debug>` with the necessary value.

```sh
cmake --build build --config <Release-or-Debug> --parallel
```

## Run-Time Requirements

*Linux*

Install and run the Avahi Daemon.

```sh
apt update
apt install -y dbus avahi-daemon

/etc/init.d/dbus start
/etc/init.d/avahi-daemon start
```

> 💬 **Note:**
> Since Ubuntu 24.04, an init script is not provided for the Avahi daemon; run `avahi-daemon --daemonize` instead.

*Windows*

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
