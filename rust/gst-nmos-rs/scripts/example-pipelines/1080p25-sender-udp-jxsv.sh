#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# 1080p25 JPEG XS RTP/UDP sender — eager activation, matched to
# `1080p25-sender-udp.sh` (multicast group 1, `auto-activate=true`).
#
# Requires `gst-plugins-rs` (`rsrtp`: rtpjxsvpay) and `svtjpegxsenc`.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
require_nvnmosd

exec gst-launch-1.0 -e \
    videotestsrc pattern=smpte is-live=true ! \
    "$DEMO_UDP_VIDEO_JXSV_RAW_CAPS" ! \
    svtjpegxsenc ! \
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
        caps="$DEMO_UDP_VIDEO_JXSV_CAPS" \
        format-bit-rate="$DEMO_UDP_VIDEO_JXSV_BIT_RATE" \
        label="${DEMO_UDP_VIDEO_JXSV_LABEL} sender" \
        auto-activate=true
