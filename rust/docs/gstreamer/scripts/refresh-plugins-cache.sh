#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

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

python3 "$generator" \
  "$cache_file" \
  "$build_cache" \
  "$rust_root/target/release/libgstnmos.so" \
  "$rust_root/target/release/libgstavsynctest.so"

if ! cmp -s "$build_cache" "$cache_file"; then
  cp "$build_cache" "$cache_file"
  echo "Updated $cache_file"
else
  echo "Plugin cache unchanged"
fi
