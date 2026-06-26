// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GStreamer plugin `nmos`: `nmossrc` and `nmossink` elements that talk
//! to the `nvnmosd` NMOS daemon over gRPC.
//!
//! See [`doc/designs/nvnmosd/README.md`](../../../doc/designs/nvnmosd/README.md)
//! for the architecture. The elements declare their property surface
//! and run the session lifecycle: NULL→READY opens a session against
//! `nvnmosd`, subscribes to activations, and (when `transport-file`
//! is set) adds the Sender or Receiver via `AddSender` /
//! `AddReceiver`; READY→NULL closes it.
//!
//! Each `ActivationEvent` arriving on the subscription is routed
//! through the element: the daemon's activation task hands the event
//! to an element-supplied handler, the handler hops onto the
//! GStreamer thread via `Element::call_async`, derives the new inner
//! configuration from the event's transport file (the daemon's
//! post-IS-05-PATCH view is authoritative — element-level identity
//! properties are not consulted at activation time; the essence-shape
//! cross-check on `caps` vs the file's `format` still applies and an
//! incompatible shape is ack-failed), and swaps the inner element
//! accordingly. Swaps at state ≥ PAUSED are gated on a single-shot
//! IDLE pad probe so the streaming thread is not inside the inner
//! element during the swap. The outcome (Applied / Failed) is
//! reported back to the daemon as the `AckActivation` `success` /
//! `failure_reason`.
//!
//! Property override / cross-check at NULL→READY: identity and
//! cosmetic properties (`sender-name` / `receiver-name`, `mxl-flow-id`,
//! `mxl-domain-id`, `label`, `description`, `receiver-caps-mode`)
//! that overlap with the transport file's content **override** the
//! file — the element rewrites the matching field/tag before handing
//! it to the daemon. Essence-shape properties (`caps`,
//! `transport-caps`) are **cross-checked** against the file and
//! mismatch is a hard error. See `flow_def::splice_overrides` (MXL)
//! and `sdp::passthrough_with_overrides` (RTP/UDP) for the splice
//! mechanics and `rust/gst-nmos-rs/README.md` ("Property interaction
//! with `transport-file`") for the full property matrix.
//!
//! Inner data path: when the resolved configuration is complete for
//! the chosen `transport`, the bin is *capable* of running the real
//! inner transport chain. Whether it does so eagerly is controlled
//! by the `auto-activate` boolean property:
//!
//! - `auto-activate=false` (default, canonical NMOS): the element
//!   adds the Sender or Receiver to the daemon so it appears on
//!   IS-04 but leaves the inner on the fake chain. The daemon's
//!   `/single/{senders,receivers}/{id}/active` reports
//!   `master_enable: false` until an IS-05 PATCH activates it; the
//!   activation event then flows through `apply_activation` and
//!   swaps the inner.
//! - `auto-activate=true`: the element brings the inner up
//!   immediately from the resolved configuring transport file and
//!   calls [`session::sync_active`] (which dispatches the daemon's
//!   `SyncResourceState` RPC) so the daemon's IS-04/IS-05 view of
//!   the resource flips to active without requiring an IS-05
//!   controller. This is a development / no-controller shortcut.
//!
//! The toggle is orthogonal to where the configuring transport file
//! came from. The flow id may have been supplied by `mxl-flow-id` as a
//! plain property override, taken from the transport file's top-level
//! `id`, or produced by caps-driven synthesis (MXL `flow_def` or SDP,
//! depending on `transport`) — all three routes funnel into the same
//! gate.
//!
//! If the resolved configuration is incomplete for the chosen
//! `transport` (e.g. missing MXL domain path / flow id, or incomplete
//! RTP SDP / IS-05 endpoints), the element stays on the fake chain
//! regardless of `auto-activate` — the gate only upgrades configurations
//! that *could* run; it never invents missing pieces.
//!
//! Fake chain: while the inner is on the fake chain, the bin still
//! has to look like a valid GStreamer element to the rest of the
//! pipeline — the ghost pad needs to answer caps queries and the
//! bin needs to reach PLAYING. `nmossink`'s fake chain is
//! `capsfilter ! fakesink` when essence caps are known (`caps` or
//! `transport-file*`), or a bare `fakesink` until then. Known caps
//! are pinned at NULL→READY; deferred senders query upstream peer
//! caps to pin the fake chain at READY→PAUSED *before* child negotiation.
//! `nmossrc`'s fake chain is an `appsrc` configured with the
//! best-available essence caps (the user-supplied `caps` property,
//! or caps synthesised from `transport-file*`) and `is-live=true`;
//! we never push buffers into it, so its basesrc loop blocks idle
//! in `create()` and the bin holds at PLAYING waiting for an IS-05
//! activation to swap in the real inner source chain. When no caps
//! are yet available (constructed-time, before any properties have
//! been set) the appsrc is built without caps and downstream caps
//! negotiation will fail; the NULL→READY transition replaces it
//! with a caps-aware `appsrc` as soon as a caps source is
//! available.
//!
//! On `nmossink` there is also a *deferred mode*: if NULL→READY runs
//! with neither `transport-file*` nor `caps` set, the session is
//! opened without a resource and the actual `AddSender` is driven
//! from `change_state(ReadyToPaused)`. The ghost sink pad's upstream
//! peer is queried for caps, the result is fixated and used to pin
//! the fake chain before child negotiation, then fed to the
//! caps-driven transport-file builder (`flow_def` on MXL, SDP on
//! `udp` / `udp2` / `nvdsudp`), and on success the inner is swapped to
//! the real transport chain. State-change errors propagate when peer
//! caps are ANY/EMPTY or unsupported by the builder so the user gets
//! a clear, pipeline-visible "declare `caps=…` or insert a
//! `capsfilter`" hint. Receiver-side deferred mode is intentionally
//! out of scope (no peer to query).
//!
//! `nmossrc` pins essence caps on its ghost source pad from the
//! configuring transport file when the Receiver is *narrow* (BCP-004-01:
//! Receiver Caps are advertised on IS-04). *Wide* receivers advertise
//! no Receiver Caps; on MXL the `urn:x-nvnmos:tag:caps` flow_def tag
//! marks wide and bare `mxlsrc` is used so runtime caps come from the
//! filesystem flow_def; on RTP/UDP `a=x-nvnmos-caps:` marks wide and the
//! inner chain omits `capssetter` so runtime caps come from the live RTP
//! depay. Narrow receivers reverse-map the configuring file and pin
//! essence caps via an internal `capssetter` so downstream caps queries
//! see the concrete shape — the canonical `nmossrc ! transform !
//! nmossink` pipeline then resolves end-to-end at READY→PAUSED: the
//! deferred `nmossink`'s peer_query_caps lands on the pinned caps and
//! `AddSender` runs against the right configuring transport file.
//!
//! Receiver-side caps-driven synthesis is symmetric with the Sender
//! path: `nmossrc` with `caps` (no transport file) builds a
//! configuring transport file — MXL `flow_def` or SDP depending on
//! `transport`. For narrow receivers the daemon advertises BCP-004-01
//! Receiver Caps on IS-04 from that file; for wide receivers none are
//! advertised. `receiver-caps-mode` splices the wide/narrow marker:
//! `urn:x-nvnmos:tag:caps` on MXL, `a=x-nvnmos-caps:` on RTP/UDP.
//! The configuring file describes the Receiver at AddReceiver time;
//! IS-05 activation then supplies the real config. On MXL, the
//! activation transport file (typically synthesized by libnvnmos from
//! the PATCH) updates subscription fields such as `mxl-flow-id` without
//! changing the IS-04 caps advertisement. On RTP, activation
//! usually delivers a full activation SDP in the `ActivationEvent`; the
//! element builds the inner chain from that file — not from the
//! configuring SDP passed at AddReceiver. On MXL, when `mxl-flow-id` is
//! unset the element still calls AddSender / AddReceiver (the
//! synthesised `flow_def` omits top-level `id`) but keeps the fake
//! inner chain until IS-05 activation supplies the MXL flow id.

use std::sync::LazyLock;

use gst::glib;
use gstreamer as gst;

mod channel_mapping_session;
mod channel_mapping;
mod daemon;
mod domain;
mod essence_caps;
mod flow_def;
mod iface;
mod inner;
mod network_services;
mod nvdsudp;
mod nmossink;
mod nmossrc;
mod runtime;
mod sdp;
mod sdp_passthrough;
mod session;
mod nmosaudiochannelmap;
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
    nmosaudiochannelmap::register(plugin)?;
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
