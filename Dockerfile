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
# Match .github/workflows/ci.yml until a Conan Center nmos-cpp release is published.
ARG NMOS_CPP_REF=079620d88756aa138ede92d3f52a0102370307fe

FROM ${BASE_IMAGE} as builder

ARG BASE_IMAGE
ARG DEBIAN_FRONTEND
ARG PACKAGE_SUFFIX
ARG PIP_BREAK_SYSTEM_PACKAGES
ARG NMOS_CPP_REF

# use pattern replacement to clean up '/' and ':'
ENV _BASE_IMAGE=${BASE_IMAGE//\//-}
ENV PACKAGE_NAME=nvnmos${PACKAGE_SUFFIX:--${_BASE_IMAGE//:/-}}

RUN apt update && apt install -y gcc git python3-pip doxygen graphviz
RUN pip install cmake~=3.17
RUN pip install conan~=2.2 && conan profile detect

# nmos-cpp alongside nvnmos (see src/CMakeLists.txt NMOS_CPP_DIRECTORY).
RUN git init /nmos-cpp \
    && git -C /nmos-cpp remote add origin https://github.com/sony/nmos-cpp.git \
    && git -C /nmos-cpp fetch --depth 1 origin "${NMOS_CPP_REF}" \
    && git -C /nmos-cpp checkout FETCH_HEAD

# Same layout as README / CI: repo root with src/ and build/ siblings.
WORKDIR /nvnmos

COPY src/ src/
COPY entrypoint.sh LICENSE README.md Doxyfile ./
COPY doc/doxygen doc/doxygen

RUN chmod +x entrypoint.sh

RUN conan install src \
    --settings:all build_type=Release \
    --build=missing \
    --output-folder=src/conan \
    --lockfile="" \
    --lockfile-out=src/conan.lock \
    -o "&:nmos_cpp_from_source=True"

RUN cmake -B build \
    -DCMAKE_TOOLCHAIN_FILE=conan/conan_toolchain.cmake \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_SHARED_LIBS=ON \
    -DUSE_ADD_SUBDIRECTORY=ON \
    src

RUN cmake --build build --parallel 2

RUN cmake --install build --prefix=${PACKAGE_NAME}

RUN cp src/conan.lock ${PACKAGE_NAME}/

RUN cp LICENSE README.md ${PACKAGE_NAME}/

# Doxyfile paths assume CWD is src/ (INPUT, HTML_HEADER, etc.).
RUN cd src && doxygen ../Doxyfile && mv html ../${PACKAGE_NAME}/

RUN tar -cvzf ${PACKAGE_NAME}.tar.gz ${PACKAGE_NAME}

FROM ${BASE_IMAGE}

ARG BASE_IMAGE
ARG PACKAGE_SUFFIX

# use pattern replacement to clean up '/' and ':'
ENV _BASE_IMAGE=${BASE_IMAGE//\//-}
ENV PACKAGE_NAME=nvnmos${PACKAGE_SUFFIX:--${_BASE_IMAGE//:/-}}

COPY --from=builder \
    /nvnmos/${PACKAGE_NAME}.tar.gz \
    /nvnmos/entrypoint.sh \
    ./

ENTRYPOINT ["/entrypoint.sh"]
