#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
require_nvnmosd

# Processor: receive pair-1 video multicast, flip, re-transmit on pair-3
# (Node 3 out). Start 1080p25-sender-udp.sh first.

exec gst-launch-1.0 -e \
    nmossrc \
        daemon-uri="$DEMO_DAEMON_URI" \
        transport="$DEMO_UDP_TRANSPORT" \
        node-seed=example-flipper \
        http-port=18104 \
        receiver-name=video-in \
        multicast-ip="$DEMO_UDP_VIDEO_MCAST_IP1" \
        destination-port="$DEMO_UDP_VIDEO_MCAST_PORT1" \
        interface-ip="$DEMO_NIC_IP" \
        source-ip="$DEMO_NIC_IP" \
        $(udp_video_buffer_props) \
        caps="$DEMO_UDP_VIDEO_CAPS" \
        auto-activate=true ! \
    $(demo_video_queue) ! \
    videoflip method=horizontal-flip ! \
    nmossink \
        daemon-uri="$DEMO_DAEMON_URI" \
        transport="$DEMO_UDP_TRANSPORT" \
        node-seed=example-flipper \
        http-port=18104 \
        sender-name=video-out \
        destination-ip="$DEMO_UDP_VIDEO_MCAST_IP3" \
        destination-port="$DEMO_UDP_VIDEO_MCAST_PORT3" \
        source-ip="$DEMO_NIC_IP" \
        $(udp_video_buffer_props) \
        auto-activate=true
