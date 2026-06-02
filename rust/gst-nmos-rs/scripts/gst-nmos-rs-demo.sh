#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Interactive three-Node demo for the `gst-nmos-rs` plugin against a
# live `nvnmosd` daemon. The transport family is selected via the
# `DEMO_TRANSPORT` env var:
#
#   DEMO_TRANSPORT=mxl   (default): MXL shared-memory transport
#                                   (`mxlsrc` / `mxlsink`). Requires
#                                   `/dev/shm` write access + a real
#                                   MXL toolchain.
#   DEMO_TRANSPORT=udp              ST 2110 over RTP/UDP via
#                                   gst-plugins-good (`udpsrc` /
#                                   `udpsink` + the `rtp*pay` /
#                                   `rtp*depay` family).
#   DEMO_TRANSPORT=udp2             ST 2110 over RTP/UDP, preferring
#                                   gst-plugins-rs's `udpsrc2` and
#                                   `*pay2` / `*depay2` siblings
#                                   where available.
#
# The topology and interactive control loop are identical across
# transports; only the per-Sender / per-Receiver property surface,
# the essence caps, and the diagnostics differ. The MXL path uses
# `mxl-domain-id` / `mxl-domain-path` / `mxl-flow-id` to identify
# Flows; the UDP path uses the IS-05 endpoint properties
# (`destination-ip` etc. on senders, `multicast-ip` etc. on
# receivers) with per-flow multicast groups.
#
#   Node 1 (producer):  one pipeline with audiotestsrc (440 Hz tone) +
#                       videotestsrc (smpte bars, horizontal-speed=2),
#                       fed to two `nmossink` Senders on the same Node
#                       (different sender-name, different per-flow
#                       identity — MXL flow-id, or destination
#                       multicast group + port).
#
#   Node 2 (consumer):  one pipeline with two `nmossrc` Receivers (one
#                       per format) on the same Node, fed to `autoaudiosink`
#                       and `autovideosink` (override AUDIO_SINK / VIDEO_SINK
#                       via env for WSL / headless setups).
#
#   Node 3 (processor): two separate gst-launch-1.0 processes sharing
#                       one node-seed (one Node, two gst processes):
#                         a) nmossrc → videoflip horizontal → nmossink
#                         b) nmossrc → volume 0.3            → nmossink
#                       Both Receivers pull Node 1's flows; both Senders
#                       publish processed flows that Node 2 can switch to
#                       via IS-05 PATCH.
#
# Out of the box everything is pre-wired by per-flow identity (MXL
# flow-id or RTP multicast group + port) so the user sees and hears
# Node 1's flows on Node 2 immediately. The interactive curl examples
# printed at the end let the user PATCH Receivers to disable,
# re-enable, or rewire to Node 3's processed flows.
#
# MXL mode requires a real MXL toolchain + `/dev/shm` (cannot run in
# a sandbox without shared-memory write access). UDP / UDP2 modes
# require only the gst-plugins-good / gst-plugins-rs RTP elements and
# a NIC that can carry multicast — see `DEMO_NIC_IP` below.
#
# Stop with Ctrl+C — the cleanup trap SIGTERMs the daemon + all
# gst-launch pipelines, waits up to 2 s for orderly shutdown, then
# SIGKILLs any stragglers. Do NOT use Ctrl+Z: SIGTSTP stops the
# script (and its children) before the trap can fire, and if the
# daemon child is then killed externally it becomes a zombie
# parented to the stopped script (the kernel can't reparent to
# init until the parent actually exits).

set -u

# ---- Knobs ---------------------------------------------------------

# Transport family for the inner data path. `mxl` (the default) keeps
# this script's historical behaviour: MXL shared-memory transport on
# `/dev/shm`. `udp` and `udp2` switch every Sender / Receiver in the
# topology to ST 2110 over RTP/UDP — they share the property
# surface, differing only in which RTP factory family the inner chain
# picks (gst-plugins-good vs gst-plugins-rs). Validated below once
# the helper functions are in place.
DEMO_TRANSPORT=${DEMO_TRANSPORT:-mxl}

# Local NIC IP used by the UDP path: drives the receiver's
# `interface-ip` (the IGMP-join interface, resolved to an iface name
# for `udpsrc.multicast-iface`) and the sender's `source-ip`
# (`udpsink.bind-address` plus the SDP `a=source-filter:`
# include-source). Ignored when DEMO_TRANSPORT=mxl. Defaults to the
# host's first non-loopback IPv4 address (typically `eth0` on
# WSL / Docker; the primary physical NIC otherwise). libnvnmos
# walks the host's iface list and skips `lo` when resolving the
# SDP `a=x-nvnmos-iface-ip` attribute back to an iface, so anything
# assigned to the loopback iface is rejected — including non-127.x
# global-scope IPs like WSL2's quirky `10.255.255.254/32` on `lo`.
_default_nic_ip() {
    local addr
    if command -v ip >/dev/null 2>&1; then
        # `ip -4 -o addr show` prints one line per address; column
        # 2 is the iface name, column 4 is the CIDR. Skip the
        # loopback iface explicitly (WSL2 can land a global-scope
        # IP on `lo`, so `scope global` alone isn't sufficient) and
        # take the first surviving address.
        addr=$(ip -4 -o addr show 2>/dev/null \
            | awk '$2 != "lo" {print $4; exit}' \
            | cut -d/ -f1)
    fi
    if [[ -z "${addr:-}" ]]; then
        # Fallback: `hostname -I` (space-separated list, already
        # excludes 127.0.0.1 by `hostname(1)` convention). Safety
        # net for systems with iproute2 deinstalled / replaced.
        addr=$(hostname -I 2>/dev/null | awk '{print $1}')
    fi
    echo "${addr:-127.0.0.1}"
}
DEMO_NIC_IP=${DEMO_NIC_IP:-$(_default_nic_ip)}

# Resolve the script's own directory so `REPO` works in any checkout
# without an env override. Layout assumed: <repo>/rust/gst-nmos-rs/scripts/
# i.e. three `..` to reach the repo root.
_SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=${REPO:-$(cd "$_SCRIPT_DIR/../../.." && pwd)}
MXL_REPO=${MXL_REPO:-$REPO/../mxl}

# `libnvnmos.so` (consumed by nvnmosd) and the gst-mxl-rs plugin +
# libmxl.so runtime (consumed by the inner mxlsink/mxlsrc swaps).
LIB=${NVNMOS_LIB_DIR:-$REPO/build-mxl}
MXL_PLUGIN_DIR=${MXL_PLUGIN_DIR:-$MXL_REPO/rust/target/debug}
MXL_RT_LIB_DIR=${MXL_RT_LIB_DIR:-$MXL_REPO/build/Linux-Clang-Debug/lib}

SOCK=/tmp/gst-nmos-rs-demo.sock
LOG_DIR=$(mktemp -d -t gst-nmos-rs-demo-XXXXXX)
DAEMON_LOG="$LOG_DIR/daemon.log"

# Shared MXL Domain (tmpfs-backed). All three Nodes operate inside
# it. Only used when DEMO_TRANSPORT=mxl; the bootstrap block below
# only creates the directory + `domain_def.json` in that case.
MXL_DOMAIN_ID=1ac254d9-c9be-475a-93a7-f80b9c1063a8
MXL_DOMAIN_PATH=/dev/shm/gst-nmos-rs-demo

# Three NMOS Nodes (one daemon, three node_seeds, three HTTP ports).
# Spaced 10 apart deliberately: nmos-cpp's settings normaliser
# auto-assigns each Node `ws_port = http_port + 1` (IS-07 events ws,
# currently disabled by libnvnmos) and
# `control_protocol_ws_port = http_port + 2` (IS-12 ncp ws). libnvnmos
# disables the NCP-WS listener too (see `src/nvnmos.cpp`), but if any
# of those ever come back, adjacent Nodes' auto-ports collide and
# whichever Node loses the race binds but never serves HTTP. 10-apart
# leaves room.
NODE1_SEED=demo-node1; NODE1_PORT=18011
NODE2_SEED=demo-node2; NODE2_PORT=18021
NODE3_SEED=demo-node3; NODE3_PORT=18031

# Per-flow identity. Each Flow has a transport-specific identifier
# used to wire Senders to their Receiver counterparts:
#   MXL:  `mxl-flow-id` (UUID), shared between sender + matching receivers.
#   UDP:  destination multicast group + port, joined by any receiver
#         that wants to subscribe.
# The MXL ids and UDP groups / ports stay stable here so the printed
# curl recipes are deterministic.
FLOW_VIDEO_NODE1=11111111-aaaa-1111-aaaa-111111111111
FLOW_AUDIO_NODE1=11111111-bbbb-1111-bbbb-111111111111
FLOW_VIDEO_NODE3=33333333-aaaa-3333-aaaa-333333333333
FLOW_AUDIO_NODE3=33333333-bbbb-3333-bbbb-333333333333

# UDP multicast groups + ports, one per flow. Picked from the
# AD-HOC III administratively-scoped block (232.0.0.0/8 — SSM)
# so they don't collide with any well-known group. Per-flow ports
# (rather than a single 5004 across the board) keep wireshark /
# tcpdump filters one-line and make per-flow log lines easy to
# tell apart.
FLOW_VIDEO1_GROUP=232.99.99.1;     FLOW_VIDEO1_PORT=5004
FLOW_AUDIO1_GROUP=232.99.99.2;     FLOW_AUDIO1_PORT=5006
FLOW_VIDEO_OUT_GROUP=232.99.99.3;  FLOW_VIDEO_OUT_PORT=5008
FLOW_AUDIO_OUT_GROUP=232.99.99.4;  FLOW_AUDIO_OUT_PORT=5010

# Essence caps. Chosen per transport because each transport has its
# own GStreamer raw-format vocabulary in this plugin:
#   MXL: v210 (10-bit 4:2:2 video) + F32LE (32-bit float audio) per
#        the gst-mxl-rs `video_data_sync` integration test reference.
#   UDP: UYVY (8-bit 4:2:2 video) + S24BE (L24 audio) per the
#        rtpvrawdepay / rtpL24depay common case. Resolution is
#        smaller for UDP so a multicast loopback run stays well
#        below 1 Gbps (UYVY 1280x720@25 ≈ 368 Mbps; UYVY 192x108@25
#        ≈ 8 Mbps).
case "$DEMO_TRANSPORT" in
    mxl)
        VIDEO_CAPS='video/x-raw,format=v210,width=1920,height=1080,framerate=60000/1001,interlace-mode=progressive'
        AUDIO_CAPS='audio/x-raw,format=F32LE,rate=48000,channels=2,layout=interleaved'
        ;;
    udp|udp2)
        VIDEO_CAPS='video/x-raw,format=UYVY,width=1280,height=720,framerate=25/1,interlace-mode=progressive'
        AUDIO_CAPS='audio/x-raw,format=S24BE,rate=48000,channels=2,layout=interleaved'
        ;;
    *)
        echo "[error] DEMO_TRANSPORT=$DEMO_TRANSPORT not recognised; use one of mxl, udp, udp2"
        exit 1
        ;;
