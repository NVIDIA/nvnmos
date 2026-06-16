#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
require_nvnmosd

exec gst-launch-1.0 -e \
    videotestsrc pattern=smpte is-live=true ! \
    "$DEMO_UDP_VIDEO_CAPS" ! \
    nmossink \
        daemon-uri="$DEMO_DAEMON_URI" \
        transport="$DEMO_UDP_TRANSPORT" \
        node-seed=example-producer \
        http-port=18101 \
        sender-name=video1 \
        destination-ip="$DEMO_UDP_VIDEO_MCAST_IP1" \
        destination-port="$DEMO_UDP_VIDEO_MCAST_PORT1" \
        source-ip="$DEMO_NIC_IP" \
        $(udp_video_buffer_props) \
        caps="$DEMO_UDP_VIDEO_CAPS" \
        label="${DEMO_UDP_VIDEO_LABEL} sender" \
        auto-activate=true
