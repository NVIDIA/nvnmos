#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

rust_root="$(cd "$(dirname "$0")/../../.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$rust_root/target}"

cd "$rust_root"
cargo build --release -p gst-nmos-rs -p gst-avsynctest-rs
