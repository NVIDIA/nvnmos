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
//! `AddReceiver`; READY→NULL closes it.
//!
//! Each `ActivationEvent` arriving on the subscription is routed
//! through the element: the daemon's activation task hands the event
//! to an element-supplied handler, the handler hops onto the
//! GStreamer thread via `Element::call_async`, re-runs the same
//! domain/flow cross-checks `validate_and_open` did at NULL→READY
//! (with the event's `transport_file` substituted in), and swaps the
//! inner element accordingly. Swaps at state ≥ PAUSED are gated on a
//! single-shot IDLE pad probe so the streaming thread is not inside
//! the inner element during the swap. The outcome (Applied / Failed)
//! is reported back to the daemon as the `AckActivation` `success` /
//! `failure_reason`.
//!
//! Inner data path: when the resolved configuration pins a Domain
//! path and a Flow id (plus a Flow format on the receiver), the bin
//! instantiates the real `mxlsink` / `mxlsrc` and ghosts its pad
//! through the bin's external pad. Otherwise it keeps a placeholder
//! `fakesink` / `fakesrc` so the element remains valid in the
//! pipeline until an IS-05 activation supplies the missing pieces.
//!
//! On `nmossink` there is also a *deferred mode*: if NULL→READY runs
//! with neither `transport-file*` nor `caps` set, the session is
//! opened without a resource and the actual `AddSender` is driven
//! from `change_state(ReadyToPaused)`. The ghost sink pad's upstream
//! peer is queried for caps, the result is fixated and fed to the
//! shared caps-driven flow_def builder, and on success the inner is
//! swapped to `mxlsink`. State-change errors propagate when peer
//! caps are ANY/EMPTY or unsupported by the builder so the user gets
//! a clear, pipeline-visible "declare `caps=…` or insert a
//! `capsfilter`" hint. Receiver-side deferred mode is intentionally
//! out of scope (no peer to query).
//!
//! `nmossrc` advertises essence caps on its ghost source pad
//! whenever a flow_def is in play (`transport-file*` at NULL→READY,
//! or the daemon-spliced internal transport_file at activation).
//! The flow_def is reverse-mapped via
//! [`flow_def::caps_from_flow_def`] and pinned by an internal
//! `mxlsrc ! capsfilter` chain so downstream caps queries see the
//! concrete shape the flow will carry — the canonical
//! `nmossrc ! transform ! nmossink` pipeline then resolves end-to-end
//! at READY→PAUSED: the deferred `nmossink`'s peer_query_caps lands
//! on the pinned caps and `AddSender` runs against the right
//! flow_def. When no transport_file is in play (development
//! convenience with properties only) the bare `mxlsrc` is used and
//! its broad pad template propagates.

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
