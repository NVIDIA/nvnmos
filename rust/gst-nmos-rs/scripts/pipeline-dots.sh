# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Shared helpers for GStreamer pipeline DOT capture + Graphviz PNG export.
# Source from gst-nmos-rs-demo.sh or generate-udp-video-pipeline-dots.sh:
#
#   source "$(dirname "${BASH_SOURCE[0]}")/pipeline-dots.sh"

# shellcheck shell=bash

pipeline_dots_require_graphviz() {
    command -v dot >/dev/null 2>&1 || {
        echo "[error] graphviz 'dot' not found (install the graphviz package)" >&2
        return 1
    }
}

pipeline_dots_dir_for_slug() {
    local slug=$1
    local root=${PIPELINE_DOT_ROOT:?PIPELINE_DOT_ROOT must be set}
    printf '%s/%s' "$root" "$slug"
}

# verbose: fuller negotiated caps on pad edges; GObject property strings may still ellipsize.
pipeline_dots_prepare_launch() {
    local slug=$1
    local dir
    dir=$(pipeline_dots_dir_for_slug "$slug")
    mkdir -p "$dir"
    export GST_DEBUG_DUMP_DOT_DIR=$dir
    export GST_DEBUG_BIN_TO_DOT=${GST_DEBUG_BIN_TO_DOT:-verbose}
}

# Ask a running gst-launch-1.0 to write a fresh snapshot (current topology).
pipeline_dots_request_snapshot() {
    local pid=$1
    if (( pid > 0 )) && kill -0 "$pid" 2>/dev/null; then
        kill -HUP "$pid" 2>/dev/null || true
        sleep 0.4
    fi
}

pipeline_dots_pick_latest() {
    local dir=$1
    local f
    for f in $(ls -t "$dir"/*.dot 2>/dev/null); do
        [[ -f "$f" ]] || continue
        printf '%s\n' "$f"
        return 0
    done
    return 1
}

pipeline_dots_render_png() {
    local src=$1 dest_base=$2
    local dpi=${PIPELINE_DOT_DPI:-200}
    [[ -f "$src" ]] || return 1
    cp "$src" "${dest_base}.dot"
    dot -Tpng -Gdpi="$dpi" -Nfontsize=8 -Efontsize=7 -o "${dest_base}.png" "$src"
    printf '%s\n' "${dest_base}.png"
}

# Export the pipeline's current graph (latest DOT after optional SIGHUP snapshot).
pipeline_dots_export_current() {
    local slug=$1 pid=${2:-0}
    local dir dest_base picked
    dir=$(pipeline_dots_dir_for_slug "$slug")
    if [[ ! -d "$dir" ]]; then
        echo "[error] no DOT dir $dir — (re)launch the pipeline (menu 7) first" >&2
        return 1
    fi
    pipeline_dots_request_snapshot "$pid"
    picked=$(pipeline_dots_pick_latest "$dir") || true
    if [[ -z "${picked:-}" ]]; then
        echo "[error] no DOT files in $dir" >&2
        return 1
    fi
    mkdir -p "${PIPELINE_DOT_ROOT}/export"
    dest_base="${PIPELINE_DOT_ROOT}/export/${slug}-$(date +%Y%m%d-%H%M%S)"
    pipeline_dots_render_png "$picked" "$dest_base" >/dev/null
    echo "[pipeline-dots] ${dest_base}.png (from ${picked##*/})"
}
