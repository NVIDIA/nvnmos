# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Shared helpers for GStreamer pipeline DOT capture + Graphviz PNG export.
# Source from gst-nmos-rs-demo.sh or generate-udp-video-pipeline-dots.sh:
#
#   source "$(dirname "${BASH_SOURCE[0]}")/pipeline-dots.sh"
#
# Export writes three files: {base}-raw.dot (GStreamer dump), {base}.dot
# (documentation-simplified), {base}.png. Set PIPELINE_DOT_SIMPLIFY=0 to
# skip simplification. PIPELINE_DOT_RANKDIR defaults to LR (horizontal).
# PIPELINE_DOT_DROP_DEFAULTS=1 (default) drops element properties that match
# gst-inspect-1.0 defaults; set to 0 to keep all non-debug props from the dump.
# PIPELINE_DOT_OMIT_EDGE_CAP_DENYLIST=1 (default) drops edge cap fields on a
# denylist (video metadata, layout=interleaved, any channel-mask).
# PIPELINE_DOT_OMIT_ELEM_PROP_DENYLIST=1 (default) drops element GObject props
# that are environment-specific (e.g. pulsesink device names) or runtime-only
# (e.g. GstQueue current-level-buffers/bytes/time at snapshot time).
# PIPELINE_DOT_COLLAPSE_BIN_TYPES=0 keeps every bin's inner subgraphs visible.
# Otherwise comma-separated GType names (Gst prefix optional): for each matching
# bin cluster, hide nested element subgraphs and keep only the outer shell (ghost
# pads, proxypads, public sink/src). Default in gst-nmos-rs-demo.sh:
# GstAutoVideoSink,GstAutoAudioSink. Add GstNmosSrc / GstNmosSink for a higher-level
# NMOS view. Prefix match works (GstAuto matches both auto*sinks).

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

# all: typical debug graph details; avoids FULL_PARAMS/VERBOSE default-property noise.
pipeline_dots_prepare_launch() {
    local slug=$1
    local dir
    dir=$(pipeline_dots_dir_for_slug "$slug")
    mkdir -p "$dir"
    export GST_DEBUG_DUMP_DOT_DIR=$dir
    export GST_DEBUG_BIN_TO_DOT=${GST_DEBUG_BIN_TO_DOT:-all}
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

# GstCapsFilter -> capsfilter (GStreamer element factory name).
pipeline_dots_gst_factory_name() {
    local gst_type=$1
    gst_type=${gst_type#Gst}
    gst_type=$(echo "$gst_type" | tr '[:upper:]' '[:lower:]')
    # Rust MXL plugin: GType GstRsMxlSrc/Sink, factory mxlsrc/mxlsink.
    case "$gst_type" in
        rsmxlsrc) gst_type=mxlsrc ;;
        rsmxlsink) gst_type=mxlsink ;;
    esac
    printf '%s\n' "$gst_type"
}

