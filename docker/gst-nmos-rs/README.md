<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# gst-nmos-rs Container Image

Operator runtime image: `nvnmosd`, `gst-nmos-rs` (`libgstnmos.so`), MXL (`libmxl` + `libgstmxl.so`), and GStreamer plugins for **linux/amd64** (Ubuntu 24.04, non-root UID/GID **10001**).

Use this image to run `gst-launch-1.0` pipelines with `nmossrc` / `nmossink`:

| `transport` | Inner elements |
|-------------|----------------|
| `mxl` | `mxlsrc` / `mxlsink` (`libgstmxl.so`) |
| `udp` | gst-plugins-good `udpsrc` / `udpsink` + `rtp*pay` / `rtp*depay` |
| `udp2` | gst-plugins-rs `udpsrc2` + `rtp*pay2` / `rtp*depay2` (falls back to gst-plugins-good per element when a v2 factory is missing) |

Build from the **repository root**:

```bash
docker build -f docker/gst-nmos-rs/Dockerfile -t nvnmos-gst .
```

The nvnmos tree is taken from the build context (`COPY src/`, `COPY rust/` via the C++ stage). The mxl and gst-plugins-rs repos are cloned at build time. MXL is built in one stage: `vcpkg` bootstrap, then `cargo build -p gst-mxl-rs`. Runtime finds `libmxl.so` via `LD_LIBRARY_PATH`. First build is slow (vcpkg + Conan + gst-plugins-rs).

## Build Arguments

| Argument | Default | Explanation |
|----------|---------|-------------|
| `BASE_IMAGE` | `ubuntu:24.04` | Base image for all stages; controls runtime compatibility. |
| `CONAN_LOCKFILE` | `src/conan.lock` | Input lockfile for `conan install`. Pass an empty value to resolve the latest compatible graph instead. |
| `RUST_TOOLCHAIN` | `1.92` | Rust toolchain for all Rust stages in this image. Matches [`rust/rust-toolchain.toml`](../../rust/rust-toolchain.toml); gst-plugins-rs MSRV is **1.92**. Workspace MSRV is **1.85** in [`rust/Cargo.toml`](../../rust/Cargo.toml). |
| `MXL_REPO` | `https://github.com/dmf-mxl/mxl.git` | MXL source repository (`libmxl`, `gst-mxl-rs`). |
| `MXL_REF` | `81738a15adb55119a6855343bc1053a4389bf6df` | Pinned MXL commit (`81738a1`, tip of `release/v1.1` at time of writing). Use a full 40-character SHA or a branch/tag name. |
| `GST_PLUGINS_RS_REPO` | `https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs.git` | gst-plugins-rs source for `transport=udp2`. |
| `GST_PLUGINS_RS_REF` | `8d5c60f0a67d3aa8120bf940b46fc3c18209661c` | Pinned gst-plugins-rs commit on `main` (`udpsrc2` is main-only; this commit also carries the `st2038combiner` skew and `rtpsmpte291depay` multi-ANC fixes). Builds `gst-plugin-udp` + `gst-plugin-rtp`. Use a full 40-character SHA or a branch/tag name. |
| `NVNMOS_UID` | `10001` | Fixed runtime user UID (`nvnmos`). |
| `NVNMOS_GID` | `10001` | Fixed runtime group GID (`nvnmos`). |
| `EXTRA_APT_PACKAGES` | *(empty)* | Optional space-separated apt package names added in the final image stage (e.g. `gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly`). Installed to the default GStreamer plugin path. |

Example with extra plugins:

```bash
docker build -f docker/gst-nmos-rs/Dockerfile -t nvnmos-gst \
  --build-arg EXTRA_APT_PACKAGES="gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly" .
```

## Run

The entrypoint starts dbus, avahi, and `nvnmosd`, publishes **`${HOSTNAME}.local`** via mDNS by default, then runs your command. Pass `gst-launch-1.0` and NMOS/MXL properties as container args. Inject `node-seed` in the pipeline command.

`domain_def.json` must already exist on the mounted MXL domain path; the entrypoint does not create domains or replicate across hosts.

```bash
docker run --rm \
  -v /path/to/mxl-domain:/mxl/domain:rw \
  nvnmos-gst \
  gst-launch-1.0 -e \
    videotestsrc is-live=true ! \
    video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001 ! \
    queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 ! \
    nmossink transport=mxl daemon-uri=unix:/tmp/nvnmosd.sock \
      node-seed=d8e9f0a1-2b3c-4d5e-8f9a-0b1c2d3e4f5a \
      sender-name=video1 \
      mxl-domain-id=1ac254d9-c9be-475a-93a7-f80b9c1063a8 \
      mxl-domain-path=/mxl/domain \
      mxl-flow-id=5fbec3b1-1b0f-417d-9059-8b94a47197ed \
      auto-activate=true
```

Set `NVNMOS_PUBLISH_MDNS=0` to disable mDNS publish (e.g. in-cluster only). For `transport=mxl`, mount your domain volume and set `mxl-domain-path` in the pipeline to the same path inside the container (e.g. `/mxl/domain` above).

## Kubernetes

One container per pod; pass the pipeline as `args`. Mount a volume and set `mxl-domain-path` to that mount point. Ensure the mount is writable by UID **10001** (`securityContext.runAsUser: 10001`, `fsGroup: 10001`).

## Environment

| Variable | Default | Purpose |
|----------|---------|---------|
| `NVNMOSD_UDS` | `/tmp/nvnmosd.sock` | `nvnmosd` Unix socket; must match `daemon-uri=unix:…` in the pipeline |
| `NVNMOS_PUBLISH_MDNS` | `1` | Publish `${HOSTNAME}.local`; set `0` to disable |

The entrypoint sets `LD_LIBRARY_PATH` and `GST_PLUGIN_PATH` for the fixed install under `/opt/nvnmos/plugins` (`libgstnmos.so`, `libgstmxl.so`, `libgstrsudp.so`, `libgstrsrtp.so`). System gst-plugins-good/-base remain on the default GStreamer search path for `transport=udp`.

## Shared Infrastructure

`entrypoint-setup.sh` is copied from [`docker/nvnmos/`](../nvnmos/entrypoint-setup.sh) (user-mode dbus + avahi, shared by both container images).