esac

# ---- Per-transport property helpers --------------------------------
#
# `transport_sender_props <flow>` / `transport_receiver_props <flow>`
# print the transport-specific property fragment for one nmossink /
# nmossrc invocation. The flow name (one of `video1`, `audio1`,
# `video-out`, `audio-out`) is the script's stable handle for the
# four Flows in the demo topology. On MXL both sides emit the same
# `mxl-domain-id` / `mxl-domain-path` / `mxl-flow-id` triplet, which
# is enough for the daemon to wire a Sender to its matching Receivers.
# On UDP the sender side emits `destination-ip` / `destination-port`
# / `source-ip`, the receiver side emits `multicast-ip` /
# `destination-port` / `interface-ip` / `source-ip`.
#
# Output is one space-separated string. The caller inlines it via
# unquoted command substitution so bash word-splits it into individual
# `key=value` tokens for gst-launch.
transport_sender_props() {
    local flow=$1
    local flow_id= group= port=
    case "$DEMO_TRANSPORT" in
        mxl)
            case "$flow" in
                video1)     flow_id=$FLOW_VIDEO_NODE1 ;;
                audio1)     flow_id=$FLOW_AUDIO_NODE1 ;;
                video-out)  flow_id=$FLOW_VIDEO_NODE3 ;;
                audio-out)  flow_id=$FLOW_AUDIO_NODE3 ;;
                *) echo "[error] transport_sender_props: unknown flow $flow" >&2; return 1 ;;
            esac
            printf 'mxl-domain-id=%s mxl-domain-path=%s mxl-flow-id=%s' \
                "$MXL_DOMAIN_ID" "$MXL_DOMAIN_PATH" "$flow_id"
            ;;
        udp|udp2)
            case "$flow" in
                video1)     group=$FLOW_VIDEO1_GROUP;     port=$FLOW_VIDEO1_PORT ;;
                audio1)     group=$FLOW_AUDIO1_GROUP;     port=$FLOW_AUDIO1_PORT ;;
                video-out)  group=$FLOW_VIDEO_OUT_GROUP;  port=$FLOW_VIDEO_OUT_PORT ;;
                audio-out)  group=$FLOW_AUDIO_OUT_GROUP;  port=$FLOW_AUDIO_OUT_PORT ;;
                *) echo "[error] transport_sender_props: unknown flow $flow" >&2; return 1 ;;
            esac
            # Sender-side IS-05 endpoint properties: destination = the
            # multicast group this flow lands on; source-ip = the local
            # NIC the egress packets leave on (drives udpsink.bind-address
            # and the SDP `a=source-filter:` include-source).
            printf 'destination-ip=%s destination-port=%d source-ip=%s' \
                "$group" "$port" "$DEMO_NIC_IP"
            ;;
    esac
}

transport_receiver_props() {
    local flow=$1
    local group= port=
    case "$DEMO_TRANSPORT" in
        mxl)
            transport_sender_props "$flow"
            ;;
        udp|udp2)
            case "$flow" in
                video1)     group=$FLOW_VIDEO1_GROUP;     port=$FLOW_VIDEO1_PORT ;;
                audio1)     group=$FLOW_AUDIO1_GROUP;     port=$FLOW_AUDIO1_PORT ;;
                video-out)  group=$FLOW_VIDEO_OUT_GROUP;  port=$FLOW_VIDEO_OUT_PORT ;;
                audio-out)  group=$FLOW_AUDIO_OUT_GROUP;  port=$FLOW_AUDIO_OUT_PORT ;;
                *) echo "[error] transport_receiver_props: unknown flow $flow" >&2; return 1 ;;
            esac
            # Receiver-side IS-05 endpoint properties: multicast-ip =
            # the group to JOIN; interface-ip = the local NIC the join
            # is issued on (resolved to an iface name for
            # udpsrc.multicast-iface); source-ip = SSM include-source
            # (the sender's egress NIC, set equal to interface-ip in
            # the demo since all four senders live on this host).
            printf 'multicast-ip=%s destination-port=%d interface-ip=%s source-ip=%s' \
                "$group" "$port" "$DEMO_NIC_IP" "$DEMO_NIC_IP"
            ;;
    esac
}

# Transport-aware /active flow-identity summary for diagnostics.
# `transport_identity_from_active <side> <body>` reads the resource's
# `/active` JSON and prints a short flow-identity string suitable for
# `printf` columns.
#   side: `sender` or `receiver` (the IS-05 transport_params schemas
#         differ slightly across sides on RTP transports).
#   body: the raw JSON returned by /active.
# Output examples:
#   MXL:          `flow=11111111-aaaa-1111-aaaa-111111111111`
#   UDP sender:   `dest=232.99.99.1:5004`
#   UDP receiver: `join=232.99.99.1:5004`
transport_identity_from_active() {
    local side=$1 body=$2
    case "$DEMO_TRANSPORT" in
        mxl)
            local flow
            flow=$(jq -r '.transport_params[0].mxl_flow_id // "n/a"' <<< "$body" 2>/dev/null)
            printf 'flow=%s' "$flow"
            ;;
        udp|udp2)
            local addr port
            case "$side" in
                sender)
                    addr=$(jq -r '.transport_params[0].destination_ip // "n/a"' <<< "$body" 2>/dev/null)
                    port=$(jq -r '.transport_params[0].destination_port // "n/a"' <<< "$body" 2>/dev/null)
                    printf 'dest=%s:%s' "$addr" "$port"
                    ;;
                receiver)
                    addr=$(jq -r '.transport_params[0].multicast_ip // "n/a"' <<< "$body" 2>/dev/null)
                    port=$(jq -r '.transport_params[0].destination_port // "n/a"' <<< "$body" 2>/dev/null)
                    printf 'join=%s:%s' "$addr" "$port"
                    ;;
                *)
                    printf 'unknown-side=%s' "$side"
                    ;;
            esac
            ;;
    esac
}

# Sinks for Node 2. The defaults (`autoaudiosink` / `autovideosink`)
# Just Work on a desktop. On WSL or other headless setups
# `autoaudiosink` spends up to ~60 s probing PipeWire / PulseAudio /
# ALSA before falling back, which serialises against every other
# element in Node 2's pipeline (GStreamer's bin state-change is
# synchronous) and delays both Node 2 receivers' registration with
# the daemon by the same ~60 s. If the script prints
# "[poll] collect_urls: still missing after 90s: receivers/video2@…
# receivers/audio2@…" or similar, override:
#
#     AUDIO_SINK=fakesink VIDEO_SINK=fakesink \\
#         ./gst-nmos-rs-demo.sh
#
# (or `AUDIO_SINK=pulsesink`/`VIDEO_SINK=glimagesink` if your distro
# / WSLg needs an explicit element). `fakesink` skips the probe
# entirely; the data path still flows, it just isn't played back.
AUDIO_SINK=${AUDIO_SINK:-autoaudiosink}
VIDEO_SINK=${VIDEO_SINK:-autovideosink}

HOST=${HOST:-localhost}

# IS-04 / IS-05 versions to drive against. libnvnmos advertises all
# supported versions by default (IS-04 v1.0..v1.3, IS-05 v1.0..v1.2);
# we pick the highest of each here. IS-05 v1.2 specifically is the
# minimum for MXL: it's the version that introduced extensible
# `urn:x-nmos:transport:*` types in the transport_params schemas, so
# resources advertising `urn:x-nmos:transport:mxl` are downgrade-
# filtered out of v1.0 / v1.1 listings and their per-resource
# endpoints return nothing usable (master_enable / transport_params
# come back null), even though the resource exists in the model.
IS04_VERSION=v1.3
IS05_VERSION=v1.2

# ---- Bootstrap -----------------------------------------------------

echo "Logs: $LOG_DIR"
echo "Transport: $DEMO_TRANSPORT"

# Re-create the MXL Domain directory. Only needed when
# DEMO_TRANSPORT=mxl: the UDP / UDP2 transports go straight from
# `nmossink` to `udpsink` and don't touch `/dev/shm` at all.
if [[ "$DEMO_TRANSPORT" == mxl ]]; then
    rm -rf  "$MXL_DOMAIN_PATH"
    mkdir -p "$MXL_DOMAIN_PATH"
    cat > "$MXL_DOMAIN_PATH/domain_def.json" <<EOF
{"id":"$MXL_DOMAIN_ID","label":"gst-nmos-rs demo domain"}
EOF
fi

echo "[build] cargo build -p nvnmosd -p gst-nmos-rs"
NVNMOS_LIB_DIR="$LIB" \
    cargo build --manifest-path "$REPO/rust/Cargo.toml" -p nvnmosd -p gst-nmos-rs \
    > "$LOG_DIR/build.log" 2>&1 \
    || { echo "[build] failed; see $LOG_DIR/build.log"; exit 1; }

TARGET_DIR=${CARGO_TARGET_DIR:-$REPO/rust/target}
DAEMON_BIN="$TARGET_DIR/debug/nvnmosd"
PLUGIN_DIR="$TARGET_DIR/debug"
[[ -x "$DAEMON_BIN" ]]               || { echo "missing $DAEMON_BIN"; exit 1; }
[[ -f "$PLUGIN_DIR/libgstnmos.so" ]] || { echo "missing $PLUGIN_DIR/libgstnmos.so"; exit 1; }

# ---- Daemon --------------------------------------------------------

rm -f "$SOCK"
echo "[daemon] starting nvnmosd on $SOCK"
RUST_LOG=${RUST_LOG:-info} \
LD_LIBRARY_PATH="$LIB:${LD_LIBRARY_PATH:-}" \
    "$DAEMON_BIN" --uds "$SOCK" > "$DAEMON_LOG" 2>&1 &
declare -i DAEMON_PID=$!

# Per-pipeline PID globals. Each launcher (`launch_node1` etc) writes
# its background PID here; `teardown_pipeline` resets it to 0 once
# the gst-launch has exited. `cleanup` reads them at shutdown so we
# don't have to keep an append-only PIDS array in sync with restarts.
declare -i NODE1_PID=0
declare -i NODE2_PID=0
declare -i NODE3_VIDEO_PID=0
declare -i NODE3_AUDIO_PID=0
declare -i BARE_PREVIEW_PID=0

