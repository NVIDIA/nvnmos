#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Minimal transport-file MXL receiver for controller-driven IS-05 activation.
#
# AddReceiver at NULL→READY with configuring flow_def from `transport-file-path`.
# The resource name comes from `urn:x-nvnmos:tag:name` in the file (no
# `receiver-name`). Domain identity comes from `mxl-domain-path` / `domain_def.json`
# (no `mxl-domain-id` property). The NMOS label comes from flow_def `label`
# in the file (no `label` property).
# Subscription identity (`mxl-flow-id`) and the data path arrive via
# IS-05 PATCH on /single/receivers/{id}/staged.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../env.sh"
require_nvnmosd
bootstrap_mxl_domain

transport_path="$(mktemp "${TMPDIR:-/tmp}/nvnmos-minimal-video.XXXXXX.mxl.json")"
trap 'rm -f "$transport_path"' EXIT
transport_label="minimal ${DEMO_MXL_VIDEO_LABEL} receiver (file)"
render_transport_fixture \
    "$SCRIPT_DIR/fixtures/minimal-video.mxl.json.in" \
    "$transport_path" \
    "$transport_label"

exec gst-launch-1.0 -e \
    nmossrc \
        daemon-uri="$DEMO_DAEMON_URI" \
        transport=mxl \
        node-seed=example-minimal-consumer \
        http-port=18112 \
        mxl-domain-path="$DEMO_MXL_DOMAIN_PATH" \
        transport-file-path="$transport_path" \
        auto-activate=false ! \
    $(demo_video_queue) ! \
    videoconvert ! "$DEMO_VIDEO_SINK" sync=false
