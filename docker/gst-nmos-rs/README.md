<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# gst-nmos-rs Container Image

Operator runtime image: `nvnmosd`, `gst-nmos-rs` (`libgstnmos.so`), MXL (`libmxl` + `libgstmxl.so`), and GStreamer plugins for **linux/amd64** (default base `ubuntu:24.04`, non-root UID/GID **10001**).

Use this image to run `gst-launch-1.0` pipelines with `nmossrc` / `nmossink`:

| `transport` | Inner elements |
|-------------|----------------|
| `mxl` | `mxlsrc` / `mxlsink` (`libgstmxl.so`) |
| `udp` | gst-plugins-good `udpsrc` / `udpsink` + `rtp*pay` / `rtp*depay` |
| `udp2` | gst-plugins-rs `udpsrc2` + `rtp*pay2` / `rtp*depay2` (falls back to gst-plugins-good per element when a v2 factory is missing) |
| `nvdsudp` | DeepStream `nvdsudpsrc` / `nvdsudpsink` — **not** in the default Ubuntu image; see [DeepStream base image](#deepstream-base-image) and [pipeline examples](../../rust/gst-nmos-rs/pipeline-examples.md#deepstream-rivermax) |

Build from the **repository root**:

```bash
docker build -f docker/gst-nmos-rs/Dockerfile -t nvnmos-gst .
```

The nvnmos tree is taken from the build context (`COPY src/`, `COPY rust/` via the C++ stage). The mxl and gst-plugins-rs repos are cloned at build time. MXL is built in one stage: `vcpkg` bootstrap, then `cargo build -p gst-mxl-rs`. Runtime finds `libmxl.so` via `LD_LIBRARY_PATH`. First build is slow (vcpkg + Conan + gst-plugins-rs).

## Build Arguments

| Argument | Default | Explanation |
|----------|---------|-------------|
| `BASE_IMAGE` | `ubuntu:24.04` | Base for **every** stage, including the final runtime. Override with a DeepStream image to be able to use DeepStream plugins (see below). |
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

## DeepStream Base Image

The default `ubuntu:24.04` runtime has the plugins for MXL and OSS RTP/UDP transports only. To get DeepStream's GStreamer plugins in the same image as `nvnmosd` / `nmossrc` / `nmossink`, build with a DeepStream base via `BASE_IMAGE` (all stages use that base, so the final image has the full DeepStream stack and is correspondingly large):

```bash
docker build -f docker/gst-nmos-rs/Dockerfile -t nvnmos-gst:ds \
  --build-arg BASE_IMAGE=nvcr.io/nvidia/deepstream:9.1-triton-multiarch .
```

NGC pull may require `docker login nvcr.io`. Pin the DeepStream tag you need. This Dockerfile has been smoke-built against `9.1-triton-multiarch`.

**Runtime (GPU):** use the [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/index.html) (e.g. `docker run --gpus all …`). Many DeepStream plugins need `libcuda` from the GPU runtime and will not load in a plain CPU-only `docker run`.

CI builds and smoke-tests only the default Ubuntu-based `docker/nvnmos` package image, not a DeepStream or Rivermax `gst-nmos-rs` variant.

### Rivermax for `transport=nvdsudp`

DeepStream plugins alone are not enough for `transport=nvdsudp`. You also need the [Rivermax SDK](https://developer.nvidia.com/networking/rivermax) libraries in the image, a Rivermax license at runtime, a ConnectX-5 (or newer) NIC, and host MOFED drivers. Pipeline property details are in the [DeepStream Rivermax pipeline examples](../../rust/gst-nmos-rs/pipeline-examples.md#deepstream-rivermax).

This repository does not ship Rivermax or install it in `docker/gst-nmos-rs/Dockerfile`.

**License (host):** request a developer license from [Rivermax Getting Started](https://developer.nvidia.com/networking/rivermax-getting-started) and store it on the host (Media Gateway convention: `/opt/mellanox/rivermax/rivermax.lic`).

**Install into a derived image** (after `nvnmos-gst:ds` exists). Obtain `rivermax_ubuntu2404_<ver>.tar.gz` from the Rivermax SDK download. Pin **1.70.32** for DeepStream 9.1: newer packages (e.g. 1.90.18) need `ibverbs-providers (>= 60)`, while the NGC DeepStream 9.1 base ships `ibverbs-providers` 59.1 unless you install matching MOFED/`ibverbs` first.

```dockerfile
# Example only — not part of this repository's Dockerfiles.
FROM nvnmos-gst:ds
USER root
ARG RMAX_VER=1.70.32
COPY rivermax_ubuntu2404_${RMAX_VER}.tar.gz /tmp/
RUN tar -xzf /tmp/rivermax_ubuntu2404_${RMAX_VER}.tar.gz -C /tmp \
 && dpkg -i /tmp/${RMAX_VER}/Ubuntu.24.04/deb-dist/x86_64/*.deb \
 && rm -rf /tmp/${RMAX_VER} /tmp/rivermax_ubuntu2404_${RMAX_VER}.tar.gz \
 && printf '%s\n' \
      /opt/nvnmos/lib/mxl \
      > /etc/ld.so.conf.d/mxl.conf \
 && ldconfig \
 && setcap 'CAP_NET_RAW=ep CAP_SYS_NICE=ep CAP_IPC_LOCK=ep CAP_DAC_READ_SEARCH=ep' /usr/bin/gst-launch-1.0
USER nvnmos
```

`setcap` on `gst-launch-1.0` makes the dynamic linker ignore `LD_LIBRARY_PATH`, so register `/opt/nvnmos/lib/mxl` via `ldconfig` before applying capabilities. MXL v1.1 embeds former `internal/` objects in `libmxl.so`, so that path alone is enough.

For `aarch64`, install from `deb-dist/aarch64/` instead of `x86_64/`.

```bash
docker build -t nvnmos-gst:ds-rivermax -f Dockerfile.rivermax .
```

**Run** with the license mounted at the path Rivermax expects, GPU and host network access, and the required capabilities:

```bash
docker run --rm --gpus all --net=host \
  --cap-add=NET_RAW --cap-add=SYS_NICE --cap-add=IPC_LOCK --cap-add=DAC_READ_SEARCH \
  -v /opt/mellanox/rivermax/rivermax.lic:/opt/mellanox/rivermax/rivermax.lic:ro \
  nvnmos-gst:ds-rivermax \
  gst-launch-1.0 -e … nmossink transport=nvdsudp …
```

Host OFED / NIC setup must follow the Rivermax documentation; it is outside the scope of this image build.

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
