// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Process-wide tokio runtime shared by every `nmossrc` / `nmossink`
//! instance.
//!
//! gRPC calls live on this runtime; element state-change methods (which
//! run on GStreamer streaming threads, outside any tokio context) drive
//! them synchronously via [`block_on`].
//!
//! [`block_on`]: tokio::runtime::Runtime::block_on

use std::sync::LazyLock;

use tokio::runtime::Runtime;

pub(crate) static SHARED_RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("gst-nmos-rs")
        .build()
        .expect("building shared tokio runtime for gst-nmos-rs")
});
