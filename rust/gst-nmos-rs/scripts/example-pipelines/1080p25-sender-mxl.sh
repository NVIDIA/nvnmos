#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
require_nvnmosd
bootstrap_mxl_domain

exec gst-launch-1.0 -e \
    videotestsrc pattern=smpte is-live=true ! \
    "$DEMO_MXL_VIDEO_CAPS" ! \
    nmossink \
        daemon-uri="$DEMO_DAEMON_URI" \
        transport=mxl \
        node-seed=example-producer \
        http-port=18101 \
        sender-name=video1 \
        mxl-domain-id="$DEMO_MXL_DOMAIN_ID" \
        mxl-domain-path="$DEMO_MXL_DOMAIN_PATH" \
        mxl-flow-id="$DEMO_MXL_VIDEO_FLOW_ID1" \
        caps="$DEMO_MXL_VIDEO_CAPS" \
        label="${DEMO_MXL_VIDEO_LABEL} sender" \
        auto-activate=true
