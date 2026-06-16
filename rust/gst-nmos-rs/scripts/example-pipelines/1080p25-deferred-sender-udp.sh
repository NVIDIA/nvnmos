#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
require_nvnmosd

# Deferred AddSender: no `caps=` on nmossink — peer caps at READY→PAUSED
# drive SDP synthesis. Endpoint props must still be set.
exec gst-launch-1.0 -e \
    videotestsrc pattern=smpte is-live=true ! \
    "$DEMO_UDP_VIDEO_CAPS" ! \
    nmossink \
        daemon-uri="$DEMO_DAEMON_URI" \
        transport="$DEMO_UDP_TRANSPORT" \
        node-seed=example-deferred \
        http-port=18103 \
        sender-name=video1 \
        destination-ip="$DEMO_UDP_VIDEO_MCAST_IP1" \
        destination-port="$DEMO_UDP_VIDEO_MCAST_PORT1" \
        source-ip="$DEMO_NIC_IP" \
        $(udp_video_buffer_props) \
        auto-activate=true
