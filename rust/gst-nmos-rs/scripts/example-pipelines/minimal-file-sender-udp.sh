#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Minimal transport-file RTP/UDP sender for controller-driven IS-05 activation.
#
# AddSender at NULL→READY with configuring SDP from `transport-file-path`.
# The resource name comes from `a=x-nvnmos-name` in the file (no `sender-name`).
# The NMOS label comes from SDP `s=` in the file (no `label` property).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../env.sh"
require_nvnmosd

transport_path="$(mktemp "${TMPDIR:-/tmp}/nvnmos-minimal-video.XXXXXX.sdp")"
trap 'rm -f "$transport_path"' EXIT
transport_label="minimal ${DEMO_UDP_VIDEO_LABEL} sender (file)"
render_transport_fixture \
    "$SCRIPT_DIR/fixtures/minimal-video.sdp.in" \
    "$transport_path" \
    "$transport_label"

exec gst-launch-1.0 -e \
    videotestsrc pattern=smpte is-live=true ! \
    "$DEMO_UDP_VIDEO_CAPS" ! \
    nmossink \
        daemon-uri="$DEMO_DAEMON_URI" \
        transport="$DEMO_UDP_TRANSPORT" \
        node-seed=example-minimal-producer \
        $(udp_video_buffer_props) \
        transport-file-path="$transport_path" \
        auto-activate=false
