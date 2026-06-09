#!/usr/bin/bash

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

set -euo pipefail

_nvnmos_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=entrypoint-setup.sh
source "${_nvnmos_dir}/entrypoint-setup.sh"

NVNMOS_WORK_DIR="${NVNMOS_WORK_DIR:-/tmp/nvnmos}"
mkdir -p "${NVNMOS_WORK_DIR}"
tar -xf "${_nvnmos_dir}/${PACKAGE_NAME}.tar.gz" -C "${NVNMOS_WORK_DIR}"
PACKAGE_ROOT="${NVNMOS_WORK_DIR}/${PACKAGE_NAME}"

export HOSTIP
HOSTIP="$(awk 'END{print $1}' /etc/hosts)"

# User-mode Avahi does not register the host name the way systemd does; publish
# explicitly so nvnmos-example's ${HOSTNAME}.local resolves in the container.
publish_mdns_hostname "${HOSTNAME}.local"
sleep 1

export LD_LIBRARY_PATH="${PACKAGE_ROOT}/lib${LD_LIBRARY_PATH:+:${LD_LIBRARY_PATH}}"

# yes(1) exits with SIGPIPE once nvnmos-example stops reading; check the example only.
set +o pipefail
yes | "${PACKAGE_ROOT}/bin/nvnmos-example" "${HOSTNAME}.local" 8080 "${HOSTIP}"
example_status=${PIPESTATUS[1]}
set -o pipefail
if [[ "${example_status}" -ne 0 ]]; then
    echo "nvnmos-example failed with status ${example_status}" >&2
    exit 1
fi

export NVNMOSD_UDS="${NVNMOSD_UDS:-/tmp/nvnmosd.sock}"
rm -f "${NVNMOSD_UDS}"
"${PACKAGE_ROOT}/bin/nvnmosd" &
nvnmosd_pid=$!
pids+=("${nvnmosd_pid}")
for _ in {1..50}; do [ -S "${NVNMOSD_UDS}" ] && break; sleep 0.1; done
[ -S "${NVNMOSD_UDS}" ] || {
    echo "nvnmosd failed to listen on ${NVNMOSD_UDS}" >&2
    exit 1
}
"${PACKAGE_ROOT}/bin/nvnmosd-example"
kill "${nvnmosd_pid}" 2>/dev/null || true
wait "${nvnmosd_pid}" 2>/dev/null || true

exec "$@"
