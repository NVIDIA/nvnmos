# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# gst-nmos-rs operator runtime: nvnmosd + libgstnmos +
#   transport=mxl — gst-mxl-rs (mxlsrc/mxlsink)
#   transport=udp — gst-plugins-good (udpsrc/udpsink + rtp*pay/depay)
#   transport=udp2 — gst-plugins-rs (udpsrc2 + rtp*pay2/rtp*depay2; falls back to gst-plugins-good)
#
# Build from the repository root:
#   docker build -f docker/gst-nmos-rs/Dockerfile -t nvnmos-gst .
#
# Optional extra distro packages (space-separated apt names), e.g. gst-plugins-bad/-ugly plugins:
#   docker build -f docker/gst-nmos-rs/Dockerfile -t nvnmos-gst \
#     --build-arg EXTRA_APT_PACKAGES="gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly" .
#
# Run:
#
#   docker run --rm -v /path/to/mxl-domain:/mxl/domain:rw nvnmos-gst \
#     gst-launch-1.0 -e videotestsrc ! ... ! nmossink transport=mxl \
#       daemon-uri=unix:/tmp/nvnmosd.sock mxl-domain-path=/mxl/domain node-seed=...

# Base image
ARG BASE_IMAGE=ubuntu:24.04

# Build/install arguments
ARG DEBIAN_FRONTEND=noninteractive
ARG PIP_BREAK_SYSTEM_PACKAGES=1
ARG CONAN_LOCKFILE=src/conan.lock
ARG RUST_TOOLCHAIN=1.92

# External sources
ARG MXL_REPO=https://github.com/dmf-mxl/mxl.git
ARG MXL_REF=81738a15adb55119a6855343bc1053a4389bf6df
ARG GST_PLUGINS_RS_REPO=https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs.git
ARG GST_PLUGINS_RS_REF=207b1f4eeef4c192696181ce2ddb69da8ff73c8a

# Final image arguments
ARG NVNMOS_UID=10001
ARG NVNMOS_GID=10001
ARG EXTRA_APT_PACKAGES=

# -----------------------------------------------------------------------------
# MXL — vcpkg + gst-mxl-rs. libmxl is built once by mxl-sys via cargo (no
# separate cmake step, no mxl-not-built). Runtime resolves libmxl.so via
# LD_LIBRARY_PATH; CMakePresets expect vcpkg at ~/vcpkg (i.e. /root/vcpkg).
# -----------------------------------------------------------------------------
FROM ${BASE_IMAGE} AS mxl-builder

ARG DEBIAN_FRONTEND
ARG MXL_REPO
ARG MXL_REF
ARG RUST_TOOLCHAIN

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    bison \
    ca-certificates \
    clang-19 \
    cmake \
    curl \
    flex \
    git \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer1.0-dev \
    lld-19 \
    llvm-19 \
    ninja-build \
    pkg-config \
    tar \
    unzip \
    zip \
    && rm -rf /var/lib/apt/lists/* \
    && update-alternatives --install /usr/bin/clang clang /usr/bin/clang-19 100 \
    && update-alternatives --install /usr/bin/clang++ clang++ /usr/bin/clang++-19 100 \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain "${RUST_TOOLCHAIN}" --profile minimal

ENV PATH="/root/.cargo/bin:${PATH}"

RUN git clone https://github.com/microsoft/vcpkg.git /root/vcpkg \
    && /root/vcpkg/bootstrap-vcpkg.sh -disableMetrics

RUN git init /mxl && cd /mxl \
    && git remote add origin "${MXL_REPO}" \
    && git fetch --depth 1 origin "${MXL_REF}" \
    && git checkout FETCH_HEAD

WORKDIR /mxl/rust
RUN cargo build --manifest-path Cargo.toml -p gst-mxl-rs --release --locked

# Collect libmxl from mxl-sys' cmake output into a stable path for staging.
# cmake-crate installs to out/lib; out/build/lib holds the build tree (libmxl.so
# is a symlink there, so do not use find -type f).
RUN set -eu \
    && mxl_out_root="$(find /mxl/rust/target/release/build/mxl-sys-*/out -maxdepth 0 -type d | head -1)" \
    && test -d "${mxl_out_root}/lib" \
    && mkdir -p /mxl/install/lib /mxl/install/lib/internal \
    && cp -a "${mxl_out_root}/lib/." /mxl/install/lib/ \
    && if [ -d "${mxl_out_root}/build/lib/internal" ]; then \
         cp -a "${mxl_out_root}/build/lib/internal/." /mxl/install/lib/internal/; \
       fi