_cleanup_done=
cleanup() {
    # Idempotency guard: a second signal (e.g. user mashing Ctrl+C
    # because the first one hung) must not re-enter the kill+wait
    # sequence, or we end up bouncing between the inner `wait` and
    # the trap forever.
    [[ -n "$_cleanup_done" ]] && return
    _cleanup_done=1
    echo
    echo "[cleanup] stopping pipelines and daemon..."
    # SIGTERM first — `gst-launch -e` responds with graceful EOS,
    # `nvnmosd` tears its listeners down cleanly. If anyone is
    # wedged (e.g. a pipeline stuck flushing EOS through an idle
    # placeholder, or a daemon still serving an in-flight RPC) the
    # SIGKILL after a short grace period guarantees Ctrl+C reliably
    # exits the script instead of leaving the user with no choice
    # but Ctrl+Z (which in turn leaks daemon zombies parented to
    # the stopped script).
    local -a pids=()
    local p
    for p in "$DAEMON_PID" "$NODE1_PID" "$NODE2_PID" \
             "$NODE3_VIDEO_PID" "$NODE3_AUDIO_PID" "$BARE_PREVIEW_PID"; do
        (( p > 0 )) && pids+=("$p")
    done
    for p in "${pids[@]}"; do kill -TERM "$p" 2>/dev/null || true; done
    sleep 2
    for p in "${pids[@]}"; do kill -KILL "$p" 2>/dev/null || true; done
    wait 2>/dev/null
    rm -f "$SOCK"
    echo "[cleanup] logs retained at $LOG_DIR"
}
# Split traps: cleanup runs on EXIT (idempotent, so it runs exactly
# once whether we got here via INT/TERM-then-exit or natural exit).
# INT/TERM just `exit` with the conventional status so bash unwinds
# through the EXIT trap rather than re-entering cleanup on a second
# signal.
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for _ in $(seq 1 50); do
    [[ -S "$SOCK" ]] && break
    sleep 0.1
done
[[ -S "$SOCK" ]] || { echo "[daemon] socket did not appear"; cat "$DAEMON_LOG"; exit 1; }
echo "[daemon] ready (pid $DAEMON_PID)"

# ---- Common gst env for nmossrc / nmossink + inner mxl* ----------

export GST_PLUGIN_PATH="$PLUGIN_DIR:$MXL_PLUGIN_DIR${GST_PLUGIN_PATH:+:$GST_PLUGIN_PATH}"
export LD_LIBRARY_PATH="$LIB:$MXL_RT_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# ---- Per-pipeline lifecycle helpers --------------------------------
#
# Each `launch_*` function builds one of the demo's gst-launch
# pipelines and stashes its PID in a per-pipeline global so we can
# tear it down and relaunch it on demand from the interactive menu
# (see option `6) Restart a pipeline`). Restart is the key
# diagnostic for the disable/re-enable freeze: after Node 1 has been
# disabled+re-enabled via IS-05, tearing down Node 2 (or the bare
# preview) and relaunching it tells us whether a *fresh* consumer
# can attach to the recreated flow, isolating "old reader handle is
# stale" from "new flow files are broken".

# Rotate a per-pipeline log on relaunch so the new run's output
# isn't tail-mixed with the old. The old file is renamed to
# `<file>.<HHMMSS>` and the new launcher's `>` redirect creates a
# fresh empty `<file>`.
_rotate_log() {
    local f=$1
    [[ -f "$f" ]] || return 0
    mv "$f" "$f.$(date +%H%M%S)" 2>/dev/null || true
}

# Stop a pipeline by name-referenced PID variable. Pass the PID
# global by name (`NODE1_PID`, not `$NODE1_PID`); we set it back to
# 0 once the child has been reaped so a follow-up `launch_*` knows
# it's safe to spawn a replacement. `label` is just for the
# user-visible log line.
#
# Sends SIGINT (not SIGTERM) deliberately: `gst-launch-1.0 -e`
# installs a SIGINT handler that sends EOS through the pipeline and
# waits for clean shutdown -- which is what gives `mxlsink::stop()`
# (and through it, `libmxl`'s `releaseWriter` and the `Instance`
# destructor) a chance to call `deleteFlow`, removing the
# `.mxl-flow/` directory from `MXL_DOMAIN_PATH`. SIGTERM has no
# handler in gst-launch, so the kernel terminates the process
# immediately, the C++ destructors never run, and the flow files
# stay on disk with stale internal state. A subsequent `launch_*`
# of the same producer then opens those leaked files via
# `Instance::createOrOpenDiscreteFlowData`, and the new writer can't
# produce fresh grains -- which makes every consumer (existing
# nmossrc, a relaunched Node 2, even a fresh bare mxlsrc preview)
# see a frozen flow. SIGKILL after the 2s grace remains a safety
# net for the case where the pipeline is wedged and SIGINT-driven
# EOS can't drain.
teardown_pipeline() {
    local pid_var_name=$1 label=$2
    local -n pid_ref=$pid_var_name
    if (( pid_ref <= 0 )); then
        echo "[$label] not running"
        return
    fi
    if ! kill -0 "$pid_ref" 2>/dev/null; then
        # Reap exit status so the PID isn't a zombie.
        wait "$pid_ref" 2>/dev/null || true
        echo "[$label] already exited"
        pid_ref=0
        return
    fi
    echo "[$label] stopping (pid $pid_ref)"
    kill -INT "$pid_ref" 2>/dev/null || true
    local i
    for i in 1 2 3 4 5 6 7 8 9 10; do
        sleep 0.2
        kill -0 "$pid_ref" 2>/dev/null || break
    done
    if kill -0 "$pid_ref" 2>/dev/null; then
        echo "[$label] not responding to SIGINT, sending SIGKILL"
        kill -KILL "$pid_ref" 2>/dev/null || true
    fi
    wait "$pid_ref" 2>/dev/null || true
    pid_ref=0
}

# ---- Node 1: producer (one pipeline, 2 nmossinks) ----------------
#
# Node 1 is the always-on producer: auto-activate=true on both
# Senders so the inner data path (mxlsink on MXL; udpsink + RTP
# payloader on UDP) is built and the flows start writing as soon as
# the element reaches READY, no IS-05 PATCH required. The element
# calls SyncResourceState after the inner is up, so the daemon's
# /single/senders/{id}/active reports master_enable: true from the
# start.
launch_node1() {
    if (( NODE1_PID > 0 )) && kill -0 "$NODE1_PID" 2>/dev/null; then
        echo "[node1] already running (pid $NODE1_PID)"
        return 1
    fi
    echo "[node1] starting producer pipeline (audiotestsrc + videotestsrc -> 2 nmossinks)"
    _rotate_log "$LOG_DIR/node1-producer.log"
    GST_DEBUG=${GST_DEBUG:-nmossink:3} \
        gst-launch-1.0 -e \
            videotestsrc pattern=smpte horizontal-speed=2 is-live=true ! \
                $VIDEO_CAPS ! \
                nmossink \
                    daemon-uri="unix:$SOCK" \
                    transport="$DEMO_TRANSPORT" \
                    http-port="$NODE1_PORT" \
                    node-seed="$NODE1_SEED" \
                    sender-name=video1 \
                    $(transport_sender_props video1) \
                    caps="$VIDEO_CAPS" \
                    label="Node 1 / video1 (SMPTE bars, scrolling)" \
                    auto-activate=true \
            audiotestsrc wave=sine freq=440 is-live=true ! \
                $AUDIO_CAPS ! \
                nmossink \
                    daemon-uri="unix:$SOCK" \
                    transport="$DEMO_TRANSPORT" \
                    http-port="$NODE1_PORT" \
                    node-seed="$NODE1_SEED" \
                    sender-name=audio1 \
                    $(transport_sender_props audio1) \
                    caps="$AUDIO_CAPS" \
                    label="Node 1 / audio1 (440 Hz sine)" \
                    auto-activate=true \
        > "$LOG_DIR/node1-producer.log" 2>&1 &
    NODE1_PID=$!
}

# ---- Node 3: processors (two gst-launch processes, one Node) -----
#
# Node 3 is the controller-driven processor: auto-activate=false (the
# default) on both the Receiver and the Sender in each processor
# pipeline. The resources register on IS-04 immediately so an external
# controller can see them, but the data path stays on the placeholder
# (an `appsrc` configured with the user-supplied `caps`, so downstream
# negotiation completes and the pipeline can reach PLAYING) until the
# controller PATCHes both Connection API /staged endpoints (the
# inbound Receiver to attach to Node 1's flow, the outbound Sender to
# publish the processed flow). Until then the inner mxlsink / mxlsrc
# are not instantiated and no MXL writes / reads happen on this side.
# This shows off NMOS PATCH-driven activation vs. the eager
# `auto-activate=true` shortcut Node 1 and Node 2 use.

launch_node3_video() {
    if (( NODE3_VIDEO_PID > 0 )) && kill -0 "$NODE3_VIDEO_PID" 2>/dev/null; then
        echo "[node3-video] already running (pid $NODE3_VIDEO_PID)"
        return 1
    fi
    echo "[node3-video] starting video processor (nmossrc -> videoflip horizontal -> nmossink)"
    _rotate_log "$LOG_DIR/node3-video.log"
    GST_DEBUG=${GST_DEBUG:-nmossrc:3,nmossink:3} \
        gst-launch-1.0 -e \
            nmossrc \
                daemon-uri="unix:$SOCK" \
                transport="$DEMO_TRANSPORT" \
                http-port="$NODE3_PORT" \
                node-seed="$NODE3_SEED" \
                receiver-name=video-in \
                $(transport_receiver_props video1) \
                caps="$VIDEO_CAPS" \
                label="Node 3 / video-in" \
                auto-activate=false ! \
            videoconvert ! videoflip method=horizontal-flip ! videoconvert ! \
                nmossink \
                    daemon-uri="unix:$SOCK" \
                    transport="$DEMO_TRANSPORT" \
                    http-port="$NODE3_PORT" \
                    node-seed="$NODE3_SEED" \
                    sender-name=video-out \
                    $(transport_sender_props video-out) \
                    caps="$VIDEO_CAPS" \
                    label="Node 3 / video-out (horizontal flip of video1)" \
                    auto-activate=false \
        > "$LOG_DIR/node3-video.log" 2>&1 &
    NODE3_VIDEO_PID=$!
}

launch_node3_audio() {
    if (( NODE3_AUDIO_PID > 0 )) && kill -0 "$NODE3_AUDIO_PID" 2>/dev/null; then
        echo "[node3-audio] already running (pid $NODE3_AUDIO_PID)"
        return 1
    fi
    echo "[node3-audio] starting audio processor (nmossrc -> volume 0.3 -> nmossink)"
    _rotate_log "$LOG_DIR/node3-audio.log"
    GST_DEBUG=${GST_DEBUG:-nmossrc:3,nmossink:3} \
        gst-launch-1.0 -e \
            nmossrc \
                daemon-uri="unix:$SOCK" \
                transport="$DEMO_TRANSPORT" \
                http-port="$NODE3_PORT" \
                node-seed="$NODE3_SEED" \
                receiver-name=audio-in \
                $(transport_receiver_props audio1) \
                caps="$AUDIO_CAPS" \
                label="Node 3 / audio-in" \
                auto-activate=false ! \
            audioconvert ! volume volume=0.3 ! audioconvert ! \
                nmossink \
                    daemon-uri="unix:$SOCK" \
                    transport="$DEMO_TRANSPORT" \
                    http-port="$NODE3_PORT" \
                    node-seed="$NODE3_SEED" \
                    sender-name=audio-out \
                    $(transport_sender_props audio-out) \
                    caps="$AUDIO_CAPS" \
                    label="Node 3 / audio-out (volume 0.3 of audio1)" \
                    auto-activate=false \
        > "$LOG_DIR/node3-audio.log" 2>&1 &
    NODE3_AUDIO_PID=$!
}

