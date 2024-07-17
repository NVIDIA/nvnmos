# SPDX-FileCopyrightText: Copyright (c) 2022-2024 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
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

ARG BASE_IMAGE=ubuntu:22.04
ARG DEBIAN_FRONTEND=noninteractive
ARG PACKAGE_SUFFIX
ARG PIP_BREAK_SYSTEM_PACKAGES=1
ARG USE_CONAN_LOCK=1

FROM ${BASE_IMAGE} as builder

ARG BASE_IMAGE
ARG DEBIAN_FRONTEND
ARG PACKAGE_SUFFIX
ARG PIP_BREAK_SYSTEM_PACKAGES
ARG USE_CONAN_LOCK

# use pattern replacement to clean up '/' and ':'
ENV _BASE_IMAGE=${BASE_IMAGE//\//-}
ENV PACKAGE_NAME=nvnmos${PACKAGE_SUFFIX:--${_BASE_IMAGE//:/-}}

RUN apt update && apt install -y gcc python3-pip doxygen graphviz
RUN pip install cmake~=3.17
RUN pip install conan~=2.2 && conan profile detect

WORKDIR /src

COPY src/ .

COPY entrypoint.sh LICENSE README.md Doxyfile /

RUN chmod +x /entrypoint.sh

RUN conan install . \
    -g CMakeToolchain \
    --settings:all build_type=Release \
    --build=missing \
    --output-folder=conan \
    --lockfile=${USE_CONAN_LOCK:+conan.lock} \
    --lockfile-partial \
    --lockfile-out=conan.lock

RUN cmake -B build \
    -DCMAKE_TOOLCHAIN_FILE=conan/conan_toolchain.cmake \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_SHARED_LIBS=ON \
    .

RUN cmake --build build --parallel

RUN cmake --install build --prefix=${PACKAGE_NAME}

RUN cp conan.lock ${PACKAGE_NAME}/

RUN cp /LICENSE /README.md ${PACKAGE_NAME}/

RUN doxygen ../Doxyfile && mv html ${PACKAGE_NAME}/

RUN tar -cvzf ${PACKAGE_NAME}.tar.gz ${PACKAGE_NAME}

FROM ${BASE_IMAGE}

ARG BASE_IMAGE
ARG PACKAGE_SUFFIX

# use pattern replacement to clean up '/' and ':'
ENV _BASE_IMAGE=${BASE_IMAGE//\//-}
ENV PACKAGE_NAME=nvnmos${PACKAGE_SUFFIX:--${_BASE_IMAGE//:/-}}

COPY --from=builder \
    /src/${PACKAGE_NAME}.tar.gz \
    /entrypoint.sh \
    .

ENTRYPOINT ["/entrypoint.sh"]
