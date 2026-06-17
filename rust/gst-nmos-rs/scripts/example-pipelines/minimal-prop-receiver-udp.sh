#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Minimal properties-driven RTP/UDP receiver for controller-driven IS-05 activation.
#
# AddReceiver at NULL→READY with configuring SDP synthesised from `caps`,
# `interface-ip`, and `receiver-name`.
# Subscription identity (e.g., `multicast-ip`, `destination-port`)
# and the data path arrive via IS-05 PATCH on
# /single/receivers/{id}/staged.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
require_nvnmosd

exec gst-launch-1.0 -e \
    nmossrc \
        daemon-uri="$DEMO_DAEMON_URI" \
        transport="$DEMO_UDP_TRANSPORT" \
        node-seed=example-minimal-consumer \
        receiver-name=video1 \
        interface-ip="$DEMO_NIC_IP" \
        $(udp_video_buffer_props) \
        caps="$DEMO_UDP_VIDEO_CAPS" \
        label="minimal ${DEMO_UDP_VIDEO_LABEL} receiver" \
        auto-activate=false ! \
    $(demo_video_queue) ! \
    videoconvert ! "$DEMO_VIDEO_SINK" sync=false
