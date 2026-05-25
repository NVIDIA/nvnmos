// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GStreamer plugin `nmos`: `nmossrc` and `nmossink` elements that talk
//! to the `nvnmosd` NMOS daemon over gRPC.
//!
//! See [`doc/designs/nvnmosd/README.md`](../../../doc/designs/nvnmosd/README.md)
//! for the architecture. The elements declare their property surface
//! and run the session lifecycle: NULL→READY opens a session against
//! `nvnmosd`, subscribes to activations, and (when `transport-file`
//! is set) registers the Sender or Receiver via `AddSender` /
//! `AddReceiver`; READY→NULL closes it. The activation task acks
//! every event with `success=true`.
//!
//! Inner data path: when the resolved configuration pins a Domain
//! path and a Flow id (plus a Flow format on the receiver), the bin
//! instantiates the real `mxlsink` / `mxlsrc` and ghosts its pad
//! through the bin's external pad. Otherwise it keeps a placeholder
//! `fakesink` / `fakesrc` so the element remains valid in the
//! pipeline until a later step (caps→flow_def, IS-05 activation)
//! supplies the missing pieces.

use std::sync::LazyLock;

use gst::glib;
use gstreamer as gst;

mod daemon;
mod domain;
mod flow_def;
mod inner;
mod nmossink;
mod nmossrc;
mod runtime;
mod session;
mod types;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "nmos",
        gst::DebugColorFlags::empty(),
        Some("NMOS plugin (gst-nmos-rs)"),
    )
});

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    nmossink::register(plugin)?;
    nmossrc::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    nmos,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "Apache-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
