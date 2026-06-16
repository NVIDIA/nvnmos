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
        node-seed=example-flipper \
        http-port=18104 \
        receiver-name=video-in \
        mxl-domain-id="$DEMO_MXL_DOMAIN_ID" \
        mxl-domain-path="$DEMO_MXL_DOMAIN_PATH" \
        mxl-flow-id="$DEMO_MXL_VIDEO_FLOW_ID1" \
        caps="$DEMO_MXL_VIDEO_CAPS" \
        auto-activate=true ! \
    $(demo_video_queue) ! \
    videoflip method=horizontal-flip ! \
    nmossink \
        daemon-uri="$DEMO_DAEMON_URI" \
        transport=mxl \
        node-seed=example-flipper \
        http-port=18104 \
        sender-name=video-out \
        mxl-domain-id="$DEMO_MXL_DOMAIN_ID" \
        mxl-domain-path="$DEMO_MXL_DOMAIN_PATH" \
        mxl-flow-id="$DEMO_MXL_VIDEO_FLOW_ID3" \
        auto-activate=true
