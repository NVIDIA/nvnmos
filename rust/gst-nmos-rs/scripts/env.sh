# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Shared defaults for the interactive demo and example-pipelines/*.sh.
# Source from `scripts/example-pipelines/*.sh`:
#   source "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
# Source from `scripts/gst-nmos-rs-demo.sh`:
#   source "$(dirname "${BASH_SOURCE[0]}")/env.sh"
#
# All user-tunable values use the `DEMO_` prefix. The demo script may
# override paths (domain, node seeds) before sourcing this file.
#
# MXL flow IDs — paired video/audio per set (1–4):
#   NNNNNNNN-aaaa-… = video, NNNNNNNN-bbbb-… = audio (same N prefix per pair).
# RTP/UDP multicast — paired video/audio per set (1–4), same index as MXL pairs.

# gRPC UDS — must match a running `nvnmosd`.
export DEMO_DAEMON_SOCK=${DEMO_DAEMON_SOCK:-/tmp/nvnmosd.sock}
export DEMO_DAEMON_URI="unix:${DEMO_DAEMON_SOCK}"

# Local NIC for RTP/UDP IS-05 endpoint properties and SDP source-filter.
# Skips loopback (WSL2 can assign a global-scope address on `lo`).
# Override with DEMO_NIC_IP.
_default_nic_ip() {
    local addr
    if command -v ip >/dev/null 2>&1; then
        addr=$(ip -4 -o addr show 2>/dev/null \
            | awk '$2 != "lo" {print $4; exit}' \
            | cut -d/ -f1)
    fi
    if [[ -z "${addr:-}" ]]; then
        addr=$(hostname -I 2>/dev/null | awk '{print $1}')
    fi
    echo "${addr:-127.0.0.1}"
}
export DEMO_NIC_IP=${DEMO_NIC_IP:-$(_default_nic_ip)}

# `transport={udp,udp2,nvdsudp}` on nmossrc/nmossink for RTP/UDP (ST 2110).
# `udp` requires gst-plugins-good, `udp2` also requires gst-plugins-rs.
# `nvdsudp` requires gst-nvdsudp from DeepStream 9.0 and Rivermax (see README).
export DEMO_UDP_TRANSPORT=${DEMO_UDP_TRANSPORT:-udp}

# Primary essence: 1080p25 10-bit 4:2:2 video + 2 ch @ 48 kHz audio.
export DEMO_MXL_VIDEO_CAPS='video/x-raw,format=v210,width=1920,height=1080,framerate=25/1,interlace-mode=progressive'
export DEMO_UDP_VIDEO_CAPS='video/x-raw,format=UYVP,width=1920,height=1080,framerate=25/1,interlace-mode=progressive'
export DEMO_MXL_AUDIO_CAPS='audio/x-raw,format=F32LE,rate=48000,channels=2,layout=interleaved'
export DEMO_UDP_AUDIO_CAPS='audio/x-raw,format=S24BE,rate=48000,channels=2,layout=interleaved'
# Truncated human-readable essence name for NMOS `label=` — keep in sync with *_CAPS.
export DEMO_MXL_VIDEO_LABEL=${DEMO_MXL_VIDEO_LABEL:-1080p25 v210}
export DEMO_UDP_VIDEO_LABEL=${DEMO_UDP_VIDEO_LABEL:-1080p25 UYVP}
export DEMO_MXL_AUDIO_LABEL=${DEMO_MXL_AUDIO_LABEL:-48 kHz F32LE 2ch}
export DEMO_UDP_AUDIO_LABEL=${DEMO_UDP_AUDIO_LABEL:-48 kHz S24BE 2ch}

# Alternate essence (demo Node 4 / wide-receiver CAPS renegotiation tests).
export DEMO_MXL_VIDEO_CAPS_ALT='video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001,interlace-mode=progressive'
export DEMO_UDP_VIDEO_CAPS_ALT='video/x-raw,format=UYVP,width=1920,height=1080,framerate=30000/1001,interlace-mode=progressive'
export DEMO_MXL_AUDIO_CAPS_ALT='audio/x-raw,format=F32LE,rate=48000,channels=8,layout=interleaved'
export DEMO_UDP_AUDIO_CAPS_ALT='audio/x-raw,format=S24BE,rate=48000,channels=8,layout=interleaved'
export DEMO_MXL_VIDEO_LABEL_ALT=${DEMO_MXL_VIDEO_LABEL_ALT:-1080p29.97 v210}
export DEMO_UDP_VIDEO_LABEL_ALT=${DEMO_UDP_VIDEO_LABEL_ALT:-1080p29.97 UYVP}
export DEMO_MXL_AUDIO_LABEL_ALT=${DEMO_MXL_AUDIO_LABEL_ALT:-48 kHz F32LE 8ch}
export DEMO_UDP_AUDIO_LABEL_ALT=${DEMO_UDP_AUDIO_LABEL_ALT:-48 kHz S24BE 8ch}
# ST 2110-30 shorter packet duration for alt UDP audio (Node 4).
export DEMO_UDP_AUDIO_TRANSPORT_CAPS_ALT='application/x-rtp,a-ptime=(string)0.125'

