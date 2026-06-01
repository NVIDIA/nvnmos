// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Raw FFI bindings to the C `libnvnmos` API.
//!
//! See [`../../../src/nvnmos.h`](../../../src/nvnmos.h) for the upstream C
//! API documentation. This crate is intentionally a thin `-sys` shim: it
//! exposes the C declarations verbatim and does no safety wrapping. The
//! safe `nvnmos` crate (sibling in this workspace) owns that
//! responsibility — almost all callers should depend on `nvnmos` instead
//! of consuming this crate directly.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
