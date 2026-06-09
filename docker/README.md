<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Container Images

Build context for every image in this directory is the **repository root** (`.`).

| Directory | Image | Purpose |
|-----------|-------|---------|
| [`nvnmos/`](nvnmos/) | `nvnmos` | Build `libnvnmos` + Rust workspace, produce `nvnmos-<platform>.tar.gz`, smoke-test `nvnmos-example` / `nvnmosd` / `nvnmosd-example` on start. Used by CI. |
| [`gst-nmos-rs/`](gst-nmos-rs/) | `nvnmos-gst` | Operator runtime: `nvnmosd` + `gst-nmos-rs` + plugins for `transport=mxl` and `transport=udp`/`udp2`. |

Shared [`nvnmos/entrypoint-setup.sh`](nvnmos/entrypoint-setup.sh) starts user-mode D-Bus and Avahi without systemd or `CAP_SYS_CHROOT`.

**Toolchain pins (defaults)** — same in both images as CI and manual builds:

- **Rust 1.92** — matches [`rust/rust-toolchain.toml`](../rust/rust-toolchain.toml) (workspace MSRV **1.85** in [`rust/Cargo.toml`](../rust/Cargo.toml))
- **MXL** `81738a15adb55119a6855343bc1053a4389bf6df` (`81738a1`)
- **Conan lockfile** `src/conan.lock`
- **Conan ~=2.2** / **CMake ~=3.17**

```bash
# Library Package Image (CI)
docker build -f docker/nvnmos/Dockerfile -t nvnmos .

# gst-nmos-rs Operator Image
docker build -f docker/gst-nmos-rs/Dockerfile -t nvnmos-gst .
```

See [`nvnmos/README.md`](nvnmos/README.md) and [`gst-nmos-rs/README.md`](gst-nmos-rs/README.md) for build arguments. The [top-level README](../README.md#container-images) documents extracting the default `nvnmos-ubuntu-24.04.tar.gz` package.