# UDP multicast destination per flow pair (1–4), mirroring DEMO_MXL_*_FLOW_ID*.
export DEMO_UDP_VIDEO_MCAST_IP1=${DEMO_UDP_VIDEO_MCAST_IP1:-232.99.99.1}
export DEMO_UDP_VIDEO_MCAST_PORT1=${DEMO_UDP_VIDEO_MCAST_PORT1:-5004}
export DEMO_UDP_AUDIO_MCAST_IP1=${DEMO_UDP_AUDIO_MCAST_IP1:-232.99.99.2}
export DEMO_UDP_AUDIO_MCAST_PORT1=${DEMO_UDP_AUDIO_MCAST_PORT1:-5006}
export DEMO_UDP_VIDEO_MCAST_IP2=${DEMO_UDP_VIDEO_MCAST_IP2:-232.99.99.7}
export DEMO_UDP_VIDEO_MCAST_PORT2=${DEMO_UDP_VIDEO_MCAST_PORT2:-5016}
export DEMO_UDP_AUDIO_MCAST_IP2=${DEMO_UDP_AUDIO_MCAST_IP2:-232.99.99.8}
export DEMO_UDP_AUDIO_MCAST_PORT2=${DEMO_UDP_AUDIO_MCAST_PORT2:-5018}
export DEMO_UDP_VIDEO_MCAST_IP3=${DEMO_UDP_VIDEO_MCAST_IP3:-232.99.99.3}
export DEMO_UDP_VIDEO_MCAST_PORT3=${DEMO_UDP_VIDEO_MCAST_PORT3:-5008}
export DEMO_UDP_AUDIO_MCAST_IP3=${DEMO_UDP_AUDIO_MCAST_IP3:-232.99.99.4}
export DEMO_UDP_AUDIO_MCAST_PORT3=${DEMO_UDP_AUDIO_MCAST_PORT3:-5010}
export DEMO_UDP_VIDEO_MCAST_IP4=${DEMO_UDP_VIDEO_MCAST_IP4:-232.99.99.5}
export DEMO_UDP_VIDEO_MCAST_PORT4=${DEMO_UDP_VIDEO_MCAST_PORT4:-5012}
export DEMO_UDP_AUDIO_MCAST_IP4=${DEMO_UDP_AUDIO_MCAST_IP4:-232.99.99.6}
export DEMO_UDP_AUDIO_MCAST_PORT4=${DEMO_UDP_AUDIO_MCAST_PORT4:-5014}

# MXL domain (see `bootstrap_mxl_domain`).
export DEMO_MXL_DOMAIN_ID=${DEMO_MXL_DOMAIN_ID:-$(uuidgen)}
export DEMO_MXL_DOMAIN_PATH=${DEMO_MXL_DOMAIN_PATH:-/dev/shm/gst-nmos-rs-examples}

export DEMO_MXL_VIDEO_FLOW_ID1=${DEMO_MXL_VIDEO_FLOW_ID1:-11111111-aaaa-1111-aaaa-111111111111}
export DEMO_MXL_AUDIO_FLOW_ID1=${DEMO_MXL_AUDIO_FLOW_ID1:-11111111-bbbb-1111-bbbb-111111111111}
export DEMO_MXL_VIDEO_FLOW_ID2=${DEMO_MXL_VIDEO_FLOW_ID2:-22222222-aaaa-2222-aaaa-222222222222}
export DEMO_MXL_AUDIO_FLOW_ID2=${DEMO_MXL_AUDIO_FLOW_ID2:-22222222-bbbb-2222-bbbb-222222222222}
export DEMO_MXL_VIDEO_FLOW_ID3=${DEMO_MXL_VIDEO_FLOW_ID3:-33333333-aaaa-3333-aaaa-333333333333}
export DEMO_MXL_AUDIO_FLOW_ID3=${DEMO_MXL_AUDIO_FLOW_ID3:-33333333-bbbb-3333-bbbb-333333333333}
export DEMO_MXL_VIDEO_FLOW_ID4=${DEMO_MXL_VIDEO_FLOW_ID4:-44444444-aaaa-4444-aaaa-444444444444}
export DEMO_MXL_AUDIO_FLOW_ID4=${DEMO_MXL_AUDIO_FLOW_ID4:-44444444-bbbb-4444-bbbb-444444444444}

