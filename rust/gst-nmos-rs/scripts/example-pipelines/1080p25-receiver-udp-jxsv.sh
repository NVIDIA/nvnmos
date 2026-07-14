#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# 1080p25 JPEG XS RTP/UDP receiver — eager activation, matched to
# `1080p25-receiver-udp.sh` (multicast group 1, `auto-activate=true`).
#
# Requires `gst-plugins-rs` (`rsrtp`: rtpjxsvdepay) and `svtjpegxsdec`.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
require_nvnmosd

exec gst-launch-1.0 -e \
    nmossrc \
        daemon-uri="$DEMO_DAEMON_URI" \
        transport="$DEMO_UDP_TRANSPORT" \
        node-seed=example-consumer \
        http-port=18102 \
        receiver-name=video2 \
        multicast-ip="$DEMO_UDP_VIDEO_MCAST_IP1" \
        destination-port="$DEMO_UDP_VIDEO_MCAST_PORT1" \
        interface-ip="$DEMO_NIC_IP" \
        source-ip="$DEMO_NIC_IP" \
        $(udp_video_buffer_props) \
        caps="$DEMO_UDP_VIDEO_JXSV_CAPS" \
        label="${DEMO_UDP_VIDEO_JXSV_LABEL} receiver" \
        auto-activate=true ! \
    $(demo_video_queue) ! \
    svtjpegxsdec ! videoconvert ! "$DEMO_VIDEO_SINK" sync=false
