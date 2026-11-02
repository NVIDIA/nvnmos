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

# the injection point may be used to configure conan, but that requires CMake 3.24 or higher
# so avoid confusion and reject invocations which attempt to use it on lower versions
# see https://cmake.org/cmake/help/v3.24/variable/CMAKE_PROJECT_TOP_LEVEL_INCLUDES.html
# the alternative is to run conan install first instead
if(CMAKE_PROJECT_TOP_LEVEL_INCLUDES)
    cmake_minimum_required(VERSION 3.24 FATAL_ERROR)
else()
    cmake_minimum_required(VERSION 3.17 FATAL_ERROR)
endif()

# nvnmos.h declares the version
file(STRINGS nvnmos.h _nvnmos_version_defines REGEX "^#define NVNMOS_VERSION_[A-Z]+ ")
foreach(_nvnmos_version_define IN LISTS _nvnmos_version_defines)
    if(_nvnmos_version_define MATCHES "^#define NVNMOS_VERSION_([A-Z]+) \"?([^\"]*)\"?$")
        set(NVNMOS_VERSION_${CMAKE_MATCH_1} "${CMAKE_MATCH_2}")
    endif()
endforeach()
set(_nvnmos_version_parts "${NVNMOS_VERSION_MAJOR}.${NVNMOS_VERSION_MINOR}.${NVNMOS_VERSION_PATCH}")
if(NOT "${NVNMOS_VERSION_PRERELEASE}" STREQUAL "")
    string(APPEND _nvnmos_version_parts "-${NVNMOS_VERSION_PRERELEASE}")
endif()
if(NOT "${NVNMOS_VERSION_STRING}" STREQUAL "${_nvnmos_version_parts}")
    message(FATAL_ERROR "NVNMOS_VERSION_STRING \"${NVNMOS_VERSION_STRING}\" in nvnmos.h does not match version ${_nvnmos_version_parts}")
endif()

project(NvNmos VERSION ${NVNMOS_VERSION_MAJOR}.${NVNMOS_VERSION_MINOR}.${NVNMOS_VERSION_PATCH})

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
    if(BUILD_SHARED_LIBS)
        set(_nvnmos_pic_save "${CMAKE_POSITION_INDEPENDENT_CODE}")
        set(CMAKE_POSITION_INDEPENDENT_CODE ON)
    endif()
    add_subdirectory(${NMOS_CPP_DIRECTORY} build-nmos-cpp EXCLUDE_FROM_ALL)
    if(BUILD_SHARED_LIBS)
        set(CMAKE_POSITION_INDEPENDENT_CODE "${_nvnmos_pic_save}")
    endif()
endif()

# nvnmos library

set(NVNMOS_SOURCES
    nvnmos.cpp
    nvnmos_impl.cpp
    nvnmos_version.cpp
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

# According to semver, while the major version is 0, a minor release may break the ABI,
# so the soname and the installed package's compatibility include the minor version.
if(PROJECT_VERSION_MAJOR EQUAL 0)
    set(NVNMOS_SOVERSION ${PROJECT_VERSION_MAJOR}.${PROJECT_VERSION_MINOR})
    set(NVNMOS_VERSION_COMPATIBILITY SameMinorVersion)
else()
    set(NVNMOS_SOVERSION ${PROJECT_VERSION_MAJOR})
    set(NVNMOS_VERSION_COMPATIBILITY SameMajorVersion)
endif()
set_target_properties(nvnmos PROPERTIES
    VERSION ${PROJECT_VERSION}
    SOVERSION ${NVNMOS_SOVERSION}
    )

# The version reported by the library ends with build metadata, which is the Git
# commit, such as g0123abc. Moving HEAD reconfigures, so the commit stays current.
# A build without a Git checkout, such as a Docker build, can set the build
# metadata in the NVNMOS_VERSION_BUILD environment variable instead.
set(NVNMOS_VERSION "${NVNMOS_VERSION_STRING}")
set(NVNMOS_VERSION_BUILD "$ENV{NVNMOS_VERSION_BUILD}")
if(NOT NVNMOS_VERSION_BUILD)
    find_package(Git QUIET)
endif()
if(NOT NVNMOS_VERSION_BUILD AND GIT_FOUND)
    execute_process(
        COMMAND ${GIT_EXECUTABLE} rev-parse --short=7 HEAD
        WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
        OUTPUT_VARIABLE _nvnmos_git_commit
        OUTPUT_STRIP_TRAILING_WHITESPACE
        ERROR_QUIET
        )
    execute_process(
        COMMAND ${GIT_EXECUTABLE} rev-parse --path-format=absolute --git-path HEAD
        WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
        OUTPUT_VARIABLE _nvnmos_git_head
        OUTPUT_STRIP_TRAILING_WHITESPACE
        ERROR_QUIET
        )
    execute_process(
        COMMAND ${GIT_EXECUTABLE} symbolic-ref -q HEAD
        WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
        OUTPUT_VARIABLE _nvnmos_git_branch
        OUTPUT_STRIP_TRAILING_WHITESPACE
        ERROR_QUIET
        )
    if(_nvnmos_git_branch)
        execute_process(
            COMMAND ${GIT_EXECUTABLE} rev-parse --path-format=absolute --git-path ${_nvnmos_git_branch}
            WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
            OUTPUT_VARIABLE _nvnmos_git_branch_ref
            OUTPUT_STRIP_TRAILING_WHITESPACE
            ERROR_QUIET
            )
    endif()
    foreach(_nvnmos_git_file IN ITEMS "${_nvnmos_git_head}" "${_nvnmos_git_branch_ref}")
        if(_nvnmos_git_file AND EXISTS "${_nvnmos_git_file}")
            set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS "${_nvnmos_git_file}")
        endif()
    endforeach()
    if(_nvnmos_git_commit)
        set(NVNMOS_VERSION_BUILD "g${_nvnmos_git_commit}")
    endif()
endif()
if(NVNMOS_VERSION_BUILD)
    if(NOT NVNMOS_VERSION_BUILD MATCHES "^[0-9A-Za-z-]+(\\.[0-9A-Za-z-]+)*$")
        message(FATAL_ERROR "NVNMOS_VERSION_BUILD is not Semantic Versioning build metadata: ${NVNMOS_VERSION_BUILD}")
    endif()
    string(APPEND NVNMOS_VERSION "+${NVNMOS_VERSION_BUILD}")
    # Only this small file is recompiled when the build metadata changes.
    set_source_files_properties(nvnmos_version.cpp PROPERTIES
        COMPILE_DEFINITIONS "NVNMOS_VERSION_BUILD=\"${NVNMOS_VERSION_BUILD}\""
        )
endif()

# Windows reads the DLL's file and product version from a version resource.
if(WIN32 AND BUILD_SHARED_LIBS)
    configure_file(nvnmos.rc.in "${CMAKE_CURRENT_BINARY_DIR}/nvnmos.rc" @ONLY)
    target_sources(nvnmos PRIVATE "${CMAKE_CURRENT_BINARY_DIR}/nvnmos.rc")
endif()

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
