#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Minimal MXL receiver for controller-driven IS-05 activation.
#
# AddReceiver at NULL→READY with configuring flow_def from `caps`
# and `mxl-domain-*` only.
# Subscription identity (`mxl-flow-id`) and the data path arrive via
# IS-05 PATCH on /single/receivers/{id}/staged.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
require_nvnmosd
bootstrap_mxl_domain

exec gst-launch-1.0 -e \
    nmossrc \
        daemon-uri="$DEMO_DAEMON_URI" \
        transport=mxl \
        node-seed=example-minimal-consumer \
        http-port=18112 \
        receiver-name=video1 \
        mxl-domain-id="$DEMO_MXL_DOMAIN_ID" \
        mxl-domain-path="$DEMO_MXL_DOMAIN_PATH" \
        caps="$DEMO_MXL_VIDEO_CAPS" \
        label="minimal ${DEMO_MXL_VIDEO_LABEL} receiver" \
        auto-activate=false ! \
    $(demo_video_queue) ! \
    videoconvert ! "$DEMO_VIDEO_SINK" sync=false
