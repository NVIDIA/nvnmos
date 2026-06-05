#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Run nvnmosd scale-smoke presets and append JSONL results.
#
# Usage:
#   ./rust/nvnmosd-bench/scripts/run-nvnmosd-scale-smoke.sh
#   PRESETS="medium large" ./rust/nvnmosd-bench/scripts/run-nvnmosd-scale-smoke.sh
#
# Bench nodes always use a dummy Registration API (DNS-SD disabled) so
# CloseSession is not dominated by nmos-cpp browse timeouts.
#
# Requires: built nvnmosd + nvnmosd-bench, routable interface IP when patches > 0.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
RUST_MANIFEST="$REPO/rust/Cargo.toml"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/rust/target}"
DAEMON_BIN="$TARGET_DIR/debug/nvnmosd"
BENCH_BIN="$TARGET_DIR/debug/nvnmosd-bench"
SOCK="${NVNMOSD_UDS:-/tmp/nvnmosd-scale-smoke.sock}"
BASE_HTTP_PORT="${NVNMOSD_BENCH_BASE_PORT:-18080}"
RESULTS_DIR="${RESULTS_DIR:-$REPO/rust/nvnmosd-bench/results}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="$RESULTS_DIR/$STAMP.jsonl"

# label nodes senders receivers sessions clients syncs patches
DEFAULT_PRESETS=(
    "small 1 10 10 1 1 1 10"
    "medium 1 100 100 1 1 10 100"
    "large 1 1000 1000 10 10 100 100"
    "xlarge 10 10000 10000 100 10 1000 1000"
)

PRESET_NAMES="${PRESETS:-small}"
read -r -a REQUESTED <<<"$PRESET_NAMES"
SELECTED=()
for name in "${REQUESTED[@]}"; do
    found=0
    for row in "${DEFAULT_PRESETS[@]}"; do
        if [[ "${row%% *}" == "$name" ]]; then
            SELECTED+=("$row")
            found=1
            break
        fi
    done
    if (( ! found )); then
        echo "unknown preset: $name (available: small medium large xlarge)" >&2
        exit 1
    fi
done
PRESET_ROWS=("${SELECTED[@]}")

mkdir -p "$RESULTS_DIR"
LIB="${NVNMOS_LIB_DIR:-$REPO/build}"
if [[ ! -f "$LIB/libnvnmos.so" ]]; then
    echo "missing $LIB/libnvnmos.so — build nvnmos in \$REPO/build first" >&2
    exit 1
fi
export NVNMOS_LIB_DIR="$LIB"
export LD_LIBRARY_PATH="$LIB:${LD_LIBRARY_PATH:-}"

# Fail fast if a prior nvnmosd (or other process) still owns the node HTTP port range.
max_nodes=1
for row in "${PRESET_ROWS[@]}"; do
    read -r _ nodes _ _ _ _ _ _ <<<"$row"
    if (( nodes > max_nodes )); then
        max_nodes=$nodes
    fi
done
for ((i = 0; i < max_nodes; i++)); do
    port=$((BASE_HTTP_PORT + i))
    if command -v ss >/dev/null 2>&1; then
        if ss -tln "sport = :$port" 2>/dev/null | grep -q LISTEN; then
            holder=$(ss -tlnp "sport = :$port" 2>/dev/null | head -1)
            echo "port $port already in use — free it before running (e.g. stale nvnmosd). $holder" >&2
            exit 1
        fi
    elif command -v nc >/dev/null 2>&1 && nc -z 127.0.0.1 "$port" 2>/dev/null; then
        echo "port $port already in use — free it before running (e.g. stale nvnmosd)" >&2
        exit 1
    fi
done

echo "[build] cargo build -p nvnmosd -p nvnmosd-bench (NVNMOS_LIB_DIR=$LIB)"
NVNMOS_LIB_DIR="$LIB" cargo build --manifest-path "$RUST_MANIFEST" -p nvnmosd -p nvnmosd-bench

rm -f "$SOCK"
echo "[daemon] $DAEMON_BIN --uds $SOCK (LD_LIBRARY_PATH includes $LIB)"
LD_LIBRARY_PATH="$LIB:${LD_LIBRARY_PATH:-}" NVNMOSD_UDS="$SOCK" "$DAEMON_BIN" --uds "$SOCK" &
DAEMON_PID=$!
cleanup() {
    kill -TERM "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    rm -f "$SOCK"
}
trap cleanup EXIT INT TERM

for _ in $(seq 1 50); do
    [[ -S "$SOCK" ]] && break
    sleep 0.1
done
[[ -S "$SOCK" ]] || { echo "daemon socket not ready: $SOCK" >&2; exit 1; }

echo "[results] $OUT"
for row in "${PRESET_ROWS[@]}"; do
    read -r label nodes senders receivers sessions clients syncs patches <<<"$row"
    echo "[bench] preset=$label nodes=$nodes senders=$senders receivers=$receivers sessions=$sessions clients=$clients syncs=$syncs patches=$patches"
    NVNMOSD_PID="$DAEMON_PID" NVNMOSD_UDS="$SOCK" \
        "$BENCH_BIN" \
        --label "$label" \
        --daemon-pid "$DAEMON_PID" \
        --uds "$SOCK" \
        --nodes "$nodes" \
        --senders "$senders" \
        --receivers "$receivers" \
        --sessions "$sessions" \
        --base-http-port "$BASE_HTTP_PORT" \
        --clients "$clients" \
        --syncs "$syncs" \
        --patches "$patches" \
        >>"$OUT"
done

echo "[done] wrote $(wc -l <"$OUT") lines to $OUT"
