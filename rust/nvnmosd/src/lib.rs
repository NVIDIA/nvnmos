// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Minimal library surface for in-crate tests. The `nvnmosd` binary implements
//! the gRPC service in `main.rs` (with its own private `mod` tree).

pub mod uds;