# Build factory|property|default lines from gst-inspect-1.0 for types in a DOT file.
pipeline_dots_build_defaults_cache() {
    local src=$1 cache=$2 typ factory
    : >"$cache"
    command -v gst-inspect-1.0 >/dev/null 2>&1 || return 0
    while IFS= read -r typ; do
        [[ -n "$typ" ]] || continue
        factory=$(pipeline_dots_gst_factory_name "$typ")
        gst-inspect-1.0 "$factory" 2>/dev/null | awk -v f="$factory" '
            /^  [a-zA-Z][a-zA-Z0-9_-]*[[:space:]]+:/ {
                p = $1
                sub(/:$/, "", p)
                prop = p
            }
            /Default:/ {
                line = $0
                sub(/^.*Default:[[:space:]]*/, "", line)
                gsub(/^[[:space:]]+|[[:space:]]+$/, "", line)
                gsub(/\|/, "_", line)
                # gst-inspect prints empty strings as ""; DOT uses description="".
                if (line == "\"\"") line = ""
                else if (line ~ /^".*"$/) {
                    sub(/^"/, "", line)
                    sub(/"$/, "", line)
                }
                if (prop != "") print f "|" prop "|" line
            }
        ' >>"$cache"
    done < <(grep -oE 'label="Gst[A-Za-z0-9]+' "$src" | sed 's/label="Gst//' | sort -u)
}

# Simplify GStreamer DOT for documentation (topology and inner bins unchanged).
# Reads path from argv[1], writes to stdout. Uses awk only — no invented caps.
pipeline_dots_simplify_for_docs() {
    local src=$1
    local awk_bin=awk
    local cache drop_defaults=${PIPELINE_DOT_DROP_DEFAULTS:-1}
    local omit_edge_cap_denylist=${PIPELINE_DOT_OMIT_EDGE_CAP_DENYLIST:-${PIPELINE_DOT_OMIT_EDGE_CAP_DEFAULTS:-1}}
    local omit_elem_prop_denylist=${PIPELINE_DOT_OMIT_ELEM_PROP_DENYLIST:-1}
    command -v gawk >/dev/null 2>&1 && awk_bin=gawk
    [[ -f "$src" ]] || return 1
    cache=$(mktemp)
    if [[ "$drop_defaults" == 1 ]]; then
        pipeline_dots_build_defaults_cache "$src" "$cache"
    else
        : >"$cache"
    fi
    "$awk_bin" -v defaults_file="$cache" -v drop_defaults="$drop_defaults" \
        -v omit_edge_cap_denylist="$omit_edge_cap_denylist" \
        -v omit_elem_prop_denylist="$omit_elem_prop_denylist" -f - "$src" <<'AWK'
BEGIN {
    skip_legend = 0
    omit_edge_cap_denylist = int(omit_edge_cap_denylist)
    omit_elem_prop_denylist = int(omit_elem_prop_denylist)
    while ((getline line < defaults_file) > 0) {
        n = split(line, a, "|")
        if (n >= 3) {
            f = a[1]
            p = a[2]
            d = a[3]
            for (i = 4; i <= n; i++)
                d = d "|" a[i]
            def_key = f SUBSEP p
            def[def_key] = d
        }
    }
    close(defaults_file)
}
function norm_val(v) {
    gsub(/\(int\)|\(uint\)|\(boolean\)|\(string\)|\(fraction\)/, "", v)
    gsub(/\\"/, "\"", v)
    if (v ~ /^".*"$/) {
        sub(/^"/, "", v)
        sub(/"$/, "", v)
    }
    if (tolower(v) == "null") return ""
    if (tolower(v) == "true" || v == "TRUE") return "true"
    if (tolower(v) == "false" || v == "FALSE") return "false"
    return tolower(v)
}
function prop_is_default(factory, p,   k, v, d, key) {
    if (drop_defaults != 1) return 0
    if (prop_key(p) == "caps") return 0
    k = prop_key(p)
    v = p
    sub(/^[^=]+=/, "", v)
    key = factory SUBSEP k
    if (!(key in def)) return 0
    d = def[key]
    return norm_val(v) == norm_val(d)
}
function keep_elem_prop(p) {
    if (p !~ /=/) return 0
    if (is_debug_prop(p)) return 0
    return 1
}
function is_state_token(s) {
    return (s ~ /^\[[-=>~0]\]$/)
}
function is_debug_prop(p) {
    if (p ~ /^last-sample=/) return 1
    if (p ~ /^sync=/) return 1
    if (p ~ /^async=/) return 1
    if (p ~ /0x[0-9a-fA-F]+/) return 1
    return 0
}
function prop_key(p,   k) {
    k = p
    sub(/=.*/, "", k)
    return k
}
function strip_gst_cap_types(s) {
    gsub(/\([a-zA-Z0-9_-]+\)/, "", s)
    gsub(/[[:space:]]+,/, ",", s)
    gsub(/,[[:space:]]+/, ", ", s)
    gsub(/^[[:space:]]+|[[:space:]]+$/, "", s)
    return s
}
function simplify_caps_prop(p,   caps) {
    if (p !~ /^caps=/) return p
    caps = strip_gst_cap_types(substr(p, 6))
    return "caps=" caps
}
# Element-property denylist: drop even when non-default (machine-specific, etc.).
function denylist_elem_prop_key(k) {
    if (!omit_elem_prop_denylist) return 0
    return (k == "device" || k == "current-device" || k == "device-name" \
        || k == "current-level-buffers" || k == "current-level-bytes" \
        || k == "current-level-time")
}
# Edge-cap denylist: omit uninteresting negotiated fields (never invent values).
function denylist_edge_cap_field(key, val) {
    if (!omit_edge_cap_denylist) return 0
    if (key == "channel-mask") return 1
    if (key == "layout" && val == "interleaved") return 1
    if (key == "multiview-mode" && val == "mono") return 1
    if (key == "pixel-aspect-ratio" && val == "1/1") return 1
    if (key == "interlace-mode" && val == "progressive") return 1
    return 0
}
function label_quote_end(line, start,   i, c) {
    i = start + 7
    while (i <= length(line)) {
        c = substr(line, i, 1)
        if (c == "\\") {
            i += 2
            continue
        }
        if (c == "\"") return i
        i++
    }
    return 0
}
function label_quoted_value(line,   p, end) {
    p = index(line, "label=\"")
    if (!p) return ""
    end = label_quote_end(line, p)
    if (!end) return ""
    return substr(line, p + 7, end - p - 7)
}
function set_label_quoted_value(line, new_val,   p, end) {
    p = index(line, "label=\"")
    if (!p) return line
    end = label_quote_end(line, p)
    if (!end) return line
    return substr(line, 1, p - 1) "label=\"" new_val "\"" substr(line, end + 1)
}
function split_gst_label(s, parts,   tmp) {
    gsub(/\\n/, "\034", s)
    return split(s, parts, "\034")
}
function gst_factory_name(gst_type,   f) {
    f = tolower(substr(gst_type, 4))
    if (f == "rsmxlsrc") return "mxlsrc"
    if (f == "rsmxlsink") return "mxlsink"
    return f
}
function simplify_elem_label(line,   s, n, parts, i, out, p, k, factory) {
    if (line !~ /label="Gst/ || line ~ /label="GstPipeline/) return line
    s = label_quoted_value(line)
    if (s == "") return line
    n = split_gst_label(s, parts)
    if (n < 2) return line
    factory = gst_factory_name(parts[1])
    out = parts[1] "\\n" parts[2]
    for (i = 3; i <= n; i++) {
        p = parts[i]
        if (p == "") continue
        if (is_state_token(p)) continue
        if (!keep_elem_prop(p)) continue
        if (prop_is_default(factory, p)) continue
        k = prop_key(p)
        if (denylist_elem_prop_key(k)) continue
        if (k == "caps") p = simplify_caps_prop(p)
        out = out "\\n" p
    }
    return set_label_quoted_value(line, out)
}
function simplify_pad_label(line,   m) {
    # sink, src, proxypadN, etc. — drop pad state/flag lines ([>][bfbE]…)
    if (match(line, /label="([^"\\]+)\\n\[[^"]*"/, m))
        sub(/label="[^"]*"/, "label=\"" m[1] "\"", line)
    return line
}
# Pad-edge caps: "video/x-raw\l  width: 1280\l ..." -> "video/x-raw\lwidth=1280\l..."
# (Graphviz \l lines; key=value like element caps, not comma-joined — commas break DOT).
function simplify_edge_caps_block(block,   lines, n, i, line, key, val, out, tmp) {
    if (block !~ /\\l/) return block
    tmp = block
    gsub(/\\l/, "\034", tmp)
    n = split(tmp, lines, "\034")
    out = lines[1]
    gsub(/^[[:space:]]+|[[:space:]]+$/, "", out)
    for (i = 2; i <= n; i++) {
        line = lines[i]
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", line)
        if (line == "") continue
        if (match(line, /^([^:]+):[[:space:]]*(.*)$/, m)) {
            key = m[1]
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", key)
            val = m[2]
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", val)
            val = strip_gst_cap_types(val)
            if (key != "" && val != "" && !denylist_edge_cap_field(key, val))
                out = out "\\l" key "=" val
        }
    }
    # Graphviz left-justifies lines with \l; the final line defaults to centred
    # on the edge unless the label also ends with \l (shows as a one-space shift).
    if (out ~ /\\l/) out = out "\\l"
    return out
}
function simplify_edge_labels(line,   pos, len, block, compact, repl) {
    pos = 1
    while (match(substr(line, pos), /\[label="([^"]*)"\]/, m)) {
        len = RLENGTH
        pos = pos + RSTART - 1
        block = m[1]
        compact = simplify_edge_caps_block(block)
        if (compact != block) {
            repl = "[label=\"" compact "\"]"
            line = substr(line, 1, pos - 1) repl substr(line, pos + len)
            pos = pos + length(repl)
        } else {
            pos = pos + len
        }
    }
    return line
}
function doc_layout(line) {
    sub(/rankdir=TB/, "rankdir=LR", line)
    if (line ~ /^  label="<GstPipeline>/ || line ~ /^  label="pipeline";$/) return ""
    return line
}
/^  legend \[/ { skip_legend = 1; next }
skip_legend {
    if (/^  \];$/) skip_legend = 0
    next
}
{
    line = doc_layout($0)
    if (line == "") next
    line = simplify_elem_label(line)
    line = simplify_pad_label(line)
    line = simplify_edge_labels(line)
    print line
}
AWK
    rm -f "$cache"
}

# Hide inner element subgraphs inside allowlisted bin types (shell-only view).
# Reads stdin, writes stdout.
pipeline_dots_collapse_inner_bins() {
    local patterns=${PIPELINE_DOT_COLLAPSE_BIN_TYPES:-0}
    case "$patterns" in
        '' | 0) cat && return 0 ;;
    esac
    local awk_bin=awk prog
    command -v gawk >/dev/null 2>&1 && awk_bin=gawk
    prog=$(mktemp)
    cat >"$prog" <<'COLLAPSE_AWK'
BEGIN {
    depth = 0
    collapse_depth = 0
    n_collapse = 0
    split(shell_types, type_pat, ",")
    for (i in type_pat)
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", type_pat[i])
}
function cluster_name_from_line(line,   m) {
    if (match(line, /subgraph cluster_([^ {]+)[[:space:]]*\{/, m))
        return m[1]
    return ""
}
function gst_type_from_label_line(line,   m) {
    if (match(line, /label="Gst([^"\\]+)/, m))
        return "Gst" m[1]
    return ""
}
function normalize_type_pat(p) {
    if (p !~ /^Gst/)
        return "Gst" p
    return p
}
function type_matches_allowlist(gst_type,   i, p) {
    if (gst_type == "") return 0
    for (i in type_pat) {
        if (type_pat[i] == "") continue
        p = normalize_type_pat(type_pat[i])
        if (gst_type == p || index(gst_type, p) == 1)
            return 1
    }
    return 0
}
function is_pad_cluster(name) {
    return (name ~ /_(sink|src)$/)
}
function should_collapse_cluster(name) {
    if (depth < 1) return 0
    if (shell_bin[depth] != 1) return 0
    if (is_pad_cluster(name)) return 0
    return 1
}
function register_collapsed(name,   stem) {
    stem = name
    sub(/_0x[0-9a-fA-F]+$/, "", stem)
    collapse_ids[++n_collapse] = stem
}
function references_collapsed(line,   i) {
    for (i = 1; i <= n_collapse; i++)
        if (index(line, collapse_ids[i]) > 0) return 1
    return 0
}
function brace_delta(line,   opens, closes) {
    opens = gsub(/{/, "&", line)
    closes = gsub(/}/, "&", line)
    return opens - closes
}
function clear_shell_bins_above(d,   i) {
    for (i = d + 1; i <= 128; i++)
        delete shell_bin[i]
}
function adjust_depth(d) {
    depth += d
    if (d < 0)
        clear_shell_bins_above(depth)
}
{
    line = $0
    d = brace_delta(line)
    if (collapse_depth == 0 && line ~ /label="Gst/) {
        gst_type = gst_type_from_label_line(line)
        if (type_matches_allowlist(gst_type))
            shell_bin[depth] = 1
        else
            delete shell_bin[depth]
    }
    if (collapse_depth == 0 && match(line, /subgraph cluster_/)) {
        name = cluster_name_from_line(line)
        if (name != "" && d > 0 && should_collapse_cluster(name)) {
            register_collapsed(name)
            collapse_depth = depth + 1
            adjust_depth(d)
            next
        }
    }
    if (collapse_depth > 0) {
        adjust_depth(d)
        if (depth < collapse_depth)
            collapse_depth = 0
        next
    }
    if (references_collapsed(line)) {
        adjust_depth(d)
        next
    }
    print line
    adjust_depth(d)
}
COLLAPSE_AWK
    "$awk_bin" -v shell_types="$patterns" -f "$prog" -
    rm -f "$prog"
}

# PIPELINE_DOT_SIMPLIFY=0 keeps a single .dot copy of the raw dump (legacy behaviour).
pipeline_dots_render_png() {
    local src=$1 dest_base=$2
    local dpi=${PIPELINE_DOT_DPI:-200}
    local rankdir=${PIPELINE_DOT_RANKDIR:-LR}
    local simplify=${PIPELINE_DOT_SIMPLIFY:-1}
    local render_src simplified
    [[ -f "$src" ]] || return 1

    if [[ "$simplify" == 1 ]]; then
        if [[ "$src" != "${dest_base}-raw.dot" ]]; then
            cp "$src" "${dest_base}-raw.dot"
        fi
        simplified=$(mktemp "${dest_base}.XXXXXX.dot")
        pipeline_dots_simplify_for_docs "$src" | pipeline_dots_collapse_inner_bins >"$simplified" || {
            rm -f "$simplified"
            return 1
        }
        mv "$simplified" "${dest_base}.dot"
        render_src=${dest_base}.dot
    else
        cp "$src" "${dest_base}.dot"
        render_src=${dest_base}.dot
    fi

    dot -Tpng -Gdpi="$dpi" -Grankdir="$rankdir" -Nfontsize=8 -Efontsize=7 \
        -o "${dest_base}.png" "$render_src"
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
    if [[ "${PIPELINE_DOT_SIMPLIFY:-1}" == 1 ]]; then
        echo "[pipeline-dots] ${dest_base}.png ${dest_base}.dot ${dest_base}-raw.dot (from ${picked##*/})"
    else
        echo "[pipeline-dots] ${dest_base}.png ${dest_base}.dot (from ${picked##*/})"
    fi
}
