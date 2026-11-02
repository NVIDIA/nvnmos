/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

#include "nvnmos.h"

NVNMOS_API
bool nmos_get_version(
    NvNmosVersion* version)
{
    if (!version) return false;
    version->major = NVNMOS_VERSION_MAJOR;
    version->minor = NVNMOS_VERSION_MINOR;
    version->patch = NVNMOS_VERSION_PATCH;
    version->prerelease = NVNMOS_VERSION_PRERELEASE;
#ifdef NVNMOS_VERSION_BUILD
    version->string = NVNMOS_VERSION_STRING "+" NVNMOS_VERSION_BUILD;
#else
    version->string = NVNMOS_VERSION_STRING;
#endif
    return true;
}
