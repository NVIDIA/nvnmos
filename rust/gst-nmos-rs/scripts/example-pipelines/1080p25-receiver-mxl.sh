#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
require_nvnmosd
bootstrap_mxl_domain

exec gst-launch-1.0 -e \
    nmossrc \
        daemon-uri="$DEMO_DAEMON_URI" \
        transport=mxl \
        node-seed=example-consumer \
        http-port=18102 \
        receiver-name=video2 \
        mxl-domain-id="$DEMO_MXL_DOMAIN_ID" \
        mxl-domain-path="$DEMO_MXL_DOMAIN_PATH" \
        mxl-flow-id="$DEMO_MXL_VIDEO_FLOW_ID1" \
        caps="$DEMO_MXL_VIDEO_CAPS" \
        label="${DEMO_MXL_VIDEO_LABEL} receiver" \
        auto-activate=true ! \
    $(demo_video_queue) ! \
    videoconvert ! "$DEMO_VIDEO_SINK" sync=false