# udpsrc/udpsink socket buffer for video (bytes). Not used with nvdsudp.
export DEMO_UDP_VIDEO_BUFFER_SIZE=${DEMO_UDP_VIDEO_BUFFER_SIZE:-16777216}

# Queues after nmossrc on receiver / processor branches.
export DEMO_VIDEO_QUEUE_MAX_BUFFERS=${DEMO_VIDEO_QUEUE_MAX_BUFFERS:-2}
export DEMO_AUDIO_QUEUE_MAX_TIME_MS=${DEMO_AUDIO_QUEUE_MAX_TIME_MS:-50}

# Playback sink elements (name only — not a chain). Scripts add
# videoconvert/audioconvert upstream as needed. Override for
# headless or CI (e.g. DEMO_VIDEO_SINK=fakesink DEMO_AUDIO_SINK=fakesink).
export DEMO_VIDEO_SINK=${DEMO_VIDEO_SINK:-autovideosink}
export DEMO_AUDIO_SINK=${DEMO_AUDIO_SINK:-autoaudiosink}

demo_video_queue() {
    local name=${1:-}
    if [[ -n "$name" ]]; then
        echo queue name="$name" \
            "max-size-buffers=$DEMO_VIDEO_QUEUE_MAX_BUFFERS" \
            max-size-bytes=0 max-size-time=0
    else
        echo queue \
            "max-size-buffers=$DEMO_VIDEO_QUEUE_MAX_BUFFERS" \
            max-size-bytes=0 max-size-time=0
    fi
}

demo_audio_queue() {
    local name=${1:-}
    local max_time_ns=$(( DEMO_AUDIO_QUEUE_MAX_TIME_MS * 1000000 ))
    if [[ -n "$name" ]]; then
        echo queue name="$name" \
            "max-size-time=$max_time_ns" \
            max-size-buffers=0 max-size-bytes=0
    else
        echo queue \
            "max-size-time=$max_time_ns" \
            max-size-buffers=0 max-size-bytes=0
    fi
}

udp_video_buffer_props() {
    local t=${1:-${DEMO_TRANSPORT:-${DEMO_UDP_TRANSPORT:-udp}}}
    case "$t" in
        udp|udp2)
            printf 'transport-properties="properties,buffer-size=%s"' \
                "$DEMO_UDP_VIDEO_BUFFER_SIZE"
            ;;
    esac
}

udp_audio_transport_caps_alt() {
    local t=${1:-${DEMO_TRANSPORT:-${DEMO_UDP_TRANSPORT:-udp}}}
    case "$t" in
        udp|udp2|nvdsudp)
            printf 'transport-caps="%s"' "$DEMO_UDP_AUDIO_TRANSPORT_CAPS_ALT"
            ;;
    esac
}

is_udp_transport() {
    case "${1:-${DEMO_TRANSPORT:-${DEMO_UDP_TRANSPORT:-udp}}}" in
        udp|udp2|nvdsudp) return 0 ;;
        *) return 1 ;;
    esac
}

require_nvnmosd() {
    if ! ss -xlpn 2>/dev/null | rg -F "LISTEN" | rg -qF "$DEMO_DAEMON_SOCK"; then
        echo "[error] $DEMO_DAEMON_SOCK is not listening — start nvnmosd first" >&2
        exit 1
    fi
}

bootstrap_mxl_domain() {
    mkdir -p "$DEMO_MXL_DOMAIN_PATH"
    local def="$DEMO_MXL_DOMAIN_PATH/domain_def.json"
    if [[ -f "$def" ]]; then
        if command -v jq >/dev/null 2>&1; then
            local existing_id
            existing_id=$(jq -r '.id // empty' "$def" 2>/dev/null || true)
            if [[ -n "$existing_id" ]]; then
                export DEMO_MXL_DOMAIN_ID="$existing_id"
                return 0
            fi
        fi
        echo "[warn] $def exists but .id could not be read — using DEMO_MXL_DOMAIN_ID=$DEMO_MXL_DOMAIN_ID" >&2
        return 0
    fi
    printf '{"id":"%s","label":"gst-nmos-rs example domain"}\n' "$DEMO_MXL_DOMAIN_ID" \
        >"$def"
}

# Substitute @NIC_IP@, @MXL_DOMAIN_ID@, and @LABEL@ in example-pipeline fixtures.
render_transport_fixture() {
    local template=$1
    local output=$2
    local label=${3-}
    local escaped_label
    # Prepare label for sed substitution (escape slashes and ampersands).
    escaped_label=$(printf '%s' "$label" | sed 's/[\/&]/\\&/g')
    sed -e "s/@NIC_IP@/${DEMO_NIC_IP}/g" \
        -e "s/@MXL_DOMAIN_ID@/${DEMO_MXL_DOMAIN_ID}/g" \
        -e "s/@LABEL@/${escaped_label}/g" \
        "$template" >"$output"
}
