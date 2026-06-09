#!/usr/bin/bash

# SPDX-FileCopyrightText: Copyright (c) 2024-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Starts dbus + avahi, nvnmosd, then runs a single foreground command (typically
# gst-launch-1.0). Future multi-pipeline support can branch here on an env var
# (e.g. NVNMOS_PIPELINE_MODE=multi) without changing the container image layout.
#
# mDNS hostname publish is on by default (${HOSTNAME}.local). Set
# NVNMOS_PUBLISH_MDNS=0 to disable.
#
# For transport=mxl, domain_def.json must exist at the pipeline's
# mxl-domain-path before gst-launch starts; this entrypoint does not create
# domains or replicate across hosts.

set -euo pipefail

_nvdir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=entrypoint-setup.sh
source "${_nvdir}/entrypoint-setup.sh"

NVNMOSD_UDS="${NVNMOSD_UDS:-/tmp/nvnmosd.sock}"

export NVNMOSD_UDS
export LD_LIBRARY_PATH="/opt/nvnmos/lib:/opt/nvnmos/lib/mxl:/opt/nvnmos/lib/mxl/internal${LD_LIBRARY_PATH:+:${LD_LIBRARY_PATH}}"
export GST_PLUGIN_PATH="/opt/nvnmos/plugins${GST_PLUGIN_PATH:+:${GST_PLUGIN_PATH}}"

nvnmosd_pid=""

stop_nvnmosd() {
    if [[ -n "${nvnmosd_pid}" ]]; then
        kill -TERM "${nvnmosd_pid}" 2>/dev/null || true
        wait "${nvnmosd_pid}" 2>/dev/null || true
        nvnmosd_pid=""
    fi
}

trap 'stop_nvnmosd; cleanup' EXIT INT TERM

if [[ "${NVNMOS_PUBLISH_MDNS:-1}" != "0" ]]; then
    publish_mdns_hostname "${HOSTNAME}.local"
    sleep 1
fi

rm -f "${NVNMOSD_UDS}"
/opt/nvnmos/bin/nvnmosd &
nvnmosd_pid=$!
pids+=("${nvnmosd_pid}")

for _ in {1..50}; do
    [[ -S "${NVNMOSD_UDS}" ]] && break
    sleep 0.1
done
[[ -S "${NVNMOSD_UDS}" ]] || {
    echo "nvnmosd failed to listen on ${NVNMOSD_UDS}" >&2
    exit 1
}

if [[ $# -eq 0 ]]; then
    echo "No command supplied. Pass gst-launch-1.0 (or another foreground process) as container args." >&2
    exit 1
fi

set +e
"$@"
status=$?
set -e
exit "${status}"
