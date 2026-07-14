#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Minimal properties-driven JPEG XS RTP/UDP sender for controller-driven IS-05
# activation.
#
# AddSender at NULL→READY with configuring SDP synthesised from `caps`,
# `source-ip`, `sender-name`, and `format-bit-rate`.
#
# For JPEG XS the bit rate is format-defining: `format-bit-rate` (kbit/s) drives
# the SDP `b=AS:` / fmtp advertisement and the `rtpjxsvpay max-codestream-bitrate`
# ceiling. Omit it and the sender advertises no rate.
#
# Requires `gst-plugins-rs` (`rsrtp`: rtpjxsvpay) and a JPEG XS encoder
# (`svtjpegxsenc`).
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
        node-seed=example-minimal-jxsv-producer \
        sender-name=video1 \
        source-ip="$DEMO_NIC_IP" \
        $(udp_video_buffer_props) \
        caps="$DEMO_UDP_VIDEO_JXSV_CAPS" \
        format-bit-rate="$DEMO_UDP_VIDEO_JXSV_BIT_RATE" \
        label="minimal ${DEMO_UDP_VIDEO_JXSV_LABEL} sender" \
        auto-activate=false
