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

# the injection point may be used to configure conan, but that requires CMake 3.24 or higher
# so avoid confusion and reject invocations which attempt to use it on lower versions
# see https://cmake.org/cmake/help/v3.24/variable/CMAKE_PROJECT_TOP_LEVEL_INCLUDES.html
# the alternative is to run conan install first instead
if(CMAKE_PROJECT_TOP_LEVEL_INCLUDES)
    cmake_minimum_required(VERSION 3.24 FATAL_ERROR)
else()
    cmake_minimum_required(VERSION 3.17 FATAL_ERROR)
endif()

project(NvNmos)

option(BUILD_SHARED_LIBS "Build using shared libraries" ON)
set(NVNMOS_BUILD_EXAMPLES ON CACHE BOOL "Build example applications")

# common config

include(cmake/NvNmosCommon.cmake)

# dependencies

# find and use nmos-cpp from its installed location or as a subdirectory
set(USE_ADD_SUBDIRECTORY OFF CACHE BOOL "Use add_subdirectory() rather than find_package()")
if(NOT USE_ADD_SUBDIRECTORY)
    message(STATUS "Using find_package(nmos-cpp)")

    # Using find_package() will require nmos-cpp and its dependencies (Boost, cpprestsdk, etc.)
    # to be installed and discoverable via a config-file package or find-module
    # Conan is one way of achieving this...

    find_package(nmos-cpp REQUIRED)
else()
    set(NMOS_CPP_DIRECTORY "../../nmos-cpp/Development" CACHE STRING "Path to nmos-cpp/Development directory")

    message(STATUS "Using add_subdirectory(${NMOS_CPP_DIRECTORY})")
    add_subdirectory(${NMOS_CPP_DIRECTORY} build-nmos-cpp EXCLUDE_FROM_ALL)
endif()

# nvnmos library

set(NVNMOS_SOURCES
    nvnmos.cpp
    nvnmos_impl.cpp
    )
set(NVNMOS_INTERFACE_HEADERS
    nvnmos.h
    )
set(NVNMOS_PRIVATE_HEADERS
    nvnmos_impl.h
    )
set(NVNMOS_HEADERS
    ${NVNMOS_INTERFACE_HEADERS}
    ${NVNMOS_PRIVATE_HEADERS}
    )

add_library(
    nvnmos
    ${NVNMOS_SOURCES}
    ${NVNMOS_HEADERS}
    )

source_group("Source Files" FILES ${NVNMOS_SOURCES})
source_group("Header Files" FILES ${NVNMOS_HEADERS})

if(BUILD_SHARED_LIBS)
    target_compile_definitions(
        nvnmos PRIVATE
        NVNMOS_EXPORTS
        )
    target_link_libraries(
        nvnmos PRIVATE
        nmos-cpp::compile-settings
        nmos-cpp::nmos-cpp
        )
else()
    target_compile_definitions(
        nvnmos PUBLIC
        NVNMOS_STATIC
        )
    target_link_libraries(
        nvnmos PUBLIC
        nmos-cpp::compile-settings
        nmos-cpp::nmos-cpp
        )
endif()

target_include_directories(nvnmos PUBLIC
        $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}>
        $<INSTALL_INTERFACE:${NVNMOS_INSTALL_INCLUDEDIR}>
    )

install(FILES ${NVNMOS_INTERFACE_HEADERS} DESTINATION ${NVNMOS_INSTALL_INCLUDEDIR})

list(APPEND NVNMOS_TARGETS nvnmos)
add_library(nvnmos::nvnmos ALIAS nvnmos)

if(NVNMOS_BUILD_EXAMPLES)
    # nvnmos-example executable

    set(NVNMOS_EXAMPLE_SOURCES
        main.c
        )
    set(NVNMOS_EXAMPLE_HEADERS
        )

    add_executable(
        nvnmos-example
        ${NVNMOS_EXAMPLE_SOURCES}
        ${NVNMOS_EXAMPLE_HEADERS}
        )

    source_group("Source Files" FILES ${NVNMOS_EXAMPLE_SOURCES})
    source_group("Header Files" FILES ${NVNMOS_EXAMPLE_HEADERS})
    
    target_link_libraries(
        nvnmos-example
        nvnmos
        )

    target_include_directories(nvnmos-example PRIVATE
        ${CMAKE_CURRENT_SOURCE_DIR}
        )

    list(APPEND NVNMOS_TARGETS nvnmos-example)
endif()

# export the config-file package

include(cmake/NvNmosExports.cmake)
