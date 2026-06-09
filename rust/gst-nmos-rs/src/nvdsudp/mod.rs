// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! DeepStream `nvdsudpsrc` / `nvdsudpsink` helpers for `transport=nvdsudp`.

pub(crate) mod packetization;
pub(crate) mod sdp_file;
pub(crate) mod ts_refclk;
