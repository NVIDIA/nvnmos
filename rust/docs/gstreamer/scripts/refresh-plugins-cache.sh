#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

if [[ ${1:-} != "" && ${1:-} != "--check" ]]; then
  echo "Usage: $0 [--check]" >&2
  exit 2
fi
check=${1:-}

doc_root="$(cd "$(dirname "$0")/.." && pwd)"
rust_root="$(cd "$doc_root/../.." && pwd)"
tools_dir="$doc_root/tools"
cache_file="$doc_root/plugins/gst_plugins_cache.json"
build_cache="$doc_root/build/gst_plugins_cache.json"
scanner="${GST_HOTDOC_PLUGINS_SCANNER:-$doc_root/build/gst-hotdoc-plugins-scanner}"
generator="$tools_dir/gst-plugins-doc-cache-generator.py"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$rust_root/target}"
"$doc_root/scripts/build-plugins.sh"

mkdir -p "$doc_root/build"
export GST_HOTDOC_PLUGINS_SCANNER="$scanner"
export GST_PLUGIN_PATH="$rust_root/target/release${GST_PLUGIN_PATH:+:$GST_PLUGIN_PATH}"
export MESON_BUILD_ROOT="$doc_root/build"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
export GST_REGISTRY="$tmp_dir/registry.bin"

"$scanner" "$tmp_dir/scanned.json" \
  "$rust_root/target/release/libgstnmos.so" \
  "$rust_root/target/release/libgstavsynctest.so"
python3 - "$tmp_dir/scanned.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as scanned_file:
    scanned = json.load(scanned_file)

missing = {"nmos", "avsynctest"} - scanned.keys()
if missing:
    raise SystemExit(f"Failed to scan GStreamer plugins: {', '.join(sorted(missing))}")
PY

cache_seed="$tmp_dir/gst_plugins_cache.json"
cp "$cache_file" "$cache_seed"
if [[ $check == "--check" ]]; then
  generated_cache="$tmp_dir/generated_gst_plugins_cache.json"
else
  generated_cache="$build_cache"
fi

python3 "$generator" \
  "$cache_seed" \
  "$generated_cache" \
  "$rust_root/target/release/libgstnmos.so" \
  "$rust_root/target/release/libgstavsynctest.so"

if cmp -s "$generated_cache" "$cache_file"; then
  echo "Plugin cache unchanged"
elif [[ $check == "--check" ]]; then
  echo "GStreamer plugin cache is stale; run $0" >&2
  diff -u "$cache_file" "$generated_cache" || true
  exit 1
else
  cp "$generated_cache" "$cache_file"
  echo "Updated $cache_file"
fi
