#!/bin/sh

# SPDX-FileCopyrightText: Copyright (c) 2022-2024 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
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

set -e

apt update
apt install -y dbus avahi-daemon

[ -e "/etc/init.d/dbus" ] && /etc/init.d/dbus start || dbus-daemon --system --fork
[ -e "/etc/init.d/avahi-daemon" ] && /etc/init.d/avahi-daemon start || avahi-daemon --daemonize

tar -xvf ${PACKAGE_NAME}.tar.gz

export HOSTIP=`awk 'END{print $1}' /etc/hosts`

yes | LD_LIBRARY_PATH=${PACKAGE_NAME}/lib ${PACKAGE_NAME}/bin/nvnmos-example ${HOSTNAME}.local 8080 ${HOSTIP}

exec "$@"
