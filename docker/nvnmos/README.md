<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# nvnmos Library Package Image

Builds and packages the C++ library and Rust workspace from source; the entrypoint smoke-tests _nvnmos-example_, _nvnmosd_, and _nvnmosd-example_ on start. Used by CI.

Build from the **repository root**:

```bash
docker build -f docker/nvnmos/Dockerfile -t nvnmos .
```

With default build arguments the image produces a tarball named `nvnmos-ubuntu-24.04.tar.gz` (from `BASE_IMAGE=ubuntu:24.04` and `CONAN_LOCKFILE=src/conan.lock`). See the [top-level README](../../README.md#container-images) for copying that package to the host.

## Build Arguments

| Argument | Default | Explanation |
|----------|---------|-------------|
| `BASE_IMAGE` | `ubuntu:24.04` | Base image for all stages; controls the compatibility of the created package and tarball name. |
| `PACKAGE_SUFFIX` | _(derived from `BASE_IMAGE`)_ | Package directory and tarball suffix. Default is `-ubuntu-24.04` for the default base image, yielding `nvnmos-ubuntu-24.04.tar.gz`. |
| `CONAN_LOCKFILE` | `src/conan.lock` | Input lockfile for `conan install`. Pass an empty value, e.g. `--build-arg CONAN_LOCKFILE=`, to resolve the latest compatible graph instead. |
| `RUST_TOOLCHAIN` | `1.92` | Rust toolchain for the workspace build. Matches [`rust/rust-toolchain.toml`](../../rust/rust-toolchain.toml); workspace MSRV is **1.85** in [`rust/Cargo.toml`](../../rust/Cargo.toml). |

The image installs the Conan Center `nmos-cpp` dependency graph via `conan install` (using `CONAN_LOCKFILE` when set), writes the resolved `conan.lock` into the package tarball, then builds the Rust workspace (`nvnmosd`, `gst-nmos-rs`, …) against the built `libnvnmos.so`. Conan (`~=2.2`) and CMake (`~=3.17`) versions match the manual build instructions in the top-level README and CI.

## Run

The runtime image includes _dbus_, _avahi-daemon_, _avahi-utils_, _libavahi-compat-libdnssd1_ (Bonjour/`DNSService*` API for `libnvnmos`), and _libnss-mdns_ (`.local` hostname resolution). [`entrypoint.sh`](entrypoint.sh) and [`entrypoint-setup.sh`](entrypoint-setup.sh) start D-Bus and Avahi without systemd or chroot, run the C _nvnmos-example_ and the Rust _nvnmosd_ / _nvnmosd-example_ pair, then exec your command.

```bash
docker run -it nvnmos /bin/bash
```

## Shared Infrastructure

[`entrypoint-setup.sh`](entrypoint-setup.sh) is also copied into the [`gst-nmos-rs`](../gst-nmos-rs/) operator image (user-mode dbus + avahi, shared by both container images).
