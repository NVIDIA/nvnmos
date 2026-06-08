# SPDX-FileCopyrightText: Copyright (c) 2022-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
# http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

ARG BASE_IMAGE=ubuntu:24.04
ARG DEBIAN_FRONTEND=noninteractive
ARG PACKAGE_SUFFIX
ARG PIP_BREAK_SYSTEM_PACKAGES=1
ARG CONAN_LOCKFILE=src/conan.lock

# use pattern replacement to clean up '/' and ':'
ARG _BASE_IMAGE=${BASE_IMAGE//\//-}
ARG PACKAGE_NAME=nvnmos${PACKAGE_SUFFIX:--${_BASE_IMAGE//:/-}}

# -----------------------------------------------------------------------------
# C++ library (libnvnmos) — same layout as README / CI (repo root, build/ sibling of src/).
# -----------------------------------------------------------------------------
FROM ${BASE_IMAGE} AS cpp-builder

ARG DEBIAN_FRONTEND
ARG PACKAGE_NAME
ARG PIP_BREAK_SYSTEM_PACKAGES
ARG CONAN_LOCKFILE

ENV PIP_BREAK_SYSTEM_PACKAGES=${PIP_BREAK_SYSTEM_PACKAGES}

RUN apt update && apt install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    python3-pip \
    && rm -rf /var/lib/apt/lists/*
RUN pip install cmake~=3.17
RUN pip install conan~=2.2 && conan profile detect

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
    src

RUN cmake --build build --parallel 2

RUN cmake --install build --prefix=${PACKAGE_NAME}

# -----------------------------------------------------------------------------
# Rust workspace — links against libnvnmos from cpp-builder (CI NVNMOS_LIB_DIR layout).
# -----------------------------------------------------------------------------
FROM ${BASE_IMAGE} AS rust-builder

ARG DEBIAN_FRONTEND

RUN apt update && apt install -y --no-install-recommends \
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
    && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain 1.85 --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /nvnmos

COPY --from=cpp-builder /nvnmos/build /nvnmos/build
COPY --from=cpp-builder /nvnmos/src /nvnmos/src
COPY rust/ rust/

RUN NVNMOS_LIB_DIR=/nvnmos/build cargo build --manifest-path rust/Cargo.toml --workspace --release --locked

# -----------------------------------------------------------------------------
# Package tarball (C++ install tree + Rust artifacts + docs).
# -----------------------------------------------------------------------------
FROM ${BASE_IMAGE} AS package

ARG DEBIAN_FRONTEND
ARG PACKAGE_NAME

RUN apt update && apt install -y --no-install-recommends \
    doxygen \
    graphviz \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /nvnmos

COPY --from=cpp-builder /nvnmos/${PACKAGE_NAME} /nvnmos/${PACKAGE_NAME}
COPY --from=cpp-builder /nvnmos/src /nvnmos/src
COPY --from=cpp-builder /nvnmos/src/conan.lock /nvnmos/src/conan.lock
COPY --from=rust-builder /nvnmos/rust/target/release/libgstnmos.so \
    /nvnmos/${PACKAGE_NAME}/lib/
COPY --from=rust-builder /nvnmos/rust/target/release/nvnmosd \
    /nvnmos/${PACKAGE_NAME}/bin/
COPY --from=rust-builder /nvnmos/rust/target/release/nvnmosd-example \
    /nvnmos/${PACKAGE_NAME}/bin/

COPY entrypoint.sh entrypoint-setup.sh LICENSE README.md Doxyfile ./
COPY doc/doxygen doc/doxygen

RUN chmod +x entrypoint.sh entrypoint-setup.sh \
    && cp src/conan.lock ${PACKAGE_NAME}/ \
    && cp LICENSE README.md ${PACKAGE_NAME}
# Doxyfile paths assume CWD is src/ (INPUT, HTML_HEADER, etc.).
RUN cd src && doxygen ../Doxyfile && mv html ../${PACKAGE_NAME}/

RUN tar -cvzf ${PACKAGE_NAME}.tar.gz ${PACKAGE_NAME}

# -----------------------------------------------------------------------------
# Runtime image (tarball + entrypoint only).
# -----------------------------------------------------------------------------
FROM ${BASE_IMAGE}

ARG DEBIAN_FRONTEND=noninteractive
ARG PACKAGE_NAME
ENV PACKAGE_NAME=${PACKAGE_NAME}

RUN apt update && apt install -y --no-install-recommends \
    avahi-daemon \
    avahi-utils \
    libavahi-compat-libdnssd1 \
    libnss-mdns \
    dbus \
    && rm -rf /var/lib/apt/lists/*

# nss-mdns communicates with avahi-daemon via a Unix socket at this compiled-in path.
# avahi-daemon insists on avahi:avahi ownership even with --no-drop-root (see
# https://github.com/avahi/avahi/issues/432). mode 1777 allows any runAsUser UID
# to write the socket when the container is started with `docker run --user`.
RUN mkdir -p /run/avahi-daemon \
    && chown avahi:avahi /run/avahi-daemon \
    && chmod 1777 /run/avahi-daemon

# By default, use a D-Bus socket file in /tmp so that root privileges are not needed.
# entrypoint-setup.sh starts dbus-daemon at this address.
# Derived images that start D-Bus differently MUST override this variable.
ENV DBUS_SYSTEM_BUS_ADDRESS=unix:path=/tmp/dbus-system-bus-socket

COPY --from=package \
    /nvnmos/${PACKAGE_NAME}.tar.gz \
    /nvnmos/entrypoint.sh \
    /nvnmos/entrypoint-setup.sh \
    ./

ENTRYPOINT ["/entrypoint.sh"]