# -----------------------------------------------------------------------------
# libnvnmos (C++) — built from this repository (same flow as docker/nvnmos).
# -----------------------------------------------------------------------------
FROM ${BASE_IMAGE} AS nvnmos-cpp-builder

ARG DEBIAN_FRONTEND
ARG PIP_BREAK_SYSTEM_PACKAGES
ARG CONAN_LOCKFILE

ENV PIP_BREAK_SYSTEM_PACKAGES=${PIP_BREAK_SYSTEM_PACKAGES}

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    python3-pip \
    && rm -rf /var/lib/apt/lists/* \
    && pip install cmake~=3.17 conan~=2.2 \
    && conan profile detect

WORKDIR /nvnmos

COPY src/ src/

RUN conan install src \
    --settings:all build_type=Release \
    --build=missing \
    --output-folder=src/conan \
    --lockfile=${CONAN_LOCKFILE} \
    --lockfile-out=src/conan.lock

RUN cmake -B build \
    -DCMAKE_TOOLCHAIN_FILE=conan/conan_toolchain.cmake \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_SHARED_LIBS=ON \
    src \
    && cmake --build build --parallel

# -----------------------------------------------------------------------------
# Rust — nvnmosd and gst-nmos-rs (MXL is built in mxl-builder).
# -----------------------------------------------------------------------------
FROM ${BASE_IMAGE} AS nvnmos-rust-builder

ARG DEBIAN_FRONTEND
ARG RUST_TOOLCHAIN

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    clang \
    cmake \
    curl \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer1.0-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain "${RUST_TOOLCHAIN}" --profile minimal

ENV PATH="/root/.cargo/bin:${PATH}"

COPY --from=nvnmos-cpp-builder /nvnmos /nvnmos
COPY rust/ /nvnmos/rust/

WORKDIR /nvnmos
RUN NVNMOS_LIB_DIR=/nvnmos/build \
    cargo build --manifest-path rust/Cargo.toml \
    -p nvnmosd -p gst-nmos-rs \
    --release --locked

# -----------------------------------------------------------------------------
# gst-plugins-rs — udpsrc2 (udp2) and rtp*pay2 / *depay2 / rtpsmpte291*.
# -----------------------------------------------------------------------------
FROM ${BASE_IMAGE} AS gst-plugins-rs-builder

ARG DEBIAN_FRONTEND
ARG GST_PLUGINS_RS_REPO
ARG GST_PLUGINS_RS_REF
ARG RUST_TOOLCHAIN

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    git \
    libglib2.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer1.0-dev \
    libssl-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain "${RUST_TOOLCHAIN}" --profile minimal \
    && . /root/.cargo/env \
    && cargo install cargo-c --locked --version 0.10.21+cargo-0.95.0

ENV PATH="/root/.cargo/bin:${PATH}"

RUN git init /gst-plugins-rs && cd /gst-plugins-rs \
    && git remote add origin "${GST_PLUGINS_RS_REPO}" \
    && git fetch --depth 1 origin "${GST_PLUGINS_RS_REF}" \
    && git checkout FETCH_HEAD

WORKDIR /gst-plugins-rs
RUN cargo cbuild -p gst-plugin-udp -p gst-plugin-rtp --release

# -----------------------------------------------------------------------------
# Stage runtime libraries and binaries into a fixed layout under /opt/nvnmos.
# -----------------------------------------------------------------------------
FROM ${BASE_IMAGE} AS staging

RUN mkdir -p /nvnmos-lib
COPY --from=nvnmos-cpp-builder /nvnmos/build/libnvnmos.so /nvnmos-lib/
COPY --from=mxl-builder /mxl/install/lib /mxl-lib
COPY --from=nvnmos-rust-builder /nvnmos/rust/target/release/nvnmosd /bin/nvnmosd
COPY --from=nvnmos-rust-builder /nvnmos/rust/target/release/libgstnmos.so /plugins/
COPY --from=mxl-builder /mxl/rust/target/release/libgstmxl.so /plugins/
COPY --from=gst-plugins-rs-builder \
    /gst-plugins-rs/target/x86_64-unknown-linux-gnu/release/libgstrsudp.so /plugins/
COPY --from=gst-plugins-rs-builder \
    /gst-plugins-rs/target/x86_64-unknown-linux-gnu/release/libgstrsrtp.so /plugins/

RUN mkdir -p /opt/nvnmos/bin /opt/nvnmos/lib /opt/nvnmos/lib/mxl/internal \
        /opt/nvnmos/plugins \
    && cp /bin/nvnmosd /opt/nvnmos/bin/ \
    && cp /plugins/libgstnmos.so /plugins/libgstmxl.so \
       /plugins/libgstrsudp.so /plugins/libgstrsrtp.so /opt/nvnmos/plugins/ \
    && cp -a /nvnmos-lib/. /opt/nvnmos/lib/ \
    && cp -a /mxl-lib/. /opt/nvnmos/lib/mxl/ \
    && if [ -d /opt/nvnmos/lib/mxl/internal ]; then :; \
       elif [ -d /mxl-lib/internal ]; then cp -a /mxl-lib/internal/. /opt/nvnmos/lib/mxl/internal/; fi

RUN apt-get update && apt-get install -y --no-install-recommends \
    libc-bin \
    && rm -rf /var/lib/apt/lists/* \
    && set -eu \
    && for bin in /opt/nvnmos/bin/nvnmosd; do \
         ldd "$bin" | awk '/=> \// {print $3}'; \
       done \
    | sort -u \
    | while read -r lib; do \
        case "$lib" in \
          /opt/nvnmos/*) continue ;; \
          /lib/*|/lib64/*|/usr/lib/*) \
            dest="/opt/nvnmos/lib/$(basename "$lib")"; \
            [ -e "$dest" ] || cp -Ln "$lib" "$dest"; \
            ;; \
        esac; \
      done

# -----------------------------------------------------------------------------
# Runtime image — non-root, dbus/avahi for NMOS registration, dumb-init as PID 1.
# -----------------------------------------------------------------------------
FROM ${BASE_IMAGE}

ARG DEBIAN_FRONTEND=noninteractive
ARG EXTRA_APT_PACKAGES
ARG NVNMOS_UID
ARG NVNMOS_GID

ENV NVNMOSD_UDS=/tmp/nvnmosd.sock \
    NVNMOS_PUBLISH_MDNS=1

# By default, use a D-Bus socket file in /tmp so that root privileges are not needed.
# entrypoint-setup.sh starts dbus-daemon at this address.
# Derived images that start D-Bus differently MUST override this variable.
ENV DBUS_SYSTEM_BUS_ADDRESS=unix:path=/tmp/dbus-system-bus-socket

RUN apt-get update && apt-get install -y --no-install-recommends \
    avahi-daemon \
    avahi-utils \
    libavahi-compat-libdnssd1 \
    libnss-mdns \
    dbus \
    dumb-init \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-tools \
    ${EXTRA_APT_PACKAGES} \
    && rm -rf /var/lib/apt/lists/*

# nss-mdns communicates with avahi-daemon via a Unix socket at this compiled-in path.
# avahi-daemon insists on avahi:avahi ownership even with --no-drop-root (see
# https://github.com/avahi/avahi/issues/432). mode 1777 allows any runAsUser UID
# to write the socket.
RUN mkdir -p /run/avahi-daemon \
    && chown avahi:avahi /run/avahi-daemon \
    && chmod 1777 /run/avahi-daemon

COPY --from=staging /opt/nvnmos /opt/nvnmos
COPY docker/gst-nmos-rs/entrypoint.sh docker/nvnmos/entrypoint-setup.sh /opt/nvnmos/

RUN chmod +x /opt/nvnmos/entrypoint.sh /opt/nvnmos/entrypoint-setup.sh \
    && groupadd -g "${NVNMOS_GID}" nvnmos \
    && useradd -u "${NVNMOS_UID}" -g nvnmos -d /opt/nvnmos -s /usr/sbin/nologin nvnmos \
    && chown -R nvnmos:nvnmos /opt/nvnmos

USER nvnmos
WORKDIR /opt/nvnmos

ENTRYPOINT ["/usr/bin/dumb-init", "--", "/opt/nvnmos/entrypoint.sh"]
