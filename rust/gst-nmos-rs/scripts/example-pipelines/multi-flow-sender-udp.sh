#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
require_nvnmosd

exec gst-launch-1.0 -e \
    videotestsrc is-live=true ! "$DEMO_UDP_VIDEO_CAPS" ! \
    nmossink daemon-uri="$DEMO_DAEMON_URI" transport="$DEMO_UDP_TRANSPORT" \
        node-seed=example-multi sender-name=video1 \
        http-port=18105 \
        destination-ip="$DEMO_UDP_VIDEO_MCAST_IP1" destination-port="$DEMO_UDP_VIDEO_MCAST_PORT1" \
        source-ip="$DEMO_NIC_IP" $(udp_video_buffer_props) \
        caps="$DEMO_UDP_VIDEO_CAPS" \
        label="${DEMO_UDP_VIDEO_LABEL} sender" auto-activate=true \
    audiotestsrc wave=sine freq=440 is-live=true ! "$DEMO_UDP_AUDIO_CAPS" ! \
    nmossink daemon-uri="$DEMO_DAEMON_URI" transport="$DEMO_UDP_TRANSPORT" \
        node-seed=example-multi sender-name=audio1 \
        http-port=18105 \
        destination-ip="$DEMO_UDP_AUDIO_MCAST_IP1" destination-port="$DEMO_UDP_AUDIO_MCAST_PORT1" \
        source-ip="$DEMO_NIC_IP" caps="$DEMO_UDP_AUDIO_CAPS" \
        label="${DEMO_UDP_AUDIO_LABEL} sender" auto-activate=true
