#!/usr/bin/bash

# SPDX-FileCopyrightText: Copyright (c) 2024-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
# http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

# Sourceable infrastructure setup for nvnmos containers.
#
# Starts dbus-daemon and avahi-daemon without systemd, without chroot, and
# without requiring CAP_SYS_CHROOT. Uses a dedicated Unix socket for D-Bus
# (exported as DBUS_SYSTEM_BUS_ADDRESS) so Avahi and libnvnmos can share a bus
# without the host system dbus. Provides a publish_mdns_hostname function.
#
# Usage:
#   source entrypoint-setup.sh
#
# Expects DBUS_SYSTEM_BUS_ADDRESS (e.g. set via Dockerfile ENV). Derived
# images that start D-Bus differently must override that variable.
#
# The caller should set shell options (e.g. set -euxo pipefail) before
# sourcing this script.
#
# This script sets a trap on EXIT/INT/TERM to clean up background processes.
# If the caller needs its own trap, it should call cleanup() from within it,
# e.g. trap 'my_cleanup; cleanup' EXIT INT TERM

pids=()

# Use a session bus and avoid daemonizing so that root privileges are not needed
dbus_address="${DBUS_SYSTEM_BUS_ADDRESS:-}"
case "$dbus_address" in
  unix:path=/*)
    dbus_path=${dbus_address#unix:path=}
    ;;
  "")
    echo "DBUS_SYSTEM_BUS_ADDRESS is not set" >&2
    echo "Expected: unix:path=/absolute/path" >&2
    exit 1
    ;;
  *)
    echo "Unsupported DBUS_SYSTEM_BUS_ADDRESS: $dbus_address" >&2
    echo "Expected: unix:path=/absolute/path" >&2
    exit 1
    ;;
esac
dbus_dir=$(dirname "$dbus_path")
mkdir -p "$dbus_dir"
rm -f "$dbus_path"
dbus-daemon --session --nofork --nopidfile --address="$dbus_address" &
pids+=($!)

for _ in {1..20}; do [ -S "$dbus_path" ] && break; sleep 0.1; done
[ -S "$dbus_path" ] || { echo "dbus-daemon failed to start"; exit 1; }

# Do not daemonize and run as current user so that CAP_SYS_CHROOT is not needed
# (running in the container provides sufficient isolation)
avahi-daemon --no-drop-root --no-chroot &
pids+=($!)

cleanup() {
    echo "Stopping background processes..."
    local pid
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait || true
}
trap cleanup EXIT INT TERM

publish_mdns_hostname() {
    local mdns_hostname="$1"
    echo "Publishing mDNS hostname: $mdns_hostname"
    while IFS= read -r ip; do
        [[ -n "$ip" ]] || continue
        echo "  Advertising $mdns_hostname at $ip"
        avahi-publish-address -R "$mdns_hostname" "$ip" &
        pids+=($!)
    done < <(hostname --all-ip-addresses | tr ' ' '\n')
}