# ---- Node 2: consumer (one pipeline, 2 nmossrcs) ------------------
#
# Node 2 is the always-on consumer: auto-activate=true on both
# Receivers so the inner mxlsrc is built at NULL→READY and the
# consumer attaches to Node 1's flows immediately (no IS-05 PATCH
# needed to play back). To re-route to Node 3's processed flows,
# PATCH the Connection API /staged endpoint of either Receiver with
# the corresponding Node 3 flow id (see the control commands printed
# at the end of this script).
launch_node2() {
    if (( NODE2_PID > 0 )) && kill -0 "$NODE2_PID" 2>/dev/null; then
        echo "[node2] already running (pid $NODE2_PID)"
        return 1
    fi
    echo "[node2] starting consumer pipeline (2 nmossrcs -> $AUDIO_SINK / $VIDEO_SINK)"
    _rotate_log "$LOG_DIR/node2-consumer.log"
    GST_DEBUG=${GST_DEBUG:-nmossrc:3} \
        gst-launch-1.0 -e \
            nmossrc \
                daemon-uri="unix:$SOCK" \
                transport="$DEMO_TRANSPORT" \
                http-port="$NODE2_PORT" \
                node-seed="$NODE2_SEED" \
                receiver-name=video2 \
                $(transport_receiver_props video1) \
                caps="$VIDEO_CAPS" \
                label="Node 2 / video2" \
                auto-activate=true ! \
                videoconvert ! "$VIDEO_SINK" sync=false \
            nmossrc \
                daemon-uri="unix:$SOCK" \
                transport="$DEMO_TRANSPORT" \
                http-port="$NODE2_PORT" \
                node-seed="$NODE2_SEED" \
                receiver-name=audio2 \
                $(transport_receiver_props audio1) \
                caps="$AUDIO_CAPS" \
                label="Node 2 / audio2" \
                auto-activate=true ! \
                audioconvert ! "$AUDIO_SINK" sync=false \
        > "$LOG_DIR/node2-consumer.log" 2>&1 &
    NODE2_PID=$!
}

# ---- Optional: bare `mxlsrc` preview (no nmossrc, no IS-04/IS-05) -
#
# Set `BARE_PREVIEW=1` to launch an extra pipeline that reads Node 1's
# video flow directly via `mxlsrc` and renders it. This is a sanity-
# check window: if the producer is writing grains, you should see live
# colour bars regardless of what's happening to the nmossrc-wrapped
# Node 2 receiver. Useful for isolating whether a "frozen video"
# symptom is mxlsrc/libmxl-level or nmossrc-level.
#
# Use VIDEO_SINK=autovideosink to see a window (the demo default),
# fakesink for headless capture. The file `bare-preview.log` will
# contain mxlsrc/basesrc/fakesink chatter.
launch_bare_preview() {
    if [[ "$DEMO_TRANSPORT" != mxl ]]; then
        echo "[preview] bare mxlsrc preview is MXL-only; skipping under DEMO_TRANSPORT=$DEMO_TRANSPORT"
        return 1
    fi
    if (( BARE_PREVIEW_PID > 0 )) && kill -0 "$BARE_PREVIEW_PID" 2>/dev/null; then
        echo "[preview] already running (pid $BARE_PREVIEW_PID)"
        return 1
    fi
    echo "[preview] starting bare mxlsrc -> $VIDEO_SINK"
    _rotate_log "$LOG_DIR/bare-preview.log"
    GST_DEBUG=${GST_DEBUG:-mxlsrc:5,basesrc:4} \
        gst-launch-1.0 -e \
            mxlsrc \
                domain="$MXL_DOMAIN_PATH" \
                video-flow-id="$FLOW_VIDEO_NODE1" \
                name=bare-preview ! \
            videoconvert ! "$VIDEO_SINK" sync=false \
        > "$LOG_DIR/bare-preview.log" 2>&1 &
    BARE_PREVIEW_PID=$!
}

# ---- Bring the demo up ---------------------------------------------

launch_node1
# Let Node 1 publish before consumers attach. On MXL the `mxlsrc`
# basesrc loop blocks in `create()` until the writer creates the
# domain's flow files; on UDP the udpsrc just sits on a quiet socket
# until packets arrive, so the wait is only really necessary for MXL,
# but a couple of seconds doesn't hurt either way.
sleep 2

launch_node3_video
launch_node3_audio
sleep 2

launch_node2

if [[ -n "${BARE_PREVIEW:-}" ]]; then
    launch_bare_preview
fi

# Brief breather to let the last gst-launch begin its NULL→READY
# (avoids hammering the daemon with IS-04 GETs the instant after
# fork). The actual readiness wait happens in `collect_urls` below.
sleep 1

# ---- Discover resource UUIDs via the IS-04 Node API ---------------
#
# Two parallel nmossinks / nmossrcs in one pipeline register in
# non-deterministic order, so we don't rely on daemon-log line order.
# Instead query each Node's IS-04 senders/receivers endpoints and
# match by `tags["urn:x-nvnmos:tag:name"]` (which gst-nmos-rs sets
# from sender-name / receiver-name). Requires `jq`.

if ! command -v jq >/dev/null 2>&1; then
    echo "[warn] 'jq' not found; the URL discovery and example PATCHes will be incomplete."
fi

# curl defaults to no timeout at all, which can wedge the whole demo
# script if the daemon's IS-04 / IS-05 HTTP server is briefly slow
# (e.g. another thread holding the model write_lock while a
# SyncResourceState is in flight). Cap each request at a few
# seconds so a slow response degrades to an empty URL rather than a
# hang. The IS-04 endpoints we hit are tiny JSON listings; if they
# don't come back in ~5 s something is wrong and the user is better
# off seeing an empty URL than a hung shell.
CURL_MAX_TIME=${CURL_MAX_TIME:-5}
CURL_CONNECT_TIMEOUT=${CURL_CONNECT_TIMEOUT:-2}

# Args: kind (senders|receivers), port, name. Echoes the connection
# /staged URL or empty string. Silent on any failure (HTTP timeout,
# empty listing, jq parse error) — the polling loop in
# `collect_urls` below is what reports a resource as "still
# missing" once the global WAIT_TIMEOUT has elapsed.
resource_url() {
    local kind=$1 port=$2 name=$3
    local body
    body=$(curl -sS \
        --max-time "$CURL_MAX_TIME" \
        --connect-timeout "$CURL_CONNECT_TIMEOUT" \
        "http://$HOST:$port/x-nmos/node/$IS04_VERSION/$kind" 2>/dev/null) || return
    local id
    id=$(echo "$body" \
        | jq -r --arg name "$name" \
            '.[] | select((.tags["urn:x-nvnmos:tag:name"] // []) | index($name)) | .id' \
        2>/dev/null | head -n 1)
    if [[ -n "$id" ]]; then
        echo "http://$HOST:$port/x-nmos/connection/$IS05_VERSION/single/$kind/$id"
    fi
}

