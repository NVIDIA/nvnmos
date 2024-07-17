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

include(GNUInstallDirs)

# if both variables aren't empty strings, join them
string(JOIN "/" NVNMOS_INSTALL_INCLUDEDIR ${CMAKE_INSTALL_INCLUDEDIR} ${NVNMOS_INCLUDE_PREFIX})

set(NVNMOS_INSTALL_LIBDIR "${CMAKE_INSTALL_LIBDIR}")
set(NVNMOS_INSTALL_BINDIR "${CMAKE_INSTALL_BINDIR}")
if(WIN32)
    string(APPEND NVNMOS_INSTALL_LIBDIR "/$<IF:$<CONFIG:Debug>,Debug,Release>")
    string(APPEND NVNMOS_INSTALL_BINDIR "/$<IF:$<CONFIG:Debug>,Debug,Release>")
endif()

# enable C++
enable_language(CXX)
# check C++11 or higher
if(CMAKE_CXX_STANDARD STREQUAL "98")
    message(FATAL_ERROR "CMAKE_CXX_STANDARD must be 11 or higher; C++98 is not supported")
endif()
if(NOT DEFINED CMAKE_CXX_STANDARD_REQUIRED)
    set(CMAKE_CXX_STANDARD_REQUIRED ON)
endif()
if(NOT DEFINED CMAKE_CXX_EXTENSIONS)
    set(CMAKE_CXX_EXTENSIONS OFF)
endif()

# location of additional CMake modules
list(APPEND CMAKE_MODULE_PATH
    ${CMAKE_CURRENT_SOURCE_DIR}/cmake
    )

# safeguards

if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE "Debug")
endif()

if(${PROJECT_SOURCE_DIR} STREQUAL ${PROJECT_BINARY_DIR})
    message(WARNING "In-source builds not recommended. Please make a new directory (called a build directory) and run CMake from there.")
endif()

string(TOLOWER "${CMAKE_BUILD_TYPE}" cmake_build_type_tolower)
string(TOUPPER "${CMAKE_BUILD_TYPE}" cmake_build_type_toupper)

if(NOT cmake_build_type_tolower STREQUAL "debug" AND
   NOT cmake_build_type_tolower STREQUAL "release" AND
   NOT cmake_build_type_tolower STREQUAL "relwithdebinfo" AND
   NOT cmake_build_type_tolower STREQUAL "minsizerel")
    message(FATAL_ERROR "Unknown build type \"${CMAKE_BUILD_TYPE}\". Allowed values are Debug, Release, RelWithDebInfo, MinSizeRel (case-insensitive).")
endif()
