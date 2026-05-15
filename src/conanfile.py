# SPDX-FileCopyrightText: Copyright (c) 2022-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
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

from conan import ConanFile
from conan.tools.cmake import CMakeDeps, CMakeToolchain

required_conan_version = ">=2.2"


class NvNmosConan(ConanFile):
    name = "nvnmos"
    description = "NVIDIA NMOS (Networked Media Open Specifications) Library"
    license = "Apache-2.0"
    url = "https://github.com/NVIDIA/nvnmos"
    homepage = "https://nvidia.github.io/nvnmos/"
    topics = ("amwa", "nmos", "is-04", "is-05", "broadcasting", "network", "media")

    settings = "os", "arch", "compiler", "build_type"
    options = {
        "nmos_cpp_from_source": [True, False],
    }
    default_options = {
        "nmos_cpp_from_source": False,
    }

    def requirements(self):
        if not self.options.nmos_cpp_from_source:
            self.requires("nmos-cpp/cci.20240223")
        else:
            # Based on Conan Center nmos-cpp/cci.20240223 direct requires.
            self.requires("boost/1.83.0", transitive_headers=True)
            self.requires("cpprestsdk/2.10.19", transitive_headers=True)
            self.requires("websocketpp/0.8.2")
            self.requires("openssl/[>=1.1 <4]")
            self.requires("json-schema-validator/2.3.0")
            self.requires("nlohmann_json/3.11.3")
            self.requires("jwt-cpp/0.7.0")
            if self.settings.os == "Linux":
                self.requires("avahi/0.8")
            elif self.settings.os == "Windows":
                self.requires("mdnsresponder/878.200.35")

    def generate(self):
        tc = CMakeToolchain(self)
        if self.options.nmos_cpp_from_source:
            tc.cache_variables["CMAKE_FIND_PACKAGE_PREFER_CONFIG"] = "ON"
            if self.settings.os == "Windows":
                # Match Conan Center nmos-cpp: use Conan's mdnsresponder instead of bundled DLL stub.
                tc.cache_variables["NMOS_CPP_USE_BONJOUR_SDK"] = "ON"
        tc.generate()
        CMakeDeps(self).generate()