# Wait until all expected resources are visible on IS-04, or
# WAIT_TIMEOUT elapses (default 90 s). One global deadline rather
# than per-resource so a slow first element doesn't burn the
# budget for everything that comes up alongside it. The most
# common slow-startup culprits we've seen:
#
#   * `autoaudiosink` spending up to ~60 s probing PipeWire /
#     PulseAudio / ALSA before falling back, on WSL or other
#     headless setups. Override `AUDIO_SINK=fakesink` (see the
#     leading-comment knobs) to skip that altogether. Because
#     GStreamer's bin state-change is synchronous, this delay
#     gates every other element in Node 2's pipeline — including
#     both `nmossrc`s — so neither the audio nor the video
#     receiver registers with the daemon until ~60 s in.
#   * `mxlsrc` opening Node 1's /dev/shm flow files and blocking
#     until they exist — fine in practice (Node 1 is started
#     first + `sleep 2`), but can add a few seconds under load.
#
# Each resource that resolves is recorded in `URLS[<key>]`;
# anything still missing at the deadline is logged once.
WAIT_TIMEOUT=${WAIT_TIMEOUT:-90}
declare -A EXPECTED=(
    [node1_sender_video]="senders|$NODE1_PORT|video1"
    [node1_sender_audio]="senders|$NODE1_PORT|audio1"
    [node2_receiver_video]="receivers|$NODE2_PORT|video2"
    [node2_receiver_audio]="receivers|$NODE2_PORT|audio2"
    [node3_receiver_video]="receivers|$NODE3_PORT|video-in"
    [node3_receiver_audio]="receivers|$NODE3_PORT|audio-in"
    [node3_sender_video]="senders|$NODE3_PORT|video-out"
    [node3_sender_audio]="senders|$NODE3_PORT|audio-out"
)
declare -A URLS=()
collect_urls() {
    local deadline=$(( SECONDS + WAIT_TIMEOUT ))
    local key triple kind port name url
    local -a missing
    while (( SECONDS < deadline )); do
        missing=()
        for key in "${!EXPECTED[@]}"; do
            [[ -n "${URLS[$key]:-}" ]] && continue
            triple=${EXPECTED[$key]}
            IFS='|' read -r kind port name <<< "$triple"
            url=$(resource_url "$kind" "$port" "$name")
            if [[ -n "$url" ]]; then
                URLS[$key]=$url
            else
                missing+=("$kind/$name@$port")
            fi
        done
        (( ${#missing[@]} == 0 )) && return 0
        sleep 0.5
    done
    echo "[warn] collect_urls: still missing after ${WAIT_TIMEOUT}s: ${missing[*]}" >&2
    echo "[warn]   (if Node 2 endpoints are stuck, try \`AUDIO_SINK=fakesink VIDEO_SINK=fakesink\` to skip the autoaudiosink/autovideosink probe; or bump \`WAIT_TIMEOUT\`)" >&2
    return 1
}
echo "[poll] waiting for all 8 IS-04 resources to register (timeout ${WAIT_TIMEOUT}s)..."
if ! collect_urls; then
    echo "[poll] FAILED: not all resources registered within ${WAIT_TIMEOUT}s" >&2
    exit 1
fi
echo "[poll] all resources visible"

if [[ -n "${DEMO_SMOKE:-}" ]]; then
    echo "[smoke] success; exiting (DEMO_SMOKE=1)"
    exit 0
fi

URL_NODE1_SENDER_VIDEO=${URLS[node1_sender_video]:-}
URL_NODE1_SENDER_AUDIO=${URLS[node1_sender_audio]:-}
URL_NODE2_RECEIVER_VIDEO=${URLS[node2_receiver_video]:-}
URL_NODE2_RECEIVER_AUDIO=${URLS[node2_receiver_audio]:-}
URL_NODE3_RECEIVER_VIDEO=${URLS[node3_receiver_video]:-}
URL_NODE3_RECEIVER_AUDIO=${URLS[node3_receiver_audio]:-}
URL_NODE3_SENDER_VIDEO=${URLS[node3_sender_video]:-}
URL_NODE3_SENDER_AUDIO=${URLS[node3_sender_audio]:-}

# ---- Per-transport heredoc substitutions --------------------------
#
# Pre-compute transport-keyed labels + PATCH bodies for the friendly
# summary heredoc and the example PATCHes that follow it. Keeping
# this here (rather than in the heredoc itself) keeps the heredoc a
# single substitution-only block and lets the conditional logic stay
# in one place. Both transports define every variable in this block.
case "$DEMO_TRANSPORT" in
    mxl)
        LABEL_FLOW_VIDEO1="flow $FLOW_VIDEO_NODE1"
        LABEL_FLOW_AUDIO1="flow $FLOW_AUDIO_NODE1"
        LABEL_FLOW_VIDEO_OUT="flow $FLOW_VIDEO_NODE3"
        LABEL_FLOW_AUDIO_OUT="flow $FLOW_AUDIO_NODE3"
        LABEL_INNER_CHAIN="mxlsink / mxlsrc"
        LABEL_PREWIRED="via mxl-flow-id"
        REROUTE_VIDEO_BODY="{\"transport_params\": [{\"mxl_flow_id\": \"$FLOW_VIDEO_NODE3\"}], \"master_enable\": true, \"activation\": {\"mode\": \"activate_immediate\"}}"
        REROUTE_AUDIO_BODY="{\"transport_params\": [{\"mxl_flow_id\": \"$FLOW_AUDIO_NODE3\"}], \"master_enable\": true, \"activation\": {\"mode\": \"activate_immediate\"}}"
        ;;
    udp|udp2)
        LABEL_FLOW_VIDEO1="dest $FLOW_VIDEO1_GROUP:$FLOW_VIDEO1_PORT"
        LABEL_FLOW_AUDIO1="dest $FLOW_AUDIO1_GROUP:$FLOW_AUDIO1_PORT"
        LABEL_FLOW_VIDEO_OUT="dest $FLOW_VIDEO_OUT_GROUP:$FLOW_VIDEO_OUT_PORT"
        LABEL_FLOW_AUDIO_OUT="dest $FLOW_AUDIO_OUT_GROUP:$FLOW_AUDIO_OUT_PORT"
        LABEL_INNER_CHAIN="udpsink + RTP payloader / udpsrc + RTP depayloader"
        LABEL_PREWIRED="via destination multicast group + port (interface $DEMO_NIC_IP)"
        REROUTE_VIDEO_BODY="{\"transport_params\": [{\"source_ip\": \"$DEMO_NIC_IP\", \"multicast_ip\": \"$FLOW_VIDEO_OUT_GROUP\", \"interface_ip\": \"$DEMO_NIC_IP\", \"destination_port\": $FLOW_VIDEO_OUT_PORT, \"rtp_enabled\": true}], \"master_enable\": true, \"activation\": {\"mode\": \"activate_immediate\"}}"
        REROUTE_AUDIO_BODY="{\"transport_params\": [{\"source_ip\": \"$DEMO_NIC_IP\", \"multicast_ip\": \"$FLOW_AUDIO_OUT_GROUP\", \"interface_ip\": \"$DEMO_NIC_IP\", \"destination_port\": $FLOW_AUDIO_OUT_PORT, \"rtp_enabled\": true}], \"master_enable\": true, \"activation\": {\"mode\": \"activate_immediate\"}}"
        ;;
esac

# ---- Friendly summary ---------------------------------------------

cat <<EOF

================================================================
gst-nmos-rs three-Node interactive demo  (transport=$DEMO_TRANSPORT)
================================================================

Topology:

  Node 1 (port $NODE1_PORT, seed $NODE1_SEED)  -- producer
    audiotestsrc(440Hz)  -> nmossink Sender audio1 ($LABEL_FLOW_AUDIO1)
    videotestsrc(smpte)  -> nmossink Sender video1 ($LABEL_FLOW_VIDEO1)

  Node 3 (port $NODE3_PORT, seed $NODE3_SEED)  -- processor (two gst processes)
    Receiver audio-in  --(volume 0.3)-->          Sender audio-out ($LABEL_FLOW_AUDIO_OUT)
    Receiver video-in  --(videoflip h-flip)-->   Sender video-out ($LABEL_FLOW_VIDEO_OUT)

  Node 2 (port $NODE2_PORT, seed $NODE2_SEED)  -- consumer
    Receiver audio2  -> $AUDIO_SINK
    Receiver video2  -> $VIDEO_SINK

Activation state out of the box:

  Node 1 / Node 2: \`auto-activate=true\` — the element brings its
    inner $LABEL_INNER_CHAIN up at NULL→READY and calls
    SyncResourceState so the daemon's /single/{senders,receivers}/{id}/active
    reports master_enable: true with no PATCH required. The Node 2
    Receivers are pre-wired to Node 1's flows $LABEL_PREWIRED,
    so audio + video are flowing already.

  Node 3: \`auto-activate=false\` — the Receiver+Sender pairs are
    registered (visible on IS-04) but the data path stays on the
    caps-aware placeholder (an idle \`appsrc\` advertising the
    user-supplied caps so downstream negotiation completes) until
    you PATCH /staged on the Connection API. Until then no real
    writes / reads happen on Node 3 even though Node 1 is publishing
    the input flows. PATCH the Receiver(s) below and optionally the
    Sender(s) to flip Node 3 into a live processor.

----------------------------------------------------------------
NMOS API roots:
  Node 1:  http://$HOST:$NODE1_PORT/x-nmos/node/$IS04_VERSION/self
  Node 2:  http://$HOST:$NODE2_PORT/x-nmos/node/$IS04_VERSION/self
  Node 3:  http://$HOST:$NODE3_PORT/x-nmos/node/$IS04_VERSION/self

Useful curl recipes (jq optional, --max-time recommended so a wedged
daemon doesn't hang your terminal):

  # List senders / receivers on a Node:
  curl -s --max-time 5 http://$HOST:$NODE1_PORT/x-nmos/node/$IS04_VERSION/senders   | jq '.[] | {id,label,flow_id}'
  curl -s --max-time 5 http://$HOST:$NODE2_PORT/x-nmos/node/$IS04_VERSION/receivers | jq '.[] | {id,label,subscription}'

  # Inspect a Receiver's /staged:
  curl -s --max-time 5 "$URL_NODE2_RECEIVER_VIDEO/staged" | jq

----------------------------------------------------------------
Resources discovered from $DAEMON_LOG
(Append /staged and PATCH to drive activations):

  Node 1 Sender video1:   $URL_NODE1_SENDER_VIDEO
  Node 1 Sender audio1:   $URL_NODE1_SENDER_AUDIO
  Node 2 Receiver video2: $URL_NODE2_RECEIVER_VIDEO
  Node 2 Receiver audio2: $URL_NODE2_RECEIVER_AUDIO
  Node 3 Receiver video-in: $URL_NODE3_RECEIVER_VIDEO
  Node 3 Receiver audio-in: $URL_NODE3_RECEIVER_AUDIO
  Node 3 Sender video-out: $URL_NODE3_SENDER_VIDEO
  Node 3 Sender audio-out: $URL_NODE3_SENDER_AUDIO

If any URL above is blank, the daemon did not register that resource
within WAIT_TIMEOUT=${WAIT_TIMEOUT}s (each IS-04 GET also capped at
CURL_MAX_TIME=${CURL_MAX_TIME}s). Re-run
\`grep -E "Created (sender|receiver):" $DAEMON_LOG\` to confirm,
then \`curl -sS --max-time 5 http://$HOST:<port>/x-nmos/node/$IS04_VERSION/senders\`
to re-check the listing.

The most common cause of a missing Node 2 URL is autoaudiosink
spending up to ~60 s probing PipeWire / PulseAudio / ALSA on
WSL / headless setups before falling back. Re-run with:

  AUDIO_SINK=fakesink VIDEO_SINK=fakesink ./$(basename "$0")

to skip the probe entirely; the data path still flows end-to-end,
it just isn't played back locally. Or bump WAIT_TIMEOUT in the
environment if a slow host needs more time at startup.

----------------------------------------------------------------
Example PATCHes (copy/paste):

  # ---- Activate Node 3 (Receiver+Sender on each side) ----
  # By default Node 3 is registered but inactive (auto-activate=false).
  # PATCH each /staged endpoint with master_enable=true to bring the
  # video processor live (the inner $LABEL_INNER_CHAIN instantiate
  # and data flows from Node 1 via Node 3's videoflip to Node 3's
  # Sender):
  curl -sS -X PATCH -H 'Content-Type: application/json' \\
    -d '{"master_enable": true, "activation": {"mode": "activate_immediate"}}' \\
    "$URL_NODE3_RECEIVER_VIDEO/staged"
  curl -sS -X PATCH -H 'Content-Type: application/json' \\
    -d '{"master_enable": true, "activation": {"mode": "activate_immediate"}}' \\
    "$URL_NODE3_SENDER_VIDEO/staged"

  # Same for the audio processor:
  curl -sS -X PATCH -H 'Content-Type: application/json' \\
    -d '{"master_enable": true, "activation": {"mode": "activate_immediate"}}' \\
    "$URL_NODE3_RECEIVER_AUDIO/staged"
  curl -sS -X PATCH -H 'Content-Type: application/json' \\
    -d '{"master_enable": true, "activation": {"mode": "activate_immediate"}}' \\
    "$URL_NODE3_SENDER_AUDIO/staged"

  # ---- Re-route Node 2 to Node 3's processed flows ----
  # Node 2's Receivers default to Node 1's flows. Once Node 3 is
  # active (above), switch Node 2 to consume Node 3's flipped /
  # attenuated outputs (the daemon synthesises a transport file from
  # the staged transport_params and the activation handler swaps the
  # inner receiver to the new $DEMO_TRANSPORT identity):
  curl -sS -X PATCH -H 'Content-Type: application/json' \\
    -d '$REROUTE_VIDEO_BODY' \\
    "$URL_NODE2_RECEIVER_VIDEO/staged"
  curl -sS -X PATCH -H 'Content-Type: application/json' \\
    -d '$REROUTE_AUDIO_BODY' \\
    "$URL_NODE2_RECEIVER_AUDIO/staged"

  # ---- Deactivate Node 2's video Receiver (back to placeholder) ----
  curl -sS -X PATCH -H 'Content-Type: application/json' \\
    -d '{"master_enable": false, "activation": {"mode": "activate_immediate"}}' \\
    "$URL_NODE2_RECEIVER_VIDEO/staged"

----------------------------------------------------------------
Logs:
  daemon:                 $DAEMON_LOG
  Node 1 producer:        $LOG_DIR/node1-producer.log
  Node 2 consumer:        $LOG_DIR/node2-consumer.log
  Node 3 video processor: $LOG_DIR/node3-video.log
  Node 3 audio processor: $LOG_DIR/node3-audio.log

Or drive activations via the interactive menu below — it can toggle
\`master_enable\` on any Sender or Receiver and subscribe a Receiver
to a particular Sender. Press Ctrl+C at any time to quit.
================================================================
EOF

# ---- Interactive control loop --------------------------------------
#
# Three actions, all targeting the IS-05 Connection API:
#
#   1. PATCH `master_enable=<true|false>` with `activate_immediate` on
#      any Sender or Receiver. Re-patching the same value is allowed
#      and useful — it re-runs the daemon's activation handler, which
#      `gst-nmos-rs` observes as an ActivationEvent and re-applies to
#      the inner data path.
#
#   2. Connect a Receiver to a particular Sender by GETting that
#      Sender's `/active`, copying its `transport_params` plus its id
#      onto the Receiver's `/staged` with `master_enable=true` +
#      `activate_immediate`. This is the IS-05 idiom for "subscribe
#      this Receiver to this Sender"; for our MXL setup the only
#      meaningful transport_params field is `mxl_flow_id`, but the
#      same code works for RTP or any other transport.
#
#   3. Dump compact `/active` state for every resource, resolving
#      each Receiver's `sender_id` back to its friendly name.

declare -a SENDER_LABELS=(
    "Node 1 / video1"
    "Node 1 / audio1"
    "Node 3 / video-out"
    "Node 3 / audio-out"
)
declare -a SENDER_URLS=(
    "$URL_NODE1_SENDER_VIDEO"
    "$URL_NODE1_SENDER_AUDIO"
    "$URL_NODE3_SENDER_VIDEO"
    "$URL_NODE3_SENDER_AUDIO"
)
declare -a RECEIVER_LABELS=(
    "Node 2 / video2"
    "Node 2 / audio2"
    "Node 3 / video-in"
    "Node 3 / audio-in"
)
declare -a RECEIVER_URLS=(
    "$URL_NODE2_RECEIVER_VIDEO"
    "$URL_NODE2_RECEIVER_AUDIO"
    "$URL_NODE3_RECEIVER_VIDEO"
    "$URL_NODE3_RECEIVER_AUDIO"
)

# Extract the UUID at the end of a connection-API resource URL like
#   http://host:port/x-nmos/connection/v1.2/single/senders/<uuid>
# Pure bash parameter expansion (strip optional trailing slash, then
# take the last path segment) so we don't depend on `sed` quirks --
# the previous BRE form used `\|` for alternation, which is a GNU
# extension and silently produces nothing on POSIX sed.
_id_from_url() { local u=${1%/}; echo "${u##*/}"; }

# Reverse lookup: sender UUID -> friendly label, so receiver state
# displays read "sender=Node 1 / video1" instead of an opaque UUID.
declare -A SENDER_ID_TO_NAME=()
for _i in "${!SENDER_LABELS[@]}"; do
    [[ -n "${SENDER_URLS[$_i]}" ]] || continue
    _id=$(_id_from_url "${SENDER_URLS[$_i]}")
    [[ -n "$_id" ]] && SENDER_ID_TO_NAME[$_id]=${SENDER_LABELS[$_i]}
done
unset _i _id

# Numbered-menu helper.
#   choose OUTVAR "Prompt:" "label1" "value1" "label2" "value2" ...
# Sets OUTVAR via name reference; returns 1 (OUTVAR untouched) on
# "b" / EOF so the caller can `|| return` cleanly. Internal vars are
# `__choose_*`-prefixed to avoid colliding with caller names passed
# in by name reference.
choose() {
    local -n __choose_out=$1; shift
    local __choose_prompt=$1; shift
    local -a __choose_labels=() __choose_values=()
    while (( $# >= 2 )); do
        __choose_labels+=("$1")
        __choose_values+=("$2")
        shift 2
    done
    local __choose_i __choose_ans
    while true; do
        echo
        echo "$__choose_prompt"
        for __choose_i in "${!__choose_labels[@]}"; do
            printf '  %d) %s\n' $((__choose_i+1)) "${__choose_labels[$__choose_i]}"
        done
        echo "  b) back"
        if ! read -r -p "> " __choose_ans; then
            echo
            return 1
        fi
        case "$__choose_ans" in
            b|B) return 1 ;;
            ''|*[!0-9]*) echo "Invalid choice." ;;
            *)
                if (( __choose_ans >= 1 && __choose_ans <= ${#__choose_labels[@]} )); then
                    __choose_out=${__choose_values[$((__choose_ans-1))]}
                    return 0
                fi
                echo "Invalid choice." ;;
        esac
    done
}

# Fill OUTVAR with alternating (label, url) pairs for entries that
# actually have a non-empty URL — i.e. resources that were
# discovered. Used to build the `choose` argument list while skipping
# anything missing from collect_urls.
_build_menu_args() {
    local -n __bma_out=$1
    local -n __bma_labels=$2
    local -n __bma_urls=$3
    __bma_out=()
    local __bma_i
    for __bma_i in "${!__bma_labels[@]}"; do
        [[ -n "${__bma_urls[$__bma_i]}" ]] || continue
        __bma_out+=("${__bma_labels[$__bma_i]}" "${__bma_urls[$__bma_i]}")
    done
}

# PATCH master_enable on a Sender or Receiver. `enable` is the raw
# JSON literal `true` or `false`. Echoes a multi-line trace
# (HTTP status, response body summary) so schema-validation failures
# don't disappear into a `[ok] master_enable=null` line.
patch_master_enable() {
    local url=$1 enable=$2
    local active_resp active_status active_body
    local patch resp status body resp_enable

    # We cannot PATCH `{master_enable, activation}` alone: nmos-cpp's default
    # IS-05 activation handler copies /staged onto /active on every
    # activate_immediate, so any fields we omit here are reset to whatever's
    # currently in /staged (typically null when the connection was brought
    # up out-of-band by nvnmosd's `auto-activate` -> `SyncResourceState`
    # path, which only writes /active). Read /active and restate the full
    # transport binding so the toggle preserves transport_params,
    # sender_id/receiver_id and transport_file.
    if ! active_resp=$(curl -sS \
            --max-time "$CURL_MAX_TIME" --connect-timeout "$CURL_CONNECT_TIMEOUT" \
            -w $'\n__HTTP_STATUS__:%{http_code}' \
            "$url/active" 2>&1); then
        echo "[error] GET $url/active failed: $active_resp"
        return 1
    fi
    active_status=${active_resp##*__HTTP_STATUS__:}
    active_body=${active_resp%$'\n'__HTTP_STATUS__:*}
    if [[ "$active_status" != 2* ]]; then
        echo "[error] GET $url/active returned HTTP $active_status:"
        echo "$active_body" | jq . 2>/dev/null || echo "$active_body"
        return 1
    fi

    # Start from /active, drop the previous activation result, override
    # master_enable, and request an immediate activation. Pick up
    # transport_params and sender_id/receiver_id verbatim if they're
    # present (senders carry receiver_id, receivers carry sender_id).
    #
    # Deliberately DO NOT echo transport_file back. Senders don't carry
    # one on /active (it lives on the separate /transportfile endpoint)
    # so this would be a no-op for them anyway; MXL receivers have it
    # in /active as `{data:null, type:null}` (the schema field is always
    # present) but nmos-cpp's connection_api.cpp explicitly rejects any
    # PATCH that carries transport_file for an MXL receiver per BCP-007-03
    # (`Rejecting PATCH for MXL receiver with transport_file`, HTTP 400).
    # See https://specs.amwa.tv/bcp-007-03/. RTP receivers DO accept
    # `transport_file` PATCHes (it's the canonical IS-05 way to push an
    # SDP at them), but a simple `master_enable` toggle has no reason to
    # round-trip it, so the omission is fine for both transports.
    patch=$(jq -c --argjson enable "$enable" '
        {
            master_enable: $enable,
            activation: { mode: "activate_immediate" }
        }
        + (if has("transport_params") then { transport_params } else {} end)
        + (if has("sender_id")        then { sender_id }        else {} end)
        + (if has("receiver_id")      then { receiver_id }      else {} end)
    ' <<< "$active_body") || return 1

    if ! resp=$(curl -sS -X PATCH -H 'Content-Type: application/json' \
            --max-time "$CURL_MAX_TIME" --connect-timeout "$CURL_CONNECT_TIMEOUT" \
            -w $'\n__HTTP_STATUS__:%{http_code}' \
            -d "$patch" "$url/staged" 2>&1); then
        echo "[error] PATCH $url/staged transport failed: $resp"
        return 1
    fi
    status=${resp##*__HTTP_STATUS__:}
    body=${resp%$'\n'__HTTP_STATUS__:*}
    if [[ "$status" != 2* ]]; then
        echo "[error] PATCH $url/staged returned HTTP $status:"
        echo "$body" | jq . 2>/dev/null || echo "$body"
        return 1
    fi
    resp_enable=$(jq -r 'if has("master_enable") and .master_enable != null then (.master_enable | tostring) else "?" end' <<< "$body" 2>/dev/null)
    echo "[ok] master_enable=$resp_enable"
}

# Translate sender-side transport_params into receiver-side
# transport_params. The IS-05 schemas differ across sides on RTP
# transports: senders carry `destination_ip` / `source_ip` /
# `destination_port`, receivers carry `multicast_ip` / `source_ip`
# (SSM include-source) / `interface_ip` / `destination_port`. MXL
# uses the same field (`mxl_flow_id`) on both sides, so the
# translation is a pass-through.
#
# Echoes a JSON value that can be plugged into the receiver's
# `transport_params:` slot via `--argjson`.
subscription_transport_params() {
    local sender_transport_params=$1
    case "$DEMO_TRANSPORT" in
        mxl)
            printf '%s' "$sender_transport_params"
            ;;
        udp|udp2)
            jq -c \
                --arg iface_ip "$DEMO_NIC_IP" \
                '[ .[] | {
                    source_ip:         (.source_ip      // null),
                    multicast_ip:      (.destination_ip // null),
                    interface_ip:      $iface_ip,
                    destination_port:  (.destination_port // null),
                    rtp_enabled:       true
                } ]' <<< "$sender_transport_params"
            ;;
    esac
}

# Subscribe a Receiver to a Sender by lifting the Sender's `/active`
# transport_params onto the Receiver's `/staged` along with the
# Sender's id, master_enable=true, and an immediate activation. The
# Sender must be active (transport_params non-null); if it isn't,
# enable it first via action 2 above.
#
# Quiet on success: prints a single `[ok] ...` line summarising the
# Receiver's post-PATCH `/active`. Errors print the offending HTTP
# response so misconfigurations are diagnosable.
connect_receiver_to_sender() {
    local receiver_url=$1 sender_url=$2
    local sender_id sender_active sender_status sender_body
    local sender_params receiver_params patch resp status body
    local active_after active_status active_body
    local resp_enable resp_sender resp_identity friendly

    sender_id=$(_id_from_url "$sender_url")
    if [[ -z "$sender_id" ]]; then
        echo "[error] could not extract sender id from $sender_url"
        return 1
    fi

    if ! sender_active=$(curl -sS \
            --max-time "$CURL_MAX_TIME" --connect-timeout "$CURL_CONNECT_TIMEOUT" \
            -w $'\n__HTTP_STATUS__:%{http_code}' \
            "$sender_url/active" 2>&1); then
        echo "[error] GET $sender_url/active transport failed: $sender_active"
        return 1
    fi
    sender_status=${sender_active##*__HTTP_STATUS__:}
    sender_body=${sender_active%$'\n'__HTTP_STATUS__:*}
    if [[ "$sender_status" != 2* ]]; then
        echo "[error] GET $sender_url/active returned HTTP $sender_status:"
        echo "$sender_body" | jq . 2>/dev/null || echo "$sender_body"
        return 1
    fi
    sender_params=$(jq -c '.transport_params' <<< "$sender_body" 2>/dev/null)
    if [[ -z "$sender_params" || "$sender_params" == "null" ]]; then
        echo "[error] sender has no usable transport_params (is it enabled?)"
        echo "$sender_body" | jq . 2>/dev/null || echo "$sender_body"
        return 1
    fi
    receiver_params=$(subscription_transport_params "$sender_params") || {
        echo "[error] could not translate sender transport_params for the receiver"
        return 1
    }

    patch=$(jq -nc \
        --argjson params "$receiver_params" \
        --arg sender_id "$sender_id" \
        '{sender_id: $sender_id, transport_params: $params, master_enable: true, activation: {mode: "activate_immediate"}}') || return 1

    if ! resp=$(curl -sS -X PATCH -H 'Content-Type: application/json' \
            --max-time "$CURL_MAX_TIME" --connect-timeout "$CURL_CONNECT_TIMEOUT" \
            -w $'\n__HTTP_STATUS__:%{http_code}' \
            -d "$patch" "$receiver_url/staged" 2>&1); then
        echo "[error] PATCH $receiver_url/staged transport failed: $resp"
        return 1
    fi
    status=${resp##*__HTTP_STATUS__:}
    body=${resp%$'\n'__HTTP_STATUS__:*}
    if [[ "$status" != 2* ]]; then
        echo "[error] PATCH $receiver_url/staged returned HTTP $status:"
        echo "$body" | jq . 2>/dev/null || echo "$body"
        return 1
    fi

    if ! active_after=$(curl -sS \
            --max-time "$CURL_MAX_TIME" --connect-timeout "$CURL_CONNECT_TIMEOUT" \
            -w $'\n__HTTP_STATUS__:%{http_code}' \
            "$receiver_url/active" 2>&1); then
        echo "[error] GET $receiver_url/active transport failed: $active_after"
        return 1
    fi
    active_status=${active_after##*__HTTP_STATUS__:}
    active_body=${active_after%$'\n'__HTTP_STATUS__:*}
    if [[ "$active_status" != 2* ]]; then
        echo "[error] GET $receiver_url/active returned HTTP $active_status:"
        echo "$active_body" | jq . 2>/dev/null || echo "$active_body"
        return 1
    fi
    resp_enable=$(jq -r 'if has("master_enable") and .master_enable != null then (.master_enable | tostring) else "?" end' <<< "$active_body" 2>/dev/null)
    resp_sender=$(jq -r '.sender_id // "null"' <<< "$active_body" 2>/dev/null)
    resp_identity=$(transport_identity_from_active receiver "$active_body")
    friendly=${SENDER_ID_TO_NAME[$resp_sender]:-$resp_sender}
    echo "[ok] master_enable=$resp_enable sender=$friendly $resp_identity"
}

# Compact dump of /active for every discovered resource.
show_state() {
    local i lbl url body en sender_id identity friendly
    echo
    echo "Current /active state:"
    echo "  Senders:"
    for i in "${!SENDER_LABELS[@]}"; do
        lbl=${SENDER_LABELS[$i]}
        url=${SENDER_URLS[$i]}
        if [[ -z "$url" ]]; then
            printf '    %-22s  (not discovered)\n' "$lbl"
            continue
        fi
        body=$(curl -sS --max-time "$CURL_MAX_TIME" --connect-timeout "$CURL_CONNECT_TIMEOUT" "$url/active" 2>/dev/null) || true
        if [[ -z "$body" ]]; then
            printf '    %-22s  (HTTP error)\n' "$lbl"
            continue
        fi
        en=$(jq -r '.master_enable' <<< "$body" 2>/dev/null)
        identity=$(transport_identity_from_active sender "$body")
        printf '    %-22s  master_enable=%-5s  %s\n' "$lbl" "$en" "$identity"
    done
    echo "  Receivers:"
    for i in "${!RECEIVER_LABELS[@]}"; do
        lbl=${RECEIVER_LABELS[$i]}
        url=${RECEIVER_URLS[$i]}
        if [[ -z "$url" ]]; then
            printf '    %-22s  (not discovered)\n' "$lbl"
            continue
        fi
        body=$(curl -sS --max-time "$CURL_MAX_TIME" --connect-timeout "$CURL_CONNECT_TIMEOUT" "$url/active" 2>/dev/null) || true
        if [[ -z "$body" ]]; then
            printf '    %-22s  (HTTP error)\n' "$lbl"
            continue
        fi
        en=$(jq -r '.master_enable' <<< "$body" 2>/dev/null)
        sender_id=$(jq -r '.sender_id // "null"' <<< "$body" 2>/dev/null)
        identity=$(transport_identity_from_active receiver "$body")
        friendly=${SENDER_ID_TO_NAME[$sender_id]:-$sender_id}
        printf '    %-22s  master_enable=%-5s  sender=%-22s  %s\n' "$lbl" "$en" "$friendly" "$identity"
    done
}

menu_set_sender_enable() {
    local -a args
    _build_menu_args args SENDER_LABELS SENDER_URLS
    if (( ${#args[@]} == 0 )); then
        echo "No Senders discovered."
        return
    fi
    local snd= en=
    choose snd "Pick a Sender:" "${args[@]}" || return
    choose en "Set master_enable to:" \
        "true  (enable)"  "true" \
        "false (disable)" "false" \
        || return
    patch_master_enable "$snd" "$en"
}

menu_set_receiver_enable() {
    local -a args
    _build_menu_args args RECEIVER_LABELS RECEIVER_URLS
    if (( ${#args[@]} == 0 )); then
        echo "No Receivers discovered."
        return
    fi
    local rcvr= en=
    choose rcvr "Pick a Receiver:" "${args[@]}" || return
    choose en "Set master_enable to:" \
        "true  (enable)"  "true" \
        "false (disable)" "false" \
        || return
    patch_master_enable "$rcvr" "$en"
}

menu_connect_receiver_to_sender() {
    local -a rargs sargs
    _build_menu_args rargs RECEIVER_LABELS RECEIVER_URLS
    _build_menu_args sargs SENDER_LABELS SENDER_URLS
    if (( ${#rargs[@]} == 0 )); then echo "No Receivers discovered."; return; fi
    if (( ${#sargs[@]} == 0 )); then echo "No Senders discovered.";  return; fi
    local rcvr= snd=
    choose rcvr "Pick a Receiver:"               "${rargs[@]}" || return
    choose snd  "Pick a Sender to connect it to:" "${sargs[@]}" || return
    connect_receiver_to_sender "$rcvr" "$snd"
}

# ---- Diagnostics ---------------------------------------------------
#
# `diag_snapshot` captures, in one pass, every surface that matters
# when investigating a freeze:
#
#   * Every Sender + Receiver `/active` and `/staged` as full JSON
#     (so a later `diff` between snapshot dirs is a one-liner).
#   * `ls -la $MXL_DOMAIN_PATH` plus a per-flow file inventory
#     (grain count, newest grain filename). We deliberately do NOT
#     report mtime-based liveness: libmxl writes grains via mmap on
#     tmpfs which does not bump the file mtime on every write, so any
#     "STALLED / advanced" indicator built from `stat -c %Y` is
#     misleading. The audio flow doesn't even use the `grains/`
#     subdirectory layout (its on-disk shape is just `data` +
#     `channels`), so it would always look stalled.
#   * The tail of every node + daemon log.
#
# All artefacts go under `$LOG_DIR/diag/NN-<label>/`. Stdout gets a
# compact per-resource summary so a quick eyeball doesn't require
# opening files.
DIAG_ROOT="$LOG_DIR/diag"
declare -i DIAG_COUNTER=0

# Turn a resource label like `Node 1 / video1` into a filename-safe
# slug like `Node-1-video1` so the per-resource JSON dumps don't end
# up trying to write to non-existent subdirectories (`/` in a path
# is a directory separator) or filenames with awkward whitespace.
_diag_slug() {
    local s=$1
    s=${s//[[:space:]]/-}    # spaces -> dashes
    s=${s//\//-}             # slashes -> dashes
    s=$(printf '%s' "$s" | tr -s '-')  # collapse repeated dashes
    s=${s#-}                 # trim leading dash
    s=${s%-}                 # trim trailing dash
    printf '%s' "$s"
}

diag_snapshot() {
    local label=${1:-snapshot}
    label=$(_diag_slug "$label")
    DIAG_COUNTER+=1
    local n
    printf -v n '%02d' "$DIAG_COUNTER"
    local outdir="$DIAG_ROOT/$n-$label"
    mkdir -p "$outdir"

    echo
    echo "[diag] snapshot $n-$label -> $outdir"

    # Per-resource /active + /staged dumps. Labels are slugified so
    # they're usable as file paths (the raw labels carry both `/` and
    # spaces, e.g. `Node 1 / video1`).
    local i lbl slug url
    for i in "${!SENDER_LABELS[@]}"; do
        lbl=${SENDER_LABELS[$i]}; url=${SENDER_URLS[$i]}
        [[ -z "$url" ]] && continue
        slug=$(_diag_slug "$lbl")
        curl -sS --max-time "$CURL_MAX_TIME" --connect-timeout "$CURL_CONNECT_TIMEOUT" \
            "$url/active" 2>/dev/null > "$outdir/sender-$slug-active.json" || true
        curl -sS --max-time "$CURL_MAX_TIME" --connect-timeout "$CURL_CONNECT_TIMEOUT" \
            "$url/staged" 2>/dev/null > "$outdir/sender-$slug-staged.json" || true
    done
    for i in "${!RECEIVER_LABELS[@]}"; do
        lbl=${RECEIVER_LABELS[$i]}; url=${RECEIVER_URLS[$i]}
        [[ -z "$url" ]] && continue
        slug=$(_diag_slug "$lbl")
        curl -sS --max-time "$CURL_MAX_TIME" --connect-timeout "$CURL_CONNECT_TIMEOUT" \
            "$url/active" 2>/dev/null > "$outdir/receiver-$slug-active.json" || true
        curl -sS --max-time "$CURL_MAX_TIME" --connect-timeout "$CURL_CONNECT_TIMEOUT" \
            "$url/staged" 2>/dev/null > "$outdir/receiver-$slug-staged.json" || true
    done

    # Compact stdout summary, sourced from the just-captured /active.
    local body en identity sender_id friendly
    echo "  Senders:"
    for i in "${!SENDER_LABELS[@]}"; do
        lbl=${SENDER_LABELS[$i]}; url=${SENDER_URLS[$i]}
        if [[ -z "$url" ]]; then printf '    %-22s  (not discovered)\n' "$lbl"; continue; fi
        slug=$(_diag_slug "$lbl")
        body=$(cat "$outdir/sender-$slug-active.json" 2>/dev/null || true)
        if [[ -z "$body" ]]; then printf '    %-22s  (HTTP error)\n' "$lbl"; continue; fi
        en=$(jq -r 'if has("master_enable") and .master_enable != null then (.master_enable | tostring) else "?" end' <<< "$body" 2>/dev/null)
        identity=$(transport_identity_from_active sender "$body")
        printf '    %-22s  master_enable=%-5s  %s\n' "$lbl" "$en" "$identity"
    done
    echo "  Receivers:"
    for i in "${!RECEIVER_LABELS[@]}"; do
        lbl=${RECEIVER_LABELS[$i]}; url=${RECEIVER_URLS[$i]}
        if [[ -z "$url" ]]; then printf '    %-22s  (not discovered)\n' "$lbl"; continue; fi
        slug=$(_diag_slug "$lbl")
        body=$(cat "$outdir/receiver-$slug-active.json" 2>/dev/null || true)
        if [[ -z "$body" ]]; then printf '    %-22s  (HTTP error)\n' "$lbl"; continue; fi
        en=$(jq -r 'if has("master_enable") and .master_enable != null then (.master_enable | tostring) else "?" end' <<< "$body" 2>/dev/null)
        sender_id=$(jq -r '.sender_id // "null"' <<< "$body" 2>/dev/null)
        identity=$(transport_identity_from_active receiver "$body")
        friendly=${SENDER_ID_TO_NAME[$sender_id]:-$sender_id}
        printf '    %-22s  master_enable=%-5s  sender=%-22s  %s\n' "$lbl" "$en" "$friendly" "$identity"
    done

    # MXL flow filesystem layout (cf. lib/internal/include/mxl-internal/FlowManager.hpp):
    #
    #   <domain>/<flow_id>.mxl-flow/
    #     flow_def.json   static NMOS Flow JSON, written once at create
    #     access          touched by readers (consumer-side activity)
    #     data            shared-memory Flow header
    #     grains/         per-grain shared-memory segments (video; producer-side)
    #     channels        audio-flavoured analogue of grains/data
    #
    # We deliberately do NOT report mtime-based liveness here: libmxl
    # writes via mmap on tmpfs and that does not bump the file mtime
    # per grain, so any "STALLED / advanced" indicator computed from
    # `stat -c %Y` is misleading. We report the static inventory only
    # (grain count + newest filename for video flows) and leave the
    # full `shm-ls-R.txt` on disk for follow-up inspection.
    # Only meaningful on MXL — UDP transports have no shared filesystem
    # state to inspect; the equivalent for those is the per-pipeline
    # log tails captured below.
    if [[ "$DEMO_TRANSPORT" == mxl ]]; then
        echo "  Flow directories ($MXL_DOMAIN_PATH):"
        ls -la "$MXL_DOMAIN_PATH" > "$outdir/shm-ls.txt" 2>/dev/null || true
        if [[ -d "$MXL_DOMAIN_PATH" ]]; then
            ls -laR "$MXL_DOMAIN_PATH" > "$outdir/shm-ls-R.txt" 2>/dev/null || true
            local d base grains_dir grain_count newest_grain
            shopt -s nullglob
            for d in "$MXL_DOMAIN_PATH"/*.mxl-flow; do
                base=${d##*/}
                grains_dir="$d/grains"
                grain_count=0
                newest_grain="(none)"
                if [[ -d "$grains_dir" ]]; then
                    local line
                    line=$(find "$grains_dir" -maxdepth 1 -type f -printf '%T@ %f\n' 2>/dev/null \
                           | sort -rn | head -1)
                    if [[ -n "$line" ]]; then
                        newest_grain=${line#* }
                    fi
                    grain_count=$(find "$grains_dir" -maxdepth 1 -type f 2>/dev/null | wc -l)
                    printf '    %s  grains=%d  newest=%s\n' \
                        "$base" "$grain_count" "$newest_grain"
                else
                    # Audio flows don't have a grains/ subdir; just list the
                    # files we do see so a missing/half-created flow is
                    # obvious without claiming any liveness.
                    local files
                    files=$(find "$d" -maxdepth 1 -type f -printf '%f ' 2>/dev/null)
                    printf '    %s  files=%s\n' "$base" "${files:-(none)}"
                fi
            done
            shopt -u nullglob
        else
            echo "    (directory absent)"
        fi
    fi

    # Log tails. Compact on stdout via a single one-line listing; full
    # tails captured per-file so they can be diff'd between snapshots.
    local TAIL_LINES=${DIAG_TAIL_LINES:-50}
    local log base
    local logs=(
        "$LOG_DIR/node1-producer.log"
        "$LOG_DIR/node2-consumer.log"
        "$LOG_DIR/node3-video.log"
        "$LOG_DIR/node3-audio.log"
        "$LOG_DIR/bare-preview.log"
        "$DAEMON_LOG"
    )
    echo "  Log tails (last $TAIL_LINES lines) -> $outdir/*-tail.txt"
    for log in "${logs[@]}"; do
        [[ -f "$log" ]] || continue
        base=${log##*/}
        tail -n "$TAIL_LINES" "$log" > "$outdir/$base-tail.txt" 2>/dev/null || true
    done
}

menu_diag_snapshot() {
    local label
    if ! read -r -p "Snapshot label (no spaces; default: snapshot): " label; then
        echo
        return
    fi
    diag_snapshot "${label:-snapshot}"
}

# Pipeline picker shared by the teardown and (re)launch menu items.
# Sets OUTVAR to one of node1 / node2 / node3_video / node3_audio /
# bare_preview, or returns 1 (OUTVAR untouched) on "b" / EOF so the
# caller can `|| return`.
#
# Teardown + (re)launch as separate operations supports the primary
# "fresh consumer after producer disable/re-enable" diagnostic:
# relaunching Node 2 (or the bare preview) after a disable+re-enable
# cycle of Node 1 tells us whether new mxlsrc instances can attach to
# the recreated flow (i.e. producer-side recreation is fine; the
# issue is stale reader handles), versus whether even fresh consumers
# can't attach (i.e. flow-file recreation itself is broken). Keeping
# the two as separate options also lets the user stop a pipeline,
# inspect with action 5 / shell tools, and then bring it back without
# a forced immediate relaunch.
_pick_pipeline() {
    local -n __pp_out=$1; shift
    local __pp_prompt=$1; shift
    local -a __pp_labels=() __pp_picks=()
    __pp_labels+=("Node 1 (producer)");        __pp_picks+=("node1")
    __pp_labels+=("Node 2 (consumer)");        __pp_picks+=("node2")
    __pp_labels+=("Node 3 video processor");   __pp_picks+=("node3_video")
    __pp_labels+=("Node 3 audio processor");   __pp_picks+=("node3_audio")
    # Always offer the bare preview: even if it wasn't launched at
    # startup (BARE_PREVIEW unset), the user can start one on demand
    # to A/B-test a fresh mxlsrc against the same producer.
    __pp_labels+=("Bare mxlsrc preview");      __pp_picks+=("bare_preview")

    local -a __pp_args=()
    local __pp_i
    for __pp_i in "${!__pp_labels[@]}"; do
        __pp_args+=("${__pp_labels[$__pp_i]}" "${__pp_picks[$__pp_i]}")
    done
    choose __pp_out "$__pp_prompt" "${__pp_args[@]}"
}

# SIGTERM (then SIGKILL after 2s) the selected pipeline. Idempotent:
# already-stopped pipelines just print a status line. The matching
# `launch_*` rotates the per-pipeline log on (re)launch so the next
# run's output doesn't tail-mix with the previous run's.
menu_teardown_pipeline() {
    local pick=
    _pick_pipeline pick "Pick a pipeline to tear down:" || return
    case "$pick" in
        node1)        teardown_pipeline NODE1_PID         "node1"        ;;
        node2)        teardown_pipeline NODE2_PID         "node2"        ;;
        node3_video)  teardown_pipeline NODE3_VIDEO_PID   "node3-video"  ;;
        node3_audio)  teardown_pipeline NODE3_AUDIO_PID   "node3-audio"  ;;
        bare_preview) teardown_pipeline BARE_PREVIEW_PID  "bare-preview" ;;
    esac
}

# Launch the selected pipeline. Idempotent: if the pipeline is
# already running, the `launch_*` function prints a status line and
# returns non-zero without spawning a duplicate (in which case we
# skip the post-launch "give it ~1-2s" hint, since nothing new
# started). To force a fresh process, tear it down first (action 6)
# and then launch (action 7).
menu_launch_pipeline() {
    local pick=
    _pick_pipeline pick "Pick a pipeline to launch:" || return
    local launched=0
    case "$pick" in
        node1)        launch_node1         && launched=1 ;;
        node2)        launch_node2         && launched=1 ;;
        node3_video)  launch_node3_video   && launched=1 ;;
        node3_audio)  launch_node3_audio   && launched=1 ;;
        bare_preview) launch_bare_preview  && launched=1 ;;
    esac
    (( launched )) && \
        echo "[hint] give the pipeline ~1-2s to re-register on IS-04 / open MXL flow"
}

interactive_loop() {
    if ! command -v jq >/dev/null 2>&1; then
        echo
        echo "[warn] 'jq' not found; the interactive menu builds JSON via jq."
        echo "        Install jq or use the copy/paste curl recipes above."
        echo "        Falling through to bare \`wait\` — Ctrl+C to quit."
        wait
        return
    fi
    local ans
    while true; do
        echo
        echo "================================================================"
        echo "Interactive control"
        echo "  1) Show current /active state"
        echo "  2) Set master_enable on a Sender   (immediate activation)"
        echo "  3) Set master_enable on a Receiver (immediate activation)"
        echo "  4) Connect a Receiver to a Sender  (transport_params + sender_id + master_enable=true)"
        echo "  5) Diagnostic snapshot             (/active + /staged + /dev/shm + log tails)"
        echo "  6) Tear down a pipeline            (SIGTERM the gst-launch process)"
        echo "  7) Launch a pipeline               (no-op if already running)"
        echo "  q) Quit"
        echo "================================================================"
        if ! read -r -p "> " ans; then
            echo
            return
        fi
        case "$ans" in
            1) show_state ;;
            2) menu_set_sender_enable ;;
            3) menu_set_receiver_enable ;;
            4) menu_connect_receiver_to_sender ;;
            5) menu_diag_snapshot ;;
            6) menu_teardown_pipeline ;;
            7) menu_launch_pipeline ;;
            q|Q) return ;;
            *) echo "Invalid choice." ;;
        esac
    done
}

interactive_loop
