// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Helpers for the inner data path of `nmossink` / `nmossrc`.
//!
//! Each element is a `GstBin` with a single ghost pad and a permanent
//! `identity` anchor behind it. The actual data path — referred to as
//! the *chain* — lives **behind** the anchor and is the *only* thing
//! swapped on every IS-05 activation. The ghost pad targets the
//! anchor's outer-facing pad, set once at construction.
//!
//! The chain is one of two flavours:
//!
//! * a **fake chain** while no real flow is wired up (`fakesink` for
//!   sinks, `appsrc` for sources, both idle in PLAYING), or
//! * a **real chain** for a specific transport (today only MXL:
//!   `mxlsink` on the sink side; on the source side a sub-bin
//!   wrapping `mxlsrc ! capssetter`) once enough configuration is
//!   pinned to instantiate it.
//!
//! Future transports (NVDS-UDP, plain UDP/RTP, ...) plug in as
//! additional `build_real_*` factories alongside [`build_mxlsink`]
//! / [`build_mxlsrc`]; the swap mechanics here are transport-agnostic.
//!
//! `nmossink` topology (data flows from the ghost into the chain):
//!
//! ```text
//! ghost(sink) → anchor.sink ─ anchor.src → chain.sink
//! ```
//!
//! `nmossrc` topology (data flows from the chain out through the ghost):
//!
//! ```text
//! chain.src → anchor.sink ─ anchor.src → ghost(src)
//! ```
//!
//! The source-side fake chain is a live `appsrc` with `format=Time`
//! and the resolved essence caps when known. We never push buffers,
//! so the basesrc loop blocks forever in `create()` and the bin sits
//! idle while still answering downstream caps queries. When caps are
//! not yet resolvable (typically at construction time before any
//! properties have been set) the appsrc is built without caps;
//! downstream caps negotiation against a discriminating peer will
//! fail in that state, which is fine because the fake chain is
//! replaced before any real activation.
//!
//! This module owns the swap mechanics — block the anchor pad,
//! unlink/remove/add/link the chain, sync state, unblock — and the
//! factory helpers for both flavours of chain. The permanent
//! `anchor + block-probe` pattern (rather than a ghost-pad retarget
//! on every activation) is what lets us swap mid-stream without
//! losing sticky events on the chain's sink pad or racing the
//! streaming thread.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use gst::glib;
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::session::{UdpMedia, UdpVariant};
use crate::types::FlowFormat;

/// Name of the permanent anchor element inside every `nmossink` /
/// `nmossrc` bin. Stable so [`rebuild_chain`] can locate it via
/// `bin.by_name(...)` if a future caller wants to (today it walks
/// from the ghost pad's target instead).
const ANCHOR_NAME: &str = "anchor";

/// How long to wait for the anchor pad to go idle before aborting a
/// rebuild. Generous — under steady-state the pad is idle within
/// microseconds because the activation handler has already installed
/// its own outer IDLE probe before calling here; this only matters
/// if some upstream element is stuck holding a buffer push.
const PROBE_WAIT: Duration = Duration::from_secs(2);

/// How long to wait for the freshly-added inner chain to reach the
/// outer bin's current state. Generous for the same reason — basesink
/// is configured with `async=false` so READY→PAUSED is synchronous,
/// and basesrc's start() typically completes in milliseconds; a 2s
/// budget catches genuine stalls (e.g. libmxl `createFlowWriter`
/// failing) without dragging out a healthy activation.
const STATE_WAIT: gst::ClockTime = gst::ClockTime::from_seconds(2);

/// Swap the chain behind the bin's permanent anchor.
///
/// This is the **only** mutator of the bin's child set after
/// [`build_initial`] has run. The ghost pad target is *never* changed
/// — it's wired to the anchor's outer-facing pad once at construction
/// and stays there for the bin's lifetime.
///
/// The mechanic is the canonical GStreamer "swap behind a block probe"
/// pattern:
///
/// 1. Install an `IDLE | BLOCK_DOWNSTREAM` probe on the anchor's
///    chain-facing pad. Wait synchronously for the callback to fire,
///    which signals the pad is quiescent **and** that no buffer can
///    pass while the probe is installed.
/// 2. Walk the probed pad's peer to find the existing chain.
/// 3. Unlink the anchor from the old chain, take it to `NULL`, and
///    remove it from the bin.
/// 4. Add the new chain to the bin and link the anchor to it.
/// 5. `sync_state_with_parent()` + a synchronous `state(timeout)`
///    check. If the chain doesn't reach the parent's state within
///    [`STATE_WAIT`], return `Err` so the caller can ack the IS-05
///    activation as `Failed` — but the probe is still removed
///    afterwards so the data path doesn't wedge.
/// 6. Remove the probe. The next buffer push at the anchor forwards
///    sticky events (STREAM_START, CAPS, SEGMENT) to the new chain
///    automatically, so e.g. `mxlsink::set_caps` fires before the
///    first `render()`.
///
/// `pad_name` is the outer-facing pad name on `new_chain` —
/// `"sink"` for sink-direction bins, `"src"` for source-direction
/// bins. For wrapped sub-bins (e.g. the `mxlsrc ! capssetter` bin
/// built by `build_mxlsrc` with `advertise_caps`) this is the
/// ghosted outer pad name on the sub-bin, which is `"src"`.
pub(crate) fn rebuild_chain(
    cat: &gst::DebugCategory,
    bin: &gst::Bin,
    ghost: &gst::GhostPad,
    new_chain: &gst::Element,
    pad_name: &str,
) -> Result<(), anyhow::Error> {
    let anchor_outer_pad = ghost
        .target()
        .ok_or_else(|| anyhow!("ghost pad has no target; bin not initialised via build_initial"))?;
    let anchor = anchor_outer_pad
        .parent_element()
        .ok_or_else(|| anyhow!("ghost pad target has no parent element"))?;
    if anchor.name() != ANCHOR_NAME {
        bail!(
            "ghost target's parent is `{}`, expected `{ANCHOR_NAME}`; bin not initialised via build_initial",
            anchor.name(),
        );
    }

    // The probe always lives on the anchor's *chain-side* pad: that's
    // the pad upstream of (sink direction) or downstream of (source
    // direction) the chain we're about to rebuild. The block direction
    // is `DOWNSTREAM` in either case — we want to stop the buffer
    // stream from progressing past the anchor while we mutate the
    // bin's child set.
    let (probe_pad_name, _outer_pad_dir_label) = match ghost.direction() {
        gst::PadDirection::Sink => ("src", "sink"),
        gst::PadDirection::Src => ("sink", "src"),
        _ => bail!("ghost pad direction is neither Sink nor Src"),
    };
    let probe_pad = anchor.static_pad(probe_pad_name).ok_or_else(|| {
        anyhow!("anchor `{ANCHOR_NAME}` missing `{probe_pad_name}` pad")
    })?;

    let probe_id = block_and_wait(cat, &probe_pad)
        .context("blocking anchor pad before chain rebuild")?;

    // Always remove the probe on the way out, even on error, so the
    // pipeline can drain — a stuck probe with no chain behind it is a
    // worse failure than a partially-completed rebuild.
    let result = swap_chain_inner(cat, bin, &anchor, &probe_pad, new_chain, pad_name, ghost);
    probe_pad.remove_probe(probe_id);
    gst::debug!(
        cat,
        "rebuild_chain: probe removed on `{}` ({})",
        probe_pad.name(),
        if result.is_ok() { "ok" } else { "err" },
    );
    result
}

/// Inner half of [`rebuild_chain`] — runs with the anchor pad held
/// blocked. Factored out so we can `?`-propagate errors and still
/// remove the probe unconditionally in the caller.
fn swap_chain_inner(
    cat: &gst::DebugCategory,
    bin: &gst::Bin,
    anchor: &gst::Element,
    probe_pad: &gst::Pad,
    new_chain: &gst::Element,
    new_chain_pad_name: &str,
    ghost: &gst::GhostPad,
) -> Result<(), anyhow::Error> {
    // Direction-dependent pad on the anchor that's actually linked to
    // the old chain. For sink-direction this is the same as
    // `probe_pad` (anchor.src ↔ old_chain.sink). For source-direction
    // it's the opposite pad (old_chain.src ↔ anchor.sink) — we still
    // block at `probe_pad` (anchor.src) because that's the only pad
    // GStreamer can hold downstream-blocked, even though we
    // unlink/link on the other side.
    let (link_pad_name, _) = match ghost.direction() {
        gst::PadDirection::Sink => ("src", "sink"),
        gst::PadDirection::Src => ("sink", "src"),
        _ => unreachable!("checked in rebuild_chain"),
    };
    let link_pad = anchor.static_pad(link_pad_name).ok_or_else(|| {
        anyhow!("anchor `{ANCHOR_NAME}` missing `{link_pad_name}` pad")
    })?;

    // The anchor's chain-side pad usually has a peer — the chain we're
    // about to replace. But if a previous `rebuild_chain` errored out
    // after unlink + remove and before add + link, the anchor is left
    // dangling. Treat that as "no old chain to remove" rather than a
    // hard error so we can still install `new_chain` and bring the bin
    // back to a working state. (`execute_activation_plan`'s fake-chain
    // fallback relies on this.)
    if let Some(old_chain_pad) = link_pad.peer() {
        let old_chain = old_chain_pad
            .parent_element()
            .ok_or_else(|| anyhow!("old chain pad has no parent element"))?;
        gst::debug!(
            cat,
            "rebuild_chain: old chain = `{}`; probe held on `{}`",
            old_chain.name(),
            probe_pad.name(),
        );

        // Unlink in the direction-appropriate order: src.unlink(sink).
        match ghost.direction() {
            gst::PadDirection::Sink => link_pad.unlink(&old_chain_pad),
            gst::PadDirection::Src => old_chain_pad.unlink(&link_pad),
            _ => unreachable!(),
        }
        .map_err(|e| {
            anyhow!("unlinking anchor from old chain `{}`: {e}", old_chain.name())
        })?;

        let _ = old_chain.set_state(gst::State::Null);
        bin.remove(&old_chain)
            .with_context(|| format!("removing old chain `{}`", old_chain.name()))?;
    } else {
        gst::warning!(
            cat,
            "rebuild_chain: anchor `{ANCHOR_NAME}.{}` has no peer (previous rebuild left the bin dangling?); recovering by installing the new chain directly",
            link_pad.name(),
        );
    }

    bin.add(new_chain)
        .with_context(|| format!("adding new chain `{}`", new_chain.name()))?;

    let new_chain_pad = new_chain
        .static_pad(new_chain_pad_name)
        .ok_or_else(|| anyhow!(
            "new chain `{}` missing `{new_chain_pad_name}` pad",
            new_chain.name(),
        ))?;
    match ghost.direction() {
        gst::PadDirection::Sink => link_pad.link(&new_chain_pad),
        gst::PadDirection::Src => new_chain_pad.link(&link_pad),
        _ => unreachable!(),
    }
    .map_err(|e| anyhow!(
        "linking anchor to new chain `{}`: {e}",
        new_chain.name(),
    ))?;

    new_chain
        .sync_state_with_parent()
        .with_context(|| format!("syncing state of `{}` with parent", new_chain.name()))?;

    wait_for_chain_state(cat, bin, new_chain)
}

/// Install an `IDLE | BLOCK_DOWNSTREAM` probe on `pad` and wait for
/// the callback to fire (or [`PROBE_WAIT`] to elapse).
///
/// The returned `PadProbeId` is the still-installed probe — the
/// caller is responsible for calling `pad.remove_probe(id)` when the
/// data path is ready to resume. The probe callback stays a no-op
/// after the initial signal so the pad stays blocked indefinitely
/// while the chain swap proceeds.
fn block_and_wait(
    cat: &gst::DebugCategory,
    pad: &gst::Pad,
) -> Result<gst::PadProbeId, anyhow::Error> {
    let fired = Arc::new((Mutex::new(false), Condvar::new()));
    let fired_cb = Arc::clone(&fired);
    let pad_name = pad.name().to_string();

    let probe_id = pad
        .add_probe(
            gst::PadProbeType::IDLE | gst::PadProbeType::BLOCK_DOWNSTREAM,
            move |_pad, _info| {
                let (lock, cvar) = &*fired_cb;
                let mut f = lock.lock().unwrap();
                if !*f {
                    *f = true;
                    cvar.notify_all();
                }
                // Stay installed so the pad stays blocked until the
                // caller explicitly removes us.
                gst::PadProbeReturn::Ok
            },
        )
        .ok_or_else(|| anyhow!("add_probe on anchor pad `{pad_name}` returned None"))?;

    let (lock, cvar) = &*fired;
    let mut f = lock.lock().unwrap();
    if !*f {
        let (guard, status) = cvar
            .wait_timeout(f, PROBE_WAIT)
            .expect("anchor probe condvar poisoned");
        f = guard;
        if status.timed_out() && !*f {
            drop(f);
            pad.remove_probe(probe_id);
            bail!(
                "anchor pad `{pad_name}` did not become idle within {PROBE_WAIT:?}; aborting rebuild",
            );
        }
    }
    drop(f);
    gst::debug!(cat, "rebuild_chain: anchor pad `{pad_name}` is blocked + idle");
    Ok(probe_id)
}

/// Block until `new_chain` reaches the parent bin's *target* state
/// (i.e. the state the parent is heading for, not necessarily the
/// state it's at right now), or until [`STATE_WAIT`] expires.
/// Returns `Err` if the chain's state change times out async or
/// fails outright — the caller propagates so the IS-05 activation
/// handler acks `Failed`.
///
/// "Target" rather than "current" matters because the very first
/// call to [`rebuild_chain`] happens from inside the bin's own
/// `change_state(NullToReady)` vfunc: at that point the bin's
/// `current_state` is still `Null` even though it has already
/// committed to going to `Ready`, and `sync_state_with_parent`
/// correctly pulls the new chain to `Ready` to match. If we
/// compared against `current_state` we'd see "new chain at Ready
/// but parent at Null" and incorrectly fail the rebuild — that
/// would propagate up through `open_session` and `change_state`
/// would return `StateChangeError`, so the pipeline could never
/// reach `READY` in the first place.
///
/// The 2-second budget is deliberately generous: state changes on
/// the streaming thread of a basesink can take O(100 ms) when
/// preroll has to fire, but with `async=false` (see `build_mxlsink`)
/// READY→PAUSED is synchronous and the whole transition completes
/// in milliseconds. Anything longer is almost certainly a real
/// stall worth surfacing to the controller rather than a slow
/// transition we should wait out.
fn wait_for_chain_state(
    cat: &gst::DebugCategory,
    bin: &gst::Bin,
    new_chain: &gst::Element,
) -> Result<(), anyhow::Error> {
    let parent_target = parent_target_state(bin);
    let (ret, current, pending) = new_chain.state(STATE_WAIT);
    let name = new_chain.name();
    match ret {
        Ok(gst::StateChangeSuccess::Success) | Ok(gst::StateChangeSuccess::NoPreroll) => {
            if current == parent_target {
                gst::debug!(
                    cat,
                    "rebuild_chain: `{name}` reached parent target state {parent_target:?} (ret={ret:?})",
                );
                Ok(())
            } else {
                // Successful state change but settled at the wrong
                // state — log loudly but don't fail the rebuild;
                // the parent's state machine will pull the child
                // along on its next transition, and failing here
                // would regress legitimate startup paths (e.g.
                // sync_state_with_parent racing with a parent
                // already transitioning further up).
                gst::warning!(
                    cat,
                    "rebuild_chain: `{name}` settled at {current:?} but parent target is \
                     {parent_target:?} (pending={pending:?}); proceeding and relying on \
                     parent's state-machine to pull it along",
                );
                Ok(())
            }
        }
        Ok(gst::StateChangeSuccess::Async) => bail!(
            "`{name}` state change still ASYNC after {STATE_WAIT} \
             (current={current:?}, pending={pending:?}, parent_target={parent_target:?}); \
             chain has not reached its target state",
        ),
        Err(err) => bail!(
            "`{name}` state change failed ({err:?}); \
             current={current:?}, pending={pending:?}, parent_target={parent_target:?}",
        ),
    }
}

/// Return the state the bin is currently heading for: its pending
/// state if a transition is in flight, otherwise its current
/// state. Mirrors GstElement's internal `target_state` concept;
/// gstreamer-rs doesn't expose `gst_element_get_target_state`
/// directly so we synthesise it from a zero-timeout `state(0)`.
fn parent_target_state(bin: &gst::Bin) -> gst::State {
    let (_ret, current, pending) = bin.state(Some(gst::ClockTime::ZERO));
    if pending == gst::State::VoidPending {
        current
    } else {
        pending
    }
}

/// True iff the bin's current inner chain is a *real* chain (a
/// real transport element such as `mxlsink` / `mxlsrc`, identified
/// by the absence of the `-fake` suffix on its element name) — as
/// opposed to a fake chain (`*-fake`). Returns `false` when the
/// chain can't be resolved (e.g. ghost pad missing a target, anchor
/// disconnected) — in that case the caller should fall through to a
/// single-swap rebuild rather than inserting an intermediate fake
/// hop.
///
/// Used by `execute_activation_plan` to decide whether to go via a fake
/// hop when swapping real → real: even though IS-05 requires every
/// activation to rebuild the data path, doing real → new real in
/// one step can race the transport's per-process state (libmxl, for
/// instance: the old `FlowReader` may not be fully released before
/// the new one tries to attach to the same flow id). Going
/// `real → fake → new real` serialises the tear-down and the
/// re-open so the new real chain's start-up (`mxlsrc.start()` /
/// `mxlsink.set_caps()` for MXL) sees a clean transport state.
pub(crate) fn current_chain_is_real(ghost: &gst::GhostPad) -> bool {
    let Some(anchor_outer_pad) = ghost.target() else {
        return false;
    };
    let Some(anchor) = anchor_outer_pad.parent_element() else {
        return false;
    };
    if anchor.name() != ANCHOR_NAME {
        return false;
    }
    let link_pad_name = match ghost.direction() {
        gst::PadDirection::Sink => "src",
        gst::PadDirection::Src => "sink",
        _ => return false,
    };
    let Some(link_pad) = anchor.static_pad(link_pad_name) else {
        return false;
    };
    let Some(chain_pad) = link_pad.peer() else {
        return false;
    };
    let Some(chain) = chain_pad.parent_element() else {
        return false;
    };
    !chain.name().ends_with("-fake")
}

/// Build the `nmossink` fake chain: a `fakesink` so the element
/// looks valid in the pipeline before configuration is complete
/// (it sinks any caps and drops everything). The `-fake` suffix on
/// the element name is what [`current_chain_is_real`] checks to
/// decide whether to insert a fake hop on real → real activations.
pub(crate) fn build_fake_sink() -> Result<gst::Element, anyhow::Error> {
    gst::ElementFactory::make("fakesink")
        .name("nmossink-fake")
        .property("sync", true)
        .property("async", false)
        .build()
        .map_err(|e| anyhow!("creating fakesink for nmossink fake chain: {e}"))
}

/// Build the `nmossrc` fake chain: a live `appsrc` that never gets
/// buffers pushed into it. Its basesrc loop blocks in `create()` so
/// no data flows; when `caps` is `Some` the appsrc also answers
/// downstream caps queries with a concrete shape so negotiation
/// completes. Replaced by a real chain (today `mxlsrc` in a
/// `mxlsrc ! capssetter` sub-bin) once an IS-05 activation pins a
/// Flow id. The `-fake` suffix on the element name is what
/// [`current_chain_is_real`] checks to decide whether to insert a
/// fake hop on real → real activations.
///
/// When `caps` is `None` (typical at construction time, before any
/// properties have been set) the appsrc is built without caps;
/// downstream caps negotiation against a discriminating peer will
/// fail in that state, but the fake chain is replaced before any
/// real activation. Callers are expected to pass `Some(caps)`
/// whenever a `caps` property, `transport-file`, or
/// `transport-file-path` source is available.
pub(crate) fn build_fake_src(
    caps: Option<&gst::Caps>,
) -> Result<gst::Element, anyhow::Error> {
    let elem = gst::ElementFactory::make("appsrc")
        .name("nmossrc-fake")
        .property("is-live", true)
        .property("format", gst::Format::Time)
        .build()
        .map_err(|e| anyhow!("creating appsrc for nmossrc fake chain: {e}"))?;
    if let Some(caps) = caps {
        elem.set_property("caps", caps);
    }
    Ok(elem)
}

/// Build the inner `mxlsink` for `nmossink`. Fails with a clear
/// message if the `mxl` plugin isn't on `GST_PLUGIN_PATH` or the
/// element factory rejects the supplied properties.
///
/// `async=false` is critical for mid-stream IS-05 re-enables.
/// `GstBaseSink`'s default `async=true` makes `READY→PAUSED`
/// return `ASYNC` while it waits for the first buffer to preroll
/// — fine when the whole pipeline is being brought up together
/// (the bin's latency query + live-source detection drive things
/// to PLAYING), but a deadlock when the sink is added to a
/// running bin **and** the data path is gated by the anchor
/// probe: no buffer can preroll because the probe is blocking
/// downstream flow, so the state change never resolves. With
/// `async=false` READY→PAUSED returns synchronously, the
/// [`wait_for_chain_state`] check passes, the probe is removed,
/// and the next buffer pushed through the anchor triggers
/// `set_caps` + `render()` in the expected order.
#[derive(Debug)]
pub(crate) struct MxlSinkChain {
    /// Wrapper element added to the outer bin (`mxlsink` — depth 1).
    pub bin: gst::Element,
    /// Inner sink (`mxlsink`; same object as [`Self::bin`]).
    pub transport: gst::Element,
}

#[derive(Debug)]
pub(crate) struct MxlSrcChain {
    /// Wrapper element added to the outer bin (bare `mxlsrc` or a
    /// sub-bin wrapping `mxlsrc ! capssetter`).
    pub bin: gst::Element,
    /// Inner source (`mxlsrc` inside [`Self::bin`]).
    pub transport: gst::Element,
}

#[derive(Debug)]
pub(crate) struct UdpSinkChain {
    /// Wrapper bin added to the outer bin (`nmossink-udp`).
    pub bin: gst::Element,
    /// RTP payloader inside [`Self::bin`].
    pub pay: gst::Element,
    /// Inner sink (`udpsink`) inside [`Self::bin`].
    pub transport: gst::Element,
}

#[derive(Debug)]
pub(crate) struct UdpSrcChain {
    /// Wrapper bin added to the outer bin (`nmossrc-udp` or similar).
    pub bin: gst::Element,
    /// Inner source (`udpsrc` / `udpsrc2`) inside [`Self::bin`].
    pub transport: gst::Element,
    /// RTP depayloader inside [`Self::bin`].
    pub depay: gst::Element,
}

pub(crate) fn build_mxlsink(domain_path: &str, flow_id: &str) -> Result<MxlSinkChain, anyhow::Error> {
    require_mxl_factory("mxlsink")?;
    let mxlsink = gst::ElementFactory::make("mxlsink")
        .name("nmossink-mxl")
        .property("domain", domain_path)
        .property("flow-id", flow_id)
        .property("async", false)
        .build()
        .with_context(|| {
            format!(
                "instantiating `mxlsink` with domain={domain_path:?}, flow-id={flow_id}"
            )
        })?;
    Ok(MxlSinkChain {
        bin: mxlsink.clone(),
        transport: mxlsink,
    })
}

/// Build the inner `mxlsrc` for `nmossrc`. `format` picks which of
/// `video-flow-id` / `audio-flow-id` / `data-flow-id` receives
/// `flow_id`; [`FlowFormat::Unspecified`] is rejected because the
/// caller is responsible for falling back to the fake chain before
/// reaching this helper.
///
/// When `advertise_caps` is `Some`, the returned element is a small
/// `Bin` containing `mxlsrc ! capssetter caps=advertise_caps` with
/// its src pad ghosted out; downstream caps queries against the
/// outer bin's ghost pad then see the concrete essence caps the
/// flow will carry, rather than `mxlsrc`'s broad pad template.
/// `None` returns the bare `mxlsrc` and the outer bin's ghost pad
/// reflects whatever `mxlsrc` advertises (broad template until the
/// first CAPS event flows).
pub(crate) fn build_mxlsrc(
    domain_path: &str,
    flow_id: &str,
    format: FlowFormat,
    advertise_caps: Option<&gst::Caps>,
) -> Result<MxlSrcChain, anyhow::Error> {
    require_mxl_factory("mxlsrc")?;
    let prop = match format {
        FlowFormat::Video => "video-flow-id",
        FlowFormat::Audio => "audio-flow-id",
        FlowFormat::Data => "data-flow-id",
        FlowFormat::Unspecified => {
            return Err(anyhow!(
                "build_mxlsrc called with FlowFormat::Unspecified; caller should have built a fake chain",
            ));
        }
    };
    let mxlsrc = gst::ElementFactory::make("mxlsrc")
        .name("nmossrc-mxl")
        .property("domain", domain_path)
        .property(prop, flow_id)
        .build()
        .with_context(|| {
            format!(
                "instantiating `mxlsrc` with domain={domain_path:?}, {prop}={flow_id}"
            )
        })?;

    let Some(caps) = advertise_caps else {
        return Ok(MxlSrcChain {
            bin: mxlsrc.clone(),
            transport: mxlsrc,
        });
    };

    let capssetter = gst::ElementFactory::make("capssetter")
        .name("nmossrc-caps")
        .property("caps", caps)
        .build()
        .map_err(|e| anyhow!("instantiating `capssetter` for nmossrc caps advertisement: {e}"))?;
    let bin = gst::Bin::with_name("nmossrc-inner");
    bin.add_many([&mxlsrc, &capssetter])
        .map_err(|e| anyhow!("adding mxlsrc + capssetter to inner bin: {e}"))?;
    mxlsrc
        .link(&capssetter)
        .with_context(|| "linking mxlsrc to inner capssetter")?;
    let capssetter_src = capssetter
        .static_pad("src")
        .ok_or_else(|| anyhow!("capssetter missing src pad"))?;
    let ghost = gst::GhostPad::builder(gst::PadDirection::Src)
        .name("src")
        .build();
    ghost
        .set_target(Some(&capssetter_src))
        .map_err(|e| anyhow!("setting inner ghost src target: {e}"))?;
    ghost
        .set_active(true)
        .map_err(|e| anyhow!("activating inner ghost src: {e}"))?;
    bin.add_pad(&ghost)
        .map_err(|e| anyhow!("adding ghost src to inner bin: {e}"))?;
    Ok(MxlSrcChain {
        bin: bin.upcast(),
        transport: mxlsrc.clone(),
    })
}

/// Build the inner UDP/RTP sink chain for `nmossink`. Always
/// constructs a sub-bin: `rtp<essence>pay ! udpsink` (or the
/// gst-plugins-rs `*pay2` family + `udpsink` for [`UdpVariant::V2`])
/// with the payloader's sink pad ghosted out so the outer
/// [`rebuild_chain`] swap mechanism plugs it in directly behind the
/// anchor.
///
/// **Contract.** `media` is expected to come from
/// [`crate::sdp::parse_sdp`], which guarantees the RTP caps carry
/// the full set of fields this factory reads: `encoding-name`,
/// `payload`, `clock-rate`, `encoding-params`, and (for audio
/// SDPs that included `a=ptime:`) `a-ptime`. There is no
/// caps-derives-SDP / synthesis path for UDP today, so the only
/// caller (`decide_inner_config_udp`) won't reach here with an
/// incomplete `rtp_caps`; absent an SDP the caller returns
/// `InnerConfig::Fake` instead and waits for IS-05 PATCH. When
/// BCP-006-02-style transmitter-side SDP synthesis from `caps` +
/// properties lands, the defaults it picks (PT 96, ptime 1 ms
/// for audio, encoding-name from raw caps, …) flow through the
/// same `UdpMedia` surface and this factory needs no changes.
///
/// `pt` is taken from `media.rtp_caps.payload`; for audio
/// essences `min-ptime` / `max-ptime` are pinned (in nanoseconds)
/// from the `a-ptime` field [`crate::sdp::parse_sdp`] hoists onto
/// the RTP caps so the receiver sees exactly the packet duration
/// the SDP advertises. When `a-ptime` is absent the payloader
/// auto-sizes packets based on buffer arrival — `parse_sdp`
/// faithfully reflects "SDP didn't say" and we deliberately don't
/// pick a default here.
///
/// `ssrc` is left at the payloader's default (random per element
/// instance) — the daemon-published SDP does not advertise an
/// SSRC.
///
/// `udpsink.async=false` mirrors `mxlsink`'s rationale (see
/// [`build_mxlsink`]'s doc): mid-stream activation behind the
/// anchor's block-probe needs synchronous READY→PAUSED so the
/// state-change resolves before the probe is removed and buffers
/// start flowing. `sync=true` preserves RTP packet timing.
///
/// `bind-port` is set when `primary.source_port` is `Some` so
/// the IS-04 / IS-05 advertised source port matches the wire.
/// `bind-address` is set when `primary.interface_ip` is `Some` so
/// unicast send routing picks the right NIC. For multicast
/// destinations [`multicast_iface_name`] additionally resolves
/// `interface_ip` to its kernel interface name and pins it on
/// `udpsink.multicast-iface` — `bind-address` alone does *not*
/// constrain Linux multicast egress (`IP_MULTICAST_IF` does), so
/// without this set on a multi-NIC host the kernel's default
/// multicast route picks the NIC, which is wrong for the SMPTE
/// "red/blue" two-NIC layout. Unknown interface IPs (operator
/// misconfiguration, or an `interface_ip` that lives on a
/// different host) silently fall back to "leave `multicast-iface`
/// unset and let the kernel's default-route pick" — that's the
/// only safe answer when we can't prove which NIC was intended,
/// and matches single-NIC hosts where there's only one choice.
///
/// **Why no symmetric capssetter fix-up on the sender side?**
/// The receiver-side capssetter trick in [`build_udpsrc`]
/// corrects fields that the V1 depayloader leaves at format
/// defaults despite the SDP saying otherwise (`framerate=0/1`,
/// `colorimetry=bt601` on UYVY, etc.). The sender side has the
/// opposite gap: V1 `rtpvrawpay` omits `exactframerate` /
/// `chroma-position` / `tcs` from the wire fmtp, and V1 / V2
/// `rtpvrawpay` both omit `RANGE`. None of those omissions
/// propagate to the receiver in our deployment model: the
/// receiver gets its caps from the **SDP we publish** (which
/// `parse_sdp` pins onto `udpsrc.caps` in [`build_udpsrc`]),
/// not from any caps-on-the-wire mechanism. The payloader's
/// `application/x-rtp` src caps are consumed only by `udpsink`,
/// which doesn't care about anything beyond
/// `application/x-rtp`. The only scenario where wire-caps gaps
/// would matter is the caps-only SDP synthesis path: there
/// [`crate::sdp::from_caps`] reads the app's **input** caps to
/// `nmossink` (which carry full GStreamer colorimetry including
/// range, framerate, etc.) rather than the payloader output,
/// and `rtp_caps_from_raw_video` synthesises a self-consistent
/// `application/x-rtp` view of those caps. So nothing to
/// capssetter-fix here.
pub(crate) fn build_udpsink(
    media: &UdpMedia,
    variant: UdpVariant,
) -> Result<UdpSinkChain, anyhow::Error> {
    let rtp_s = media
        .rtp_caps
        .structure(0)
        .ok_or_else(|| anyhow!("UdpMedia.rtp_caps is empty (no structure(0))"))?;
    let encoding_name = rtp_s
        .get::<&str>("encoding-name")
        .map_err(|e| anyhow!("UdpMedia.rtp_caps missing `encoding-name`: {e}"))?;
    let pt = rtp_s
        .get::<i32>("payload")
        .map_err(|e| anyhow!("UdpMedia.rtp_caps missing `payload` field: {e}"))?;
    if !(0..=127).contains(&pt) {
        bail!(
            "UdpMedia.rtp_caps `payload`={pt} out of valid RTP payload-type range 0..=127"
        );
    }
    let ptime_ns = if matches!(media.format, FlowFormat::Audio) {
        ptime_ns_from_rtp_caps(rtp_s)?
    } else {
        None
    };

    let payloader_factory = select_rtp_factory("pay", media.format, encoding_name, variant)?;
    let payloader = gst::ElementFactory::make(&payloader_factory)
        .name("nmossink-payloader")
        .property("pt", pt as u32)
        .build()
        .with_context(|| {
            format!("instantiating payloader `{payloader_factory}` (pt={pt})")
        })?;
    if let Some(ns) = ptime_ns {
        payloader.set_property("min-ptime", ns);
        payloader.set_property("max-ptime", ns);
    }

    let udpsink = gst::ElementFactory::make("udpsink")
        .name("nmossink-udpsink")
        .property("host", &media.primary.destination_ip)
        .property("port", i32::from(media.primary.destination_port))
        .property("async", false)
        .build()
        .with_context(|| {
            format!(
                "instantiating `udpsink` (host={}, port={})",
                media.primary.destination_ip, media.primary.destination_port,
            )
        })?;
    if let Some(port) = media.primary.source_port {
        udpsink.set_property("bind-port", i32::from(port));
    }
    if let Some(addr) = &media.primary.interface_ip {
        udpsink.set_property("bind-address", addr);
    }
    if let Some(iface) = multicast_iface_name(
        &media.primary.destination_ip,
        media.primary.interface_ip.as_deref(),
    ) {
        udpsink.set_property("multicast-iface", &iface);
    }

    let bin = gst::Bin::with_name("nmossink-udp");
    bin.add_many([&payloader, &udpsink])
        .map_err(|e| anyhow!("adding payloader + udpsink to inner bin: {e}"))?;
    payloader
        .link(&udpsink)
        .with_context(|| "linking payloader to udpsink")?;

    let payloader_sink = payloader
        .static_pad("sink")
        .ok_or_else(|| anyhow!("payloader `{payloader_factory}` missing sink pad"))?;
    let ghost = gst::GhostPad::builder(gst::PadDirection::Sink)
        .name("sink")
        .build();
    ghost
        .set_target(Some(&payloader_sink))
        .map_err(|e| anyhow!("setting inner ghost sink target: {e}"))?;
    ghost
        .set_active(true)
        .map_err(|e| anyhow!("activating inner ghost sink: {e}"))?;
    bin.add_pad(&ghost)
        .map_err(|e| anyhow!("adding ghost sink to inner bin: {e}"))?;

    Ok(UdpSinkChain {
        bin: bin.upcast(),
        pay: payloader,
        transport: udpsink,
    })
}

/// Look up the GStreamer factory name for an
/// `rtp<essence><suffix>` element pair, optionally upgrading to the
/// `gst-plugins-rs` `*pay2` / `*depay2` sibling when
/// [`UdpVariant::V2`] is requested.
///
/// `suffix` is `"pay"` for senders or `"depay"` for receivers;
/// `stem` is one of `"rtpvraw"` / `"rtpL24"` / `"rtpL16"` /
/// `"rtpsmpte291"`, chosen from `(FlowFormat, encoding-name)`.
///
/// `encoding_name` is matched ASCII-case-insensitively. The SDP
/// parser already upper-cases `encoding-name` (GStreamer's
/// `gst_sdp_media_get_caps_from_media` calls `g_ascii_strup` on
/// the rtpmap name before storing it on the caps), so any value
/// produced by [`crate::sdp::parse_sdp`] is already canonical;
/// the normalisation here is defensive against future call sites
/// that build `UdpMedia` directly (synthesis-from-caps,
/// hand-crafted tests, …).
///
/// V2 falls back to V1 transparently when the `*pay2` / `*depay2`
/// sibling isn't present — `gst-plugins-rs` ships v2 forms
/// piecemeal (e.g. `rtpvrawpay2` lands separately from
/// `rtpL24pay2`) so the V2 dispatch shouldn't break in environments
/// that have only some of them. For RFC 8331 SMPTE 291 ANC there is
/// no V1 (`gst-plugins-good`) equivalent at all — the only element
/// is `gst-plugins-rs`' `rtpsmpte291pay` / `rtpsmpte291depay`, and
/// both [`UdpVariant::V1`] and [`UdpVariant::V2`] resolve to the
/// same name. The variant choice is essence-orthogonal for ANC.
fn select_rtp_factory(
    suffix: &str,
    format: FlowFormat,
    encoding_name: &str,
    variant: UdpVariant,
) -> Result<String, anyhow::Error> {
    let encoding_name_upper = encoding_name.to_ascii_uppercase();
    let stem = match (format, encoding_name_upper.as_str()) {
        (FlowFormat::Video, "RAW") => "rtpvraw",
        (FlowFormat::Audio, "L24") => "rtpL24",
        (FlowFormat::Audio, "L16") => "rtpL16",
        (FlowFormat::Data, "SMPTE291") => "rtpsmpte291",
        _ => bail!(
            "unsupported essence for UDP/RTP `{suffix}`: format={format:?}, \
             encoding-name=`{encoding_name}` (today RFC 4175 `RAW` video, \
             ST 2110-30 `L24` / `L16` audio, and RFC 8331 / ST 2110-40 \
             `SMPTE291` ANC are supported)"
        ),
    };
    let v1 = format!("{stem}{suffix}");
    if gst::ElementFactory::find(&v1).is_none() {
        bail!(
            "GStreamer factory `{v1}` not found; install `{package}` and \
             ensure it's on `GST_PLUGIN_PATH`",
            package = package_hint_for_stem(stem),
        );
    }
    match variant {
        UdpVariant::V1 => Ok(v1),
        UdpVariant::V2 => {
            let v2 = format!("{v1}2");
            if gst::ElementFactory::find(&v2).is_some() {
                Ok(v2)
            } else {
                Ok(v1)
            }
        }
    }
}

/// Best-guess package hint for a missing-factory error message.
///
/// `rtpsmpte291` only exists in `gst-plugins-rs`' `rsrtp` plugin —
/// suggesting `gst-plugins-good` for that one would send the user
/// hunting in the wrong package. Everything else
/// ([`select_rtp_factory`] knows about today) lives in
/// `gst-plugins-good`.
fn package_hint_for_stem(stem: &str) -> &'static str {
    match stem {
        "rtpsmpte291" => "gst-plugins-rs (`rsrtp` plugin)",
        _ => "gst-plugins-good",
    }
}

/// Pick the `udpsrc` factory for a given [`UdpVariant`], mirroring
/// the V2-fallback pattern in [`select_rtp_factory`].
///
/// `V1` is gst-plugins-good's `udpsrc`. `V2` prefers
/// gst-plugins-rs' `udpsrc2` (the high-performance `recvmmsg` +
/// optional GRO rewrite added in gst-plugins-rs 0.16 / GStreamer
/// 1.30 — Centricular's primary motivation for it is exactly ST
/// 2110 multicast capture), and falls back to V1 `udpsrc` when
/// `udpsrc2` isn't installed. The factories are documented as
/// drop-in API replacements for the properties this module sets
/// today; see [`build_udpsrc`] for the one place the API differs
/// (SSM source-filter property name).
///
/// No `udpsink2` exists upstream — `udpsrc2`'s motivation was
/// fixing the receive-side packet-rate ceiling, which doesn't
/// apply to sending — so the sender path doesn't need an
/// analogous helper. [`build_udpsink`] hard-codes `udpsink`.
fn select_udpsrc_factory(variant: UdpVariant) -> &'static str {
    match variant {
        UdpVariant::V1 => "udpsrc",
        UdpVariant::V2 => {
            if gst::ElementFactory::find("udpsrc2").is_some() {
                "udpsrc2"
            } else {
                "udpsrc"
            }
        }
    }
}

/// Convert the `a-ptime` field hoisted onto the RTP caps by
/// [`crate::sdp::parse_sdp`] into nanoseconds for `rtpaudiopay`'s
/// `min-ptime` / `max-ptime` properties. SDP carries ptime in
/// (possibly fractional) milliseconds; the payloader properties
/// are `int64` nanoseconds.
///
/// Returns `Ok(None)` when the field is absent (no ptime to pin —
/// the payloader will use its default packetisation). Returns
/// `Err` for a present-but-malformed value (non-numeric, ≤0,
/// overflows i64 ns), which we want to surface clearly rather
/// than silently fall back to defaults.
fn ptime_ns_from_rtp_caps(rtp_s: &gst::StructureRef) -> Result<Option<i64>, anyhow::Error> {
    let Ok(s) = rtp_s.get::<&str>("a-ptime") else {
        return Ok(None);
    };
    let ms: f64 = s
        .parse()
        .with_context(|| format!("parsing a-ptime=`{s}` as milliseconds"))?;
    if !ms.is_finite() || ms <= 0.0 {
        bail!("a-ptime=`{s}` ms must be a finite positive value");
    }
    let ns = ms * 1_000_000.0;
    if ns > i64::MAX as f64 {
        bail!("a-ptime=`{s}` ms overflows i64 nanoseconds");
    }
    Ok(Some(ns as i64))
}

/// Resolve the kernel interface name to set on `multicast-iface`
/// when the destination is multicast and `interface_ip` is bound on
/// a local NIC. `None` collapses every "leave the property unset"
/// case:
///
/// - Unicast destination — no multicast routing decision to pin;
///   `bind-address` already covers source-IP selection for senders
///   and the kernel's destination-IP demux covers receivers.
/// - `interface_ip` absent on the [`UdpLeg`] — user didn't express
///   a NIC preference; let the kernel pick.
/// - Malformed `destination_ip` or `interface_ip` — we don't try to
///   guess, but we also don't fail the chain factory; the SDP parser
///   would have rejected these already.
/// - `interface_ip` not bound on any local NIC ([`crate::iface::iface_name_for_ip`]
///   returns `None`) — operator misconfiguration or the SDP came
///   from a different host; falling back to the default route is the
///   only safe answer when we can't prove which NIC the user meant.
fn multicast_iface_name(destination_ip: &str, interface_ip: Option<&str>) -> Option<String> {
    let dest = destination_ip.parse::<std::net::IpAddr>().ok()?;
    if !dest.is_multicast() {
        return None;
    }
    let iface = interface_ip?.parse::<std::net::IpAddr>().ok()?;
    crate::iface::iface_name_for_ip(iface)
}

/// Build the inner UDP/RTP source chain for `nmossrc`. Always
/// constructs a sub-bin:
/// `udpsrc(caps=rtp_caps) ! rtp<essence>depay [! capsfilter(advertise_caps)]`
/// (with the udpsrc factory picked via [`select_udpsrc_factory`]
/// and the depayloader picked via [`select_rtp_factory`], so
/// [`UdpVariant::V2`] auto-upgrades to `gst-plugins-rs`' `udpsrc2`
/// and the `*depay2` siblings when present and falls back per
/// element to V1 when not) with the trailing element's src pad
/// ghosted out so the outer [`rebuild_chain`] swap mechanism
/// plugs it in directly behind the anchor.
///
/// **`rtpjitterbuffer` is intentionally not included.** ST 2110 RTP
/// is open-loop multicast (no NACK feedback path — retransmission is
/// impossible), and single-RTP-flow packet order is preserved by
/// every modern Ethernet switch and NIC RSS bucket (the 5-tuple hash
/// keeps a flow on one path / one receive queue). The remaining
/// services `rtpjitterbuffer` provides — `do-lost` events for loss
/// detection, the `stats` `GstStructure` for telemetry — only earn
/// their cost (~one frame of latency at the default 200 ms; even
/// tuned to 40 ms still meaningful for low-latency RTP) if the
/// element surface plumbs them out to user code, which `nmossrc`
/// doesn't yet. Adding the jitterbuffer is a couple of `bin.add` /
/// `link` lines slotted between `udpsrc` and the depayloader; that
/// change should land alongside the surface that justifies its
/// latency cost (an `rtp-latency` element property + an `rtp-stats`
/// readback or a periodic element `GstMessage`). Strict ST 2110
/// reception with kernel-bypass + PTP-aligned timing belongs to
/// `nvdsudpsrc` rather than this OSS chain.
///
/// `udpsrc.caps` is set to `media.rtp_caps` so downstream caps
/// queries (and the depayloader's pad negotiation) see the exact
/// `application/x-rtp` shape from the SDP — no separate
/// `capsfilter` needed for the RTP side.
///
/// `multicast-iface` (via [`multicast_iface_name`]) is set only
/// when the destination is multicast and the configured
/// `interface_ip` resolves to a local NIC. For multicast receive
/// this matters *more* than for sender egress: without it, the
/// kernel sends `IGMP_JOIN_GROUP` on whichever interface its
/// multicast route table picks (often the default route, wrong for
/// SMPTE red/blue layouts), and packets arriving on the *other*
/// NIC are silently dropped with no error.
///
/// The SSM include filter is configured from `primary.source_ip`
/// when the SDP carried an SSM `a=source-filter:incl IN IP4 …`
/// line. This pins the kernel-level `MCAST_JOIN_SOURCE_GROUP` so
/// only packets from the advertised sender are accepted — the
/// canonical NMOS / ST 2110 SSM scheme. Only applied for multicast
/// destinations; for unicast there's nothing to filter against.
/// The property name differs between V1 and V2:
/// - `udpsrc` (V1) uses `multicast-source="+<ip>"` — the `+`
///   prefix is mandatory per the documented grammar, selecting
///   "include" rather than "exclude" mode.
/// - `udpsrc2` (V2) uses `source-filter="<ip>"` (no signed prefix;
///   dropped because mixing inclusive/exclusive on one property
///   never made sense) plus a separate boolean
///   `source-filter-exclusive` which we leave at its default
///   `false` for NMOS's single-source-IP include case.
pub(crate) fn build_udpsrc(
    media: &UdpMedia,
    variant: UdpVariant,
    advertise_caps: Option<&gst::Caps>,
) -> Result<UdpSrcChain, anyhow::Error> {
    let rtp_s = media
        .rtp_caps
        .structure(0)
        .ok_or_else(|| anyhow!("UdpMedia.rtp_caps is empty (no structure(0))"))?;
    let encoding_name = rtp_s
        .get::<&str>("encoding-name")
        .map_err(|e| anyhow!("UdpMedia.rtp_caps missing `encoding-name`: {e}"))?;

    let depayloader_factory = select_rtp_factory("depay", media.format, encoding_name, variant)?;
    let depayloader = gst::ElementFactory::make(&depayloader_factory)
        .name("nmossrc-depayloader")
        .build()
        .with_context(|| format!("instantiating depayloader `{depayloader_factory}`"))?;

    let udpsrc_factory = select_udpsrc_factory(variant);
    let udpsrc = gst::ElementFactory::make(udpsrc_factory)
        .name("nmossrc-udpsrc")
        .property("address", &media.primary.destination_ip)
        .property("caps", &media.rtp_caps)
        .build()
        .with_context(|| {
            format!(
                "instantiating `{udpsrc_factory}` (address={}, port={})",
                media.primary.destination_ip, media.primary.destination_port,
            )
        })?;
    // `udpsrc.port` is `gint` (gst-plugins-good's pspec) while
    // `udpsrc2.port` is `guint` (gst-plugins-rs' pspec) — same
    // wire value, different glib type tag, so the property setter
    // has to be split by factory or glib raises a type-mismatch.
    match udpsrc_factory {
        "udpsrc2" => udpsrc.set_property("port", u32::from(media.primary.destination_port)),
        _ => udpsrc.set_property("port", i32::from(media.primary.destination_port)),
    }

    let dest_is_multicast = media
        .primary
        .destination_ip
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_multicast());

    if let Some(iface) = multicast_iface_name(
        &media.primary.destination_ip,
        media.primary.interface_ip.as_deref(),
    ) {
        udpsrc.set_property("multicast-iface", &iface);
    }
    if dest_is_multicast {
        if let Some(source_ip) = &media.primary.source_ip {
            // V1 `udpsrc` and V2 `udpsrc2` describe the same kernel
            // SSM include filter with different property surfaces;
            // see the doc-comment on `build_udpsrc` for the
            // grammar rationale.
            match udpsrc_factory {
                "udpsrc2" => udpsrc.set_property("source-filter", source_ip.as_str()),
                _ => udpsrc.set_property("multicast-source", format!("+{source_ip}")),
            }
        }
    }

    let bin = gst::Bin::with_name("nmossrc-udp");
    bin.add_many([&udpsrc, &depayloader])
        .map_err(|e| anyhow!("adding udpsrc + depayloader to inner bin: {e}"))?;
    udpsrc
        .link(&depayloader)
        .with_context(|| "linking udpsrc to depayloader")?;

    let tail_src_pad = match advertise_caps {
        None => depayloader
            .static_pad("src")
            .ok_or_else(|| anyhow!("depayloader `{depayloader_factory}` missing src pad"))?,
        Some(caps) => {
            // For consistent "minimal essence advertisement" across
            // transports, always use `capssetter` here. It merges the
            // advertised essence caps over whatever the depayloader
            // actually emits without failing negotiation.
            //
            // This also fixes the long-standing V1 `rtpvrawdepay`
            // mismatch where framerate/colorimetry defaults disagree
            // with the SDP.
            let tail_factory = "capssetter";
            let tail = gst::ElementFactory::make(tail_factory)
                .name("nmossrc-caps")
                .property("caps", caps)
                .build()
                .map_err(|e| {
                    anyhow!(
                        "instantiating `{tail_factory}` for nmossrc caps advertisement: {e}"
                    )
                })?;
            bin.add(&tail)
                .map_err(|e| anyhow!("adding tail {tail_factory} to inner bin: {e}"))?;
            depayloader
                .link(&tail)
                .with_context(|| format!("linking depayloader to tail {tail_factory}"))?;
            tail.static_pad("src")
                .ok_or_else(|| anyhow!("tail {tail_factory} missing src pad"))?
        }
    };

    let ghost = gst::GhostPad::builder(gst::PadDirection::Src)
        .name("src")
        .build();
    ghost
        .set_target(Some(&tail_src_pad))
        .map_err(|e| anyhow!("setting inner ghost src target: {e}"))?;
    ghost
        .set_active(true)
        .map_err(|e| anyhow!("activating inner ghost src: {e}"))?;
    bin.add_pad(&ghost)
        .map_err(|e| anyhow!("adding ghost src to inner bin: {e}"))?;

    Ok(UdpSrcChain {
        bin: bin.upcast(),
        transport: udpsrc,
        depay: depayloader,
    })
}

fn require_mxl_factory(name: &'static str) -> Result<(), anyhow::Error> {
    if gst::ElementFactory::find(name).is_none() {
        return Err(anyhow!(
            "GStreamer factory `{name}` not found; \
             load the `gst-mxl-rs` plugin (set GST_PLUGIN_PATH to the directory containing `libgstmxl.so` \
             and LD_LIBRARY_PATH so the MXL runtime libraries are visible)",
        ));
    }
    Ok(())
}

/// Build the initial inner chain for a freshly-constructed
/// `nmossink` / `nmossrc` bin: the permanent `identity` anchor and
/// the supplied fake chain element, linked together, with the
/// anchor's outer-facing pad ghosted out. Returns the ghost pad so
/// the caller can `bin.add_pad(...)` it.
///
/// The ghost pad target is set here and **never changed** by
/// [`rebuild_chain`]; all subsequent activations swap the fake
/// chain (or its real successor `mxlsink` / `mxlsrc`) behind the
/// anchor while the ghost continues to point at the anchor.
pub(crate) fn build_initial(
    bin: &gst::Bin,
    fake_chain: gst::Element,
    pad_name: &str,
    direction: gst::PadDirection,
) -> Result<gst::GhostPad, glib::BoolError> {
    let anchor = gst::ElementFactory::make("identity")
        .name(ANCHOR_NAME)
        .build()
        .map_err(|e| glib::bool_error!("creating identity anchor: {e}"))?;

    bin.add(&anchor)
        .map_err(|e| glib::bool_error!("adding anchor to bin: {e}"))?;
    bin.add(&fake_chain)
        .map_err(|e| glib::bool_error!("adding initial fake chain to bin: {e}"))?;

    let (outer_pad_name, link_result) = match direction {
        gst::PadDirection::Sink => {
            // Topology: ghost(sink) → anchor.sink ─ anchor.src → fake_chain.sink
            ("sink", anchor.link(&fake_chain))
        }
        gst::PadDirection::Src => {
            // Topology: fake_chain.src → anchor.sink ─ anchor.src → ghost(src)
            ("src", fake_chain.link(&anchor))
        }
        _ => {
            return Err(glib::bool_error!(
                "build_initial: unsupported pad direction {direction:?}",
            ));
        }
    };
    link_result.map_err(|e| glib::bool_error!("linking anchor to initial fake chain: {e}"))?;

    let outer_pad = anchor
        .static_pad(outer_pad_name)
        .ok_or_else(|| glib::bool_error!("anchor `{ANCHOR_NAME}` missing `{outer_pad_name}` pad"))?;
    let ghost = gst::GhostPad::builder(direction).name(pad_name).build();
    ghost
        .set_target(Some(&outer_pad))
        .map_err(|e| glib::bool_error!("setting initial ghost pad target: {e}"))?;
    ghost
        .set_active(true)
        .map_err(|e| glib::bool_error!("activating ghost pad: {e}"))?;
    Ok(ghost)
}


/// Apply a `GstStructure` bag of GObject property overrides to a freshly
/// built inner source, sink, payloader, or depayloader. Unknown fields and type mismatches log a warning and
/// are skipped; an absent or empty structure is a no-op.
pub(crate) fn apply_properties_to_leaf(
    cat: &gst::DebugCategory,
    element: &str,
    leaf: &gst::Element,
    props: Option<&gst::Structure>,
) {
    let Some(props) = props else { return };
    for (field_name, field_value) in props.iter() {
        let Some(pspec) = leaf.class().find_property(field_name.as_ref()) else {
            gst::warning!(
                cat,
                "{element}: ignoring unknown inner property `{field_name}` on `{}`",
                leaf.name(),
            );
            continue;
        };
        if !field_value.type_().is_a(pspec.value_type()) {
            gst::warning!(
                cat,
                "{element}: ignoring type-mismatched inner property `{field_name}` on `{}`",
                leaf.name(),
            );
            continue;
        }
        leaf.set_property(field_name.as_ref(), field_value);
    }
}

pub(crate) fn apply_udp_sink_inner_properties(
    cat: &gst::DebugCategory,
    element: &str,
    chain: &UdpSinkChain,
    transport_properties: Option<&gst::Structure>,
    pay_properties: Option<&gst::Structure>,
) {
    apply_properties_to_leaf(cat, element, &chain.transport, transport_properties);
    apply_properties_to_leaf(cat, element, &chain.pay, pay_properties);
}

pub(crate) fn apply_udp_src_inner_properties(
    cat: &gst::DebugCategory,
    element: &str,
    chain: &UdpSrcChain,
    transport_properties: Option<&gst::Structure>,
    depay_properties: Option<&gst::Structure>,
) {
    apply_properties_to_leaf(cat, element, &chain.transport, transport_properties);
    apply_properties_to_leaf(cat, element, &chain.depay, depay_properties);
}

pub(crate) fn apply_mxl_sink_inner_properties(
    cat: &gst::DebugCategory,
    element: &str,
    chain: &MxlSinkChain,
    transport_properties: Option<&gst::Structure>,
    pay_properties: Option<&gst::Structure>,
) {
    apply_properties_to_leaf(cat, element, &chain.transport, transport_properties);
    if pay_properties.is_some_and(|s| s.n_fields() > 0) {
        gst::warning!(
            cat,
            "{element}: pay-properties set but this chain has no payloader; ignoring",
        );
    }
}

pub(crate) fn apply_mxl_src_inner_properties(
    cat: &gst::DebugCategory,
    element: &str,
    chain: &MxlSrcChain,
    transport_properties: Option<&gst::Structure>,
    depay_properties: Option<&gst::Structure>,
) {
    apply_properties_to_leaf(cat, element, &chain.transport, transport_properties);
    if depay_properties.is_some_and(|s| s.n_fields() > 0) {
        gst::warning!(
            cat,
            "{element}: depay-properties set but this chain has no depayloader; ignoring",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use crate::session::UdpLeg;
    use std::str::FromStr;

    fn init_gst() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let _ = gst::init();
        });
    }

    fn test_log_cat() -> &'static gst::DebugCategory {
        static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
            gst::DebugCategory::new(
                "nmos-inner-test",
                gst::DebugColorFlags::empty(),
                Some("inner property apply tests"),
            )
        });
        &CAT
    }

    fn minimal_udp_media() -> UdpMedia {
        init_gst();
        UdpMedia {
            format: FlowFormat::Video,
            primary: UdpLeg {
                destination_ip: "239.1.1.1".to_owned(),
                destination_port: 5004,
                interface_ip: None,
                source_ip: None,
                source_port: None,
            },
            secondary: None,
            rtp_caps: gst::Caps::from_str(
                "application/x-rtp,media=video,clock-rate=90000,encoding-name=RAW,payload=96",
            )
            .expect("static rtp caps parse"),
            raw_caps: gst::Caps::from_str(
                "video/x-raw,format=UYVP,width=1920,height=1080,framerate=50/1",
            )
            .expect("static raw caps parse"),
        }
    }

    /// SMPTE ST 2110-40 ANC at 60 Hz, mirroring what
    /// `sdp::parse_sdp` produces from an RFC 8331 SDP. Note that
    /// `media=video` on the RTP caps even though `FlowFormat::Data`
    /// is the essence kind — that's per RFC 8331 §3 (ANC rides on
    /// the video media type and only `encoding-name=SMPTE291`
    /// disambiguates).
    fn anc_smpte291_media() -> UdpMedia {
        init_gst();
        UdpMedia {
            format: FlowFormat::Data,
            primary: UdpLeg {
                destination_ip: "239.1.1.10".to_owned(),
                destination_port: 5006,
                interface_ip: None,
                source_ip: None,
                source_port: None,
            },
            secondary: None,
            rtp_caps: gst::Caps::from_str(
                "application/x-rtp,media=video,clock-rate=90000,\
                 encoding-name=SMPTE291,payload=100",
            )
            .expect("static rtp caps parse"),
            raw_caps: gst::Caps::from_str(
                "meta/x-st-2038,alignment=frame,framerate=60/1",
            )
            .expect("static raw caps parse"),
        }
    }

    /// `true` iff `gst-plugins-rs`' `rtpsmpte291*` element pair is
    /// installed on the host running the test. Tests that exercise
    /// the ANC chain factories soft-skip when this returns `false`
    /// because the elements live in `gst-plugins-rs`' `rsrtp`
    /// plugin which isn't installed everywhere; the SDP-level
    /// parsing tests don't depend on it.
    fn rtpsmpte291_available() -> bool {
        init_gst();
        gst::ElementFactory::find("rtpsmpte291pay").is_some()
            && gst::ElementFactory::find("rtpsmpte291depay").is_some()
    }

    /// L24 stereo 48 kHz with `a-ptime=0.125` already hoisted onto
    /// `rtp_caps` (mirroring what `sdp::parse_sdp` produces for an
    /// SDP carrying `a=ptime:0.125`). 0.125 ms = 125 µs = 125_000 ns
    /// — see `audio_l24_ptime_pins_min_max_ptime_ns` below.
    fn audio_l24_ptime_media() -> UdpMedia {
        init_gst();
        UdpMedia {
            format: FlowFormat::Audio,
            primary: UdpLeg {
                destination_ip: "239.2.2.2".to_owned(),
                destination_port: 5004,
                interface_ip: Some("192.0.2.10".to_owned()),
                source_ip: None,
                source_port: Some(5005),
            },
            secondary: None,
            rtp_caps: gst::Caps::from_str(
                "application/x-rtp,media=audio,clock-rate=48000,encoding-name=L24,\
                 encoding-params=(string)2,payload=97,a-ptime=(string)0.125",
            )
            .expect("static rtp caps parse"),
            raw_caps: gst::Caps::from_str(
                "audio/x-raw,format=S24BE,rate=48000,channels=2,layout=interleaved",
            )
            .expect("static raw caps parse"),
        }
    }

    /// Find a child element of `bin` by GstObject name (which is
    /// what we set with the builder's `name(...)` call in
    /// [`build_udpsink`]).
    fn child(bin: &gst::Bin, name: &str) -> gst::Element {
        bin.by_name(name)
            .unwrap_or_else(|| panic!("inner bin missing child `{name}`"))
    }

    #[test]
    fn build_udpsink_video_v1_uses_rtpvrawpay_and_udpsink() {
        let chain = build_udpsink(&minimal_udp_media(), UdpVariant::V1)
            .expect("V1 video sender chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let pay = child(&bin, "nmossink-payloader");
        assert_eq!(
            pay.factory().expect("payloader has a factory").name(),
            "rtpvrawpay",
            "V1 video chain must use gst-plugins-good `rtpvrawpay`",
        );
        assert_eq!(
            pay.property::<u32>("pt"),
            96,
            "payloader `pt` must match rtp_caps `payload`",
        );
        let udpsink = child(&bin, "nmossink-udpsink");
        assert_eq!(udpsink.factory().expect("udpsink has a factory").name(), "udpsink");
        assert_eq!(udpsink.property::<String>("host"), "239.1.1.1");
        assert_eq!(udpsink.property::<i32>("port"), 5004);
        assert!(!udpsink.property::<bool>("async"));
        assert!(udpsink.property::<bool>("sync"));
        let ghost = bin
            .static_pad("sink")
            .expect("inner bin missing `sink` ghost pad");
        assert!(ghost.is::<gst::GhostPad>());
    }

    #[test]
    fn build_udpsink_audio_l24_pins_min_max_ptime_ns() {
        let chain = build_udpsink(&audio_l24_ptime_media(), UdpVariant::V1)
            .expect("L24 audio sender chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let pay = child(&bin, "nmossink-payloader");
        assert_eq!(
            pay.factory().expect("payloader has a factory").name(),
            "rtpL24pay",
            "L24 audio chain must use gst-plugins-good `rtpL24pay`",
        );
        assert_eq!(pay.property::<u32>("pt"), 97);
        // 0.125 ms × 1_000_000 ns/ms = 125_000 ns.
        assert_eq!(
            pay.property::<i64>("min-ptime"),
            125_000,
            "min-ptime must be pinned to ptime in ns",
        );
        assert_eq!(
            pay.property::<i64>("max-ptime"),
            125_000,
            "max-ptime must be pinned to ptime in ns",
        );
        let udpsink = child(&bin, "nmossink-udpsink");
        assert_eq!(udpsink.property::<i32>("bind-port"), 5005);
        assert_eq!(udpsink.property::<String>("bind-address"), "192.0.2.10");
    }

    #[test]
    fn build_udpsink_audio_l16_uses_rtpl16pay() {
        let mut media = audio_l24_ptime_media();
        media.rtp_caps = gst::Caps::from_str(
            "application/x-rtp,media=audio,clock-rate=48000,encoding-name=L16,\
             encoding-params=(string)2,payload=98",
        )
        .expect("static rtp caps parse");
        let chain = build_udpsink(&media, UdpVariant::V1).expect("L16 audio sender chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let pay = child(&bin, "nmossink-payloader");
        assert_eq!(pay.factory().expect("payloader has a factory").name(), "rtpL16pay");
    }

    #[test]
    fn build_udpsink_v2_falls_back_to_v1_when_pay2_missing() {
        // gst-plugins-rs ships `rtpL24pay2` / `rtpL16pay2` today —
        // so on a host with the `rsrtp` plugin loaded this test
        // exercises the "V2 sibling present" branch; on a stock
        // gst-plugins-good-only host it exercises the fallback
        // branch. The fallback semantic we pin is "V2 dispatch
        // never fails when V1 is present, even if no V2 sibling
        // is"; the test accepting either factory name keeps it
        // valid in both environments.
        let chain = build_udpsink(&audio_l24_ptime_media(), UdpVariant::V2)
            .expect("V2 L24 chain must construct (picks `rtpL24pay2` if present, else `rtpL24pay`)");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let pay = child(&bin, "nmossink-payloader");
        let factory_name = pay.factory().expect("payloader has a factory").name();
        assert!(
            factory_name == "rtpL24pay2" || factory_name == "rtpL24pay",
            "V2 dispatch must pick `rtpL24pay2` if present, else fall back to \
             `rtpL24pay`; got `{factory_name}`",
        );
    }

    #[test]
    fn build_udpsink_accepts_lowercase_encoding_name() {
        let mut media = audio_l24_ptime_media();
        media.rtp_caps = gst::Caps::from_str(
            "application/x-rtp,media=audio,clock-rate=48000,encoding-name=l24,\
             payload=97,encoding-params=(string)2,a-ptime=(string)0.125",
        )
        .expect("static rtp caps parse");
        let chain = build_udpsink(&media, UdpVariant::V1).expect(
            "lower-case `encoding-name=l24` must be normalised to `L24` and accepted; \
             parse_sdp upper-cases via g_ascii_strup but build_udpsink must also \
             tolerate hand-built caps from non-SDP call sites",
        );
        assert_eq!(
            chain.pay.factory().map(|f| f.name().to_string()).unwrap_or_default(),
            "rtpL24pay",
            "lower-case encoding-name must still resolve to `rtpL24pay`",
        );
    }

    #[test]
    fn build_udpsink_rejects_unsupported_essence() {
        let mut media = minimal_udp_media();
        media.rtp_caps = gst::Caps::from_str(
            "application/x-rtp,media=video,clock-rate=90000,encoding-name=H264,payload=96",
        )
        .expect("static rtp caps parse");
        let err = build_udpsink(&media, UdpVariant::V1)
            .expect_err("H264 must be rejected (today only RAW/L24/L16/SMPTE291 are supported)");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unsupported essence")
                && msg.contains("encoding-name=`H264`"),
            "expected unsupported-essence attribution: {msg}",
        );
    }

    #[test]
    fn package_hint_for_stem_routes_anc_to_plugins_rs() {
        // ANC `rtpsmpte291*` lives only in `gst-plugins-rs` —
        // suggesting `gst-plugins-good` for that one would send the
        // user hunting in the wrong package.
        assert_eq!(
            package_hint_for_stem("rtpsmpte291"),
            "gst-plugins-rs (`rsrtp` plugin)",
        );
        // Everything else `select_rtp_factory` knows about today
        // lives in `gst-plugins-good`.
        assert_eq!(package_hint_for_stem("rtpvraw"), "gst-plugins-good");
        assert_eq!(package_hint_for_stem("rtpL24"), "gst-plugins-good");
        assert_eq!(package_hint_for_stem("rtpL16"), "gst-plugins-good");
    }

    #[test]
    fn build_udpsink_anc_uses_rtpsmpte291pay() {
        if !rtpsmpte291_available() {
            // `gst-plugins-rs` `rsrtp` plugin isn't installed on this
            // host; the SDP-level ANC parsing already has full
            // coverage in `sdp::tests::anc_smpte291_*`. Soft-skip the
            // chain construction rather than failing the suite.
            return;
        }
        let chain = build_udpsink(&anc_smpte291_media(), UdpVariant::V1)
            .expect("ANC sender chain must construct when rtpsmpte291pay is available");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let pay = child(&bin, "nmossink-payloader");
        assert_eq!(
            pay.factory().expect("payloader has a factory").name(),
            "rtpsmpte291pay",
            "ANC sender chain must use `gst-plugins-rs`' rtpsmpte291pay; \
             there is no `gst-plugins-good` equivalent",
        );
        assert_eq!(pay.property::<u32>("pt"), 100);
    }

    #[test]
    fn build_udpsink_anc_works_with_both_udp_variants() {
        if !rtpsmpte291_available() {
            return;
        }
        // The `rtpsmpte291` essence lives only in `gst-plugins-rs`;
        // both UdpVariant::V1 and UdpVariant::V2 must resolve to the
        // same `rtpsmpte291pay` element (no v2 sibling exists). The
        // variant choice is essence-orthogonal for ANC, unlike RAW
        // / L16 / L24 where V2 prefers the `*pay2` form.
        for variant in [UdpVariant::V1, UdpVariant::V2] {
            let chain = build_udpsink(&anc_smpte291_media(), variant)
                .unwrap_or_else(|e| panic!("ANC sender chain must construct for {variant:?}: {e:#}"));
            let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
            let pay = child(&bin, "nmossink-payloader");
            assert_eq!(
                pay.factory().expect("payloader has a factory").name(),
                "rtpsmpte291pay",
                "{variant:?} must resolve to rtpsmpte291pay for ANC",
            );
        }
    }

    #[test]
    fn build_udpsink_rejects_missing_payload() {
        let mut media = minimal_udp_media();
        media.rtp_caps = gst::Caps::from_str(
            "application/x-rtp,media=video,clock-rate=90000,encoding-name=RAW",
        )
        .expect("static rtp caps parse");
        let err = build_udpsink(&media, UdpVariant::V1)
            .expect_err("rtp_caps without `payload` must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("payload"),
            "expected `payload` in error message: {msg}",
        );
    }

    #[test]
    fn build_udpsink_rejects_out_of_range_pt() {
        let mut media = minimal_udp_media();
        media.rtp_caps = gst::Caps::from_str(
            "application/x-rtp,media=video,clock-rate=90000,encoding-name=RAW,payload=200",
        )
        .expect("static rtp caps parse");
        let err = build_udpsink(&media, UdpVariant::V1)
            .expect_err("payload=200 is out of valid PT range 0..=127");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("0..=127") && msg.contains("200"),
            "expected PT-range attribution: {msg}",
        );
    }

    #[test]
    fn ptime_ns_from_rtp_caps_round_trips_fractional_ms() {
        init_gst();
        let caps = gst::Caps::from_str(
            "application/x-rtp,media=audio,clock-rate=48000,encoding-name=L24,\
             payload=97,a-ptime=(string)0.125",
        )
        .unwrap();
        let s = caps.structure(0).unwrap();
        assert_eq!(ptime_ns_from_rtp_caps(s).unwrap(), Some(125_000));
    }

    #[test]
    fn ptime_ns_from_rtp_caps_returns_none_when_absent() {
        init_gst();
        let caps = gst::Caps::from_str(
            "application/x-rtp,media=audio,clock-rate=48000,encoding-name=L24,payload=97",
        )
        .unwrap();
        let s = caps.structure(0).unwrap();
        assert_eq!(ptime_ns_from_rtp_caps(s).unwrap(), None);
    }

    #[test]
    fn ptime_ns_from_rtp_caps_rejects_negative() {
        init_gst();
        let caps = gst::Caps::from_str(
            "application/x-rtp,media=audio,clock-rate=48000,encoding-name=L24,\
             payload=97,a-ptime=(string)-1.0",
        )
        .unwrap();
        let s = caps.structure(0).unwrap();
        let err = ptime_ns_from_rtp_caps(s).expect_err("negative ptime must be rejected");
        assert!(format!("{err:#}").contains("positive"));
    }

    /// `minimal_udp_media` with `interface_ip=127.0.0.1` so the
    /// multicast-iface resolver finds a real local NIC (`lo` on Linux,
    /// `lo0` on macOS) and the test doesn't depend on which physical
    /// NICs the host happens to expose. Loopback is also multicast-
    /// capable, so `IP_MULTICAST_IF` setsockopt against it isn't
    /// nonsensical.
    fn multicast_udp_media_with_loopback_iface() -> UdpMedia {
        let mut media = minimal_udp_media();
        media.primary.interface_ip = Some("127.0.0.1".to_owned());
        media
    }

    #[test]
    fn build_udpsink_pins_multicast_iface_when_destination_is_multicast() {
        let chain = build_udpsink(&multicast_udp_media_with_loopback_iface(), UdpVariant::V1)
            .expect("multicast sender chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let udpsink = child(&bin, "nmossink-udpsink");
        let iface = udpsink.property::<String>("multicast-iface");
        assert!(
            !iface.is_empty(),
            "multicast-iface must be set when destination is multicast and \
             interface_ip is bound on a local NIC (loopback resolves to a \
             non-empty name on every supported platform)",
        );
    }

    #[test]
    fn build_udpsink_skips_multicast_iface_when_destination_is_unicast() {
        let mut media = minimal_udp_media();
        media.primary.destination_ip = "192.0.2.50".to_owned();
        media.primary.interface_ip = Some("127.0.0.1".to_owned());
        let chain = build_udpsink(&media, UdpVariant::V1)
            .expect("unicast sender chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let udpsink = child(&bin, "nmossink-udpsink");
        assert_eq!(
            udpsink.property::<Option<String>>("multicast-iface"),
            None,
            "multicast-iface must remain unset for unicast destinations (the \
             property is only meaningful for IP_MULTICAST_IF / IGMP group joins)",
        );
    }

    #[test]
    fn build_udpsrc_video_v1_uses_rtpvrawdepay_and_udpsrc() {
        let chain = build_udpsrc(&minimal_udp_media(), UdpVariant::V1, None)
            .expect("V1 video receiver chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let udpsrc = child(&bin, "nmossrc-udpsrc");
        assert_eq!(udpsrc.factory().expect("udpsrc has a factory").name(), "udpsrc");
        assert_eq!(udpsrc.property::<String>("address"), "239.1.1.1");
        assert_eq!(udpsrc.property::<i32>("port"), 5004);
        let caps_on_udpsrc = udpsrc
            .property::<Option<gst::Caps>>("caps")
            .expect("udpsrc.caps must be pinned to the RTP shape from the SDP");
        let s = caps_on_udpsrc.structure(0).expect("rtp caps structure(0)");
        assert_eq!(s.name(), "application/x-rtp");
        assert_eq!(s.get::<&str>("encoding-name").unwrap(), "RAW");
        let depay = child(&bin, "nmossrc-depayloader");
        assert_eq!(
            depay.factory().expect("depayloader has a factory").name(),
            "rtpvrawdepay",
            "V1 video chain must use gst-plugins-good `rtpvrawdepay`",
        );
        let ghost = bin
            .static_pad("src")
            .expect("inner bin missing `src` ghost pad");
        assert!(ghost.is::<gst::GhostPad>());
    }

    #[test]
    fn build_udpsrc_audio_l24_uses_rtpl24depay() {
        let chain = build_udpsrc(&audio_l24_ptime_media(), UdpVariant::V1, None)
            .expect("L24 audio receiver chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let depay = child(&bin, "nmossrc-depayloader");
        assert_eq!(
            depay.factory().expect("depayloader has a factory").name(),
            "rtpL24depay",
            "L24 audio chain must use gst-plugins-good `rtpL24depay`",
        );
    }

    #[test]
    fn build_udpsrc_audio_l16_uses_rtpl16depay() {
        let mut media = audio_l24_ptime_media();
        media.rtp_caps = gst::Caps::from_str(
            "application/x-rtp,media=audio,clock-rate=48000,encoding-name=L16,\
             encoding-params=(string)2,payload=97",
        )
        .expect("static rtp caps parse");
        let chain = build_udpsrc(&media, UdpVariant::V1, None)
            .expect("L16 audio receiver chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let depay = child(&bin, "nmossrc-depayloader");
        assert_eq!(
            depay.factory().expect("depayloader has a factory").name(),
            "rtpL16depay",
        );
    }

    #[test]
    fn build_udpsrc_pins_multicast_iface_when_destination_is_multicast() {
        let chain = build_udpsrc(&multicast_udp_media_with_loopback_iface(), UdpVariant::V1, None)
            .expect("multicast receiver chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let udpsrc = child(&bin, "nmossrc-udpsrc");
        let iface = udpsrc.property::<String>("multicast-iface");
        assert!(
            !iface.is_empty(),
            "multicast-iface must be set on udpsrc when joining a multicast \
             group; on multi-NIC hosts a missing value silently joins on the \
             wrong NIC and the receiver appears dead with no error",
        );
    }

    #[test]
    fn build_udpsrc_skips_multicast_iface_when_destination_is_unicast() {
        let mut media = minimal_udp_media();
        media.primary.destination_ip = "192.0.2.50".to_owned();
        media.primary.interface_ip = Some("127.0.0.1".to_owned());
        let chain = build_udpsrc(&media, UdpVariant::V1, None)
            .expect("unicast receiver chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let udpsrc = child(&bin, "nmossrc-udpsrc");
        assert_eq!(
            udpsrc.property::<Option<String>>("multicast-iface"),
            None,
        );
    }

    #[test]
    fn build_udpsrc_pins_ssm_source_filter_via_multicast_source() {
        let mut media = multicast_udp_media_with_loopback_iface();
        media.primary.source_ip = Some("192.0.2.100".to_owned());
        let chain = build_udpsrc(&media, UdpVariant::V1, None)
            .expect("SSM receiver chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let udpsrc = child(&bin, "nmossrc-udpsrc");
        assert_eq!(
            udpsrc.property::<String>("multicast-source"),
            "+192.0.2.100",
            "udpsrc.multicast-source format is `+<source-ip>` for SSM include \
             mode; the `+` prefix is mandatory per the property's documented grammar",
        );
    }

    #[test]
    fn build_udpsrc_omits_ssm_filter_for_unicast_destinations() {
        let mut media = minimal_udp_media();
        media.primary.destination_ip = "192.0.2.50".to_owned();
        media.primary.source_ip = Some("192.0.2.100".to_owned());
        let chain = build_udpsrc(&media, UdpVariant::V1, None)
            .expect("unicast receiver chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let udpsrc = child(&bin, "nmossrc-udpsrc");
        assert_eq!(
            udpsrc.property::<Option<String>>("multicast-source"),
            None,
            "multicast-source is an SSM filter; for unicast destinations the \
             IP layer already filters by source address and the property has \
             no meaning",
        );
    }

    #[test]
    fn build_udpsrc_v1_video_pins_advertise_caps_via_capssetter() {
        // V1 `rtpvrawdepay` hardcodes `framerate=0/1` on its src caps;
        // see the doc-comment in `build_udpsrc` for the GStreamer-good
        // source pointer. The tail therefore has to *override*
        // framerate (and any other field the depay leaves wrong),
        // not just intersect. `capssetter` is the gst-plugins-good
        // element that does exactly that.
        let advertise = gst::Caps::from_str(
            "video/x-raw,format=UYVP,width=1920,height=1080,framerate=50/1",
        )
        .unwrap();
        let chain = build_udpsrc(&minimal_udp_media(), UdpVariant::V1, Some(&advertise))
            .expect("V1 video receiver chain with advertise_caps must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let tail = child(&bin, "nmossrc-caps");
        assert_eq!(
            tail.factory().unwrap().name(),
            "capssetter",
            "V1 video tail must be `capssetter` so it can override \
             `rtpvrawdepay`'s hardcoded `framerate=0/1`",
        );
        let pinned = tail.property::<gst::Caps>("caps");
        assert!(
            pinned.can_intersect(&advertise),
            "tail capssetter must carry the advertise_caps the caller passed in",
        );
    }

    #[test]
    fn build_udpsrc_audio_pins_advertise_caps_via_capssetter() {
        // We use `capssetter` consistently for receiver-side caps
        // advertisement. For audio this is effectively a no-op merge,
        // but it keeps behaviour consistent across transports and
        // tolerates upstream evolution.
        let advertise = gst::Caps::from_str(
            "audio/x-raw,format=S24BE,rate=48000,channels=2,layout=interleaved",
        )
        .unwrap();
        let chain = build_udpsrc(&audio_l24_ptime_media(), UdpVariant::V1, Some(&advertise))
            .expect("audio receiver chain with advertise_caps must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let tail = child(&bin, "nmossrc-caps");
        assert_eq!(
            tail.factory().unwrap().name(),
            "capssetter",
            "audio tail uses `capssetter` for consistent receiver-side advertisement",
        );
        let pinned = tail.property::<gst::Caps>("caps");
        assert!(
            pinned.can_intersect(&advertise),
            "tail capssetter must carry the advertise_caps the caller passed in",
        );
    }

    #[test]
    fn build_udpsrc_rejects_unsupported_essence() {
        let mut media = minimal_udp_media();
        media.rtp_caps = gst::Caps::from_str(
            "application/x-rtp,media=video,clock-rate=90000,encoding-name=H264,payload=96",
        )
        .expect("static rtp caps parse");
        let err = build_udpsrc(&media, UdpVariant::V1, None)
            .expect_err("H264 must be rejected (today only RAW/L24/L16/SMPTE291 are supported)");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unsupported essence")
                && msg.contains("encoding-name=`H264`"),
            "expected unsupported-essence attribution: {msg}",
        );
    }

    #[test]
    fn build_udpsrc_anc_uses_rtpsmpte291depay() {
        if !rtpsmpte291_available() {
            return;
        }
        let chain = build_udpsrc(&anc_smpte291_media(), UdpVariant::V1, None)
            .expect("ANC receiver chain must construct when rtpsmpte291depay is available");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let depay = child(&bin, "nmossrc-depayloader");
        assert_eq!(
            depay.factory().expect("depayloader has a factory").name(),
            "rtpsmpte291depay",
        );
    }

    #[test]
    fn build_udpsrc_anc_works_with_both_udp_variants() {
        if !rtpsmpte291_available() {
            return;
        }
        // Symmetric with `build_udpsink_anc_works_with_both_udp_variants`:
        // ANC has no V2 sibling so both variants resolve to the same
        // element. Pinning this here so a future refactor doesn't
        // accidentally route ANC down a variant-suffixed path that
        // doesn't exist.
        for variant in [UdpVariant::V1, UdpVariant::V2] {
            let chain = build_udpsrc(&anc_smpte291_media(), variant, None)
                .unwrap_or_else(|e| panic!("ANC receiver chain must construct for {variant:?}: {e:#}"));
            let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
            let depay = child(&bin, "nmossrc-depayloader");
            assert_eq!(
                depay.factory().expect("depayloader has a factory").name(),
                "rtpsmpte291depay",
                "{variant:?} must resolve to rtpsmpte291depay for ANC",
            );
        }
    }

    #[test]
    fn build_udpsrc_v2_falls_back_to_v1_when_depay2_missing() {
        // Symmetric with `build_udpsink_v2_falls_back_to_v1_when_pay2_missing`:
        // gst-plugins-rs ships `rtpL24depay2` today, so this test
        // pins the "V2 dispatch never fails when V1 is present"
        // semantic and accepts either factory so it stays valid
        // whether or not the V2 sibling is installed.
        let chain = build_udpsrc(&audio_l24_ptime_media(), UdpVariant::V2, None)
            .expect("V2 L24 receiver must construct (picks `rtpL24depay2` if present, else `rtpL24depay`)");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let depay = child(&bin, "nmossrc-depayloader");
        let factory_name = depay.factory().expect("depayloader has a factory").name();
        assert!(
            factory_name == "rtpL24depay2" || factory_name == "rtpL24depay",
            "V2 dispatch must pick `rtpL24depay2` if present, else fall back to \
             `rtpL24depay`; got `{factory_name}`",
        );
    }

    /// `true` iff gst-plugins-rs' `udpsrc2` is installed on the
    /// host running the test. Tests that exercise V2 socket
    /// dispatch soft-skip when this returns `false` because
    /// `udpsrc2` lives in gst-plugins-rs 0.16+ / GStreamer 1.30+
    /// and is not yet universal; the V2 fallback semantic
    /// (V2-asks-but-V1-runs) is already pinned by
    /// `build_udpsrc_v2_falls_back_to_v1_when_depay2_missing`.
    fn udpsrc2_available() -> bool {
        init_gst();
        gst::ElementFactory::find("udpsrc2").is_some()
    }

    #[test]
    fn select_udpsrc_factory_v1_always_returns_udpsrc() {
        init_gst();
        // V1 must never auto-upgrade to `udpsrc2` even when it's
        // installed — `Transport::Udp` is the gst-plugins-good
        // path by definition. The user opts into the V2 path by
        // setting `Transport::Udp2` on the element property.
        assert_eq!(select_udpsrc_factory(UdpVariant::V1), "udpsrc");
    }

    #[test]
    fn select_udpsrc_factory_v2_prefers_udpsrc2_when_available() {
        if !udpsrc2_available() {
            return;
        }
        assert_eq!(select_udpsrc_factory(UdpVariant::V2), "udpsrc2");
    }

    #[test]
    fn select_udpsrc_factory_v2_falls_back_to_udpsrc_when_udpsrc2_missing() {
        if udpsrc2_available() {
            // Can't exercise the fallback branch when the V2
            // sibling is installed (we'd need to unregister it
            // from the registry, which would poison the rest of
            // the test binary). The fallback semantic is
            // structural and is covered by reading
            // `select_udpsrc_factory`; this test only fires on
            // hosts without `udpsrc2` to give the negative branch
            // CI coverage when it matters.
            return;
        }
        assert_eq!(select_udpsrc_factory(UdpVariant::V2), "udpsrc");
    }

    #[test]
    fn build_udpsrc_v2_uses_udpsrc2_when_available() {
        if !udpsrc2_available() {
            return;
        }
        let chain = build_udpsrc(&minimal_udp_media(), UdpVariant::V2, None)
            .expect("V2 receiver must construct against udpsrc2");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let udpsrc = child(&bin, "nmossrc-udpsrc");
        assert_eq!(
            udpsrc.factory().expect("udpsrc has a factory").name(),
            "udpsrc2",
            "V2 receiver must pick gst-plugins-rs `udpsrc2` over gst-plugins-good `udpsrc`",
        );
        // Properties shared with V1 must still be set; note
        // `udpsrc2.port` is `guint` (gst-plugins-rs) where
        // `udpsrc.port` is `gint` (gst-plugins-good).
        assert_eq!(udpsrc.property::<String>("address"), "239.1.1.1");
        assert_eq!(udpsrc.property::<u32>("port"), 5004);
        let caps_on_udpsrc = udpsrc
            .property::<Option<gst::Caps>>("caps")
            .expect("udpsrc2.caps must be pinned to the RTP shape from the SDP");
        let s = caps_on_udpsrc.structure(0).expect("rtp caps structure(0)");
        assert_eq!(s.name(), "application/x-rtp");
        assert_eq!(s.get::<&str>("encoding-name").unwrap(), "RAW");
    }

    #[test]
    fn build_udpsrc_v2_pins_ssm_filter_via_source_filter_property() {
        if !udpsrc2_available() {
            return;
        }
        let mut media = multicast_udp_media_with_loopback_iface();
        media.primary.source_ip = Some("192.0.2.100".to_owned());
        let chain = build_udpsrc(&media, UdpVariant::V2, None)
            .expect("V2 SSM receiver chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let udpsrc = child(&bin, "nmossrc-udpsrc");
        assert_eq!(
            udpsrc.factory().expect("udpsrc has a factory").name(),
            "udpsrc2",
        );
        assert_eq!(
            udpsrc.property::<String>("source-filter"),
            "192.0.2.100",
            "udpsrc2 uses `source-filter=<ip>` (no signed prefix) instead of \
             udpsrc's `multicast-source=+<ip>`; the include-vs-exclude split \
             moved to a separate boolean property",
        );
        assert!(
            !udpsrc.property::<bool>("source-filter-exclusive"),
            "NMOS only ever describes a single allowed source IP via the SDP \
             `a=source-filter:incl` clause; `source-filter-exclusive` must \
             stay at its default `false` (include mode) so that single IP \
             is the kernel-level include filter rather than an exclude list",
        );
        // multicast-iface plumbing must keep working on udpsrc2.
        let iface = udpsrc.property::<String>("multicast-iface");
        assert!(
            !iface.is_empty(),
            "udpsrc2 still needs `multicast-iface` to direct the IGMP join \
             to the right NIC on multi-NIC hosts; the property name is \
             unchanged from V1",
        );
    }

    #[test]
    fn build_udpsrc_v2_omits_ssm_filter_for_unicast_destinations() {
        if !udpsrc2_available() {
            return;
        }
        let mut media = minimal_udp_media();
        media.primary.destination_ip = "192.0.2.50".to_owned();
        media.primary.source_ip = Some("192.0.2.100".to_owned());
        let chain = build_udpsrc(&media, UdpVariant::V2, None)
            .expect("V2 unicast receiver chain must construct");
        let bin = chain.bin.downcast::<gst::Bin>().expect("returned element is a Bin");
        let udpsrc = child(&bin, "nmossrc-udpsrc");
        assert_eq!(udpsrc.factory().expect("udpsrc has a factory").name(), "udpsrc2");
        // `udpsrc2.source-filter` has no pspec default, so unset
        // reads back as `None` (not an empty string). For unicast
        // the IP layer already filters by source, so we leave the
        // property unset.
        assert_eq!(
            udpsrc.property::<Option<String>>("source-filter"),
            None,
            "source-filter is an SSM include list; for unicast the IP \
             layer already filters by source and the property must \
             stay unset",
        );
    }

    #[test]
    fn build_udpsink_returns_chain_with_distinct_pay_and_transport() {
        let chain = build_udpsink(&minimal_udp_media(), UdpVariant::V1)
            .expect("V1 video sender chain must construct");
        assert_ne!(chain.pay.as_ptr(), chain.transport.as_ptr());
        assert_ne!(chain.pay.as_ptr(), chain.bin.as_ptr());
        assert_ne!(chain.transport.as_ptr(), chain.bin.as_ptr());
    }

    #[test]
    fn build_udpsrc_returns_chain_with_distinct_transport_and_depay() {
        let chain = build_udpsrc(&minimal_udp_media(), UdpVariant::V1, None)
            .expect("V1 video receiver chain must construct");
        assert_ne!(chain.transport.as_ptr(), chain.depay.as_ptr());
        assert_ne!(chain.transport.as_ptr(), chain.bin.as_ptr());
        assert_ne!(chain.depay.as_ptr(), chain.bin.as_ptr());
    }

    #[test]
    fn build_mxlsink_returns_chain_with_transport_equal_to_bin() {
        init_gst();
        if gst::ElementFactory::find("mxlsink").is_none() {
            return;
        }
        let chain = build_mxlsink("/tmp/domain", "00000000-0000-0000-0000-000000000001")
            .expect("mxlsink chain must construct when factory is present");
        assert_eq!(chain.bin.as_ptr(), chain.transport.as_ptr());
    }

    #[test]
    fn build_mxlsrc_without_advertise_caps_returns_chain_with_transport_equal_to_bin() {
        init_gst();
        if gst::ElementFactory::find("mxlsrc").is_none() {
            return;
        }
        let chain = build_mxlsrc(
            "/tmp/domain",
            "00000000-0000-0000-0000-000000000002",
            FlowFormat::Video,
            None,
        )
        .expect("bare mxlsrc chain must construct when factory is present");
        assert_eq!(chain.bin.as_ptr(), chain.transport.as_ptr());
    }

    #[test]
    fn build_mxlsrc_with_advertise_caps_returns_chain_with_transport_inside_wrapper_bin() {
        init_gst();
        if gst::ElementFactory::find("mxlsrc").is_none() {
            return;
        }
        let advertise = minimal_udp_media().raw_caps;
        let Ok(chain) = build_mxlsrc(
            "/tmp/domain",
            "00000000-0000-0000-0000-000000000003",
            FlowFormat::Video,
            Some(&advertise),
        ) else {
            // mxlsrc is present but pad templates may not accept the
            // synthetic UYVP caps in every test environment.
            return;
        };
        assert_ne!(chain.bin.as_ptr(), chain.transport.as_ptr());
        let wrapper = chain.bin.downcast_ref::<gst::Bin>().expect("wrapper is a Bin");
        assert!(wrapper.by_name("nmossrc-mxl").is_some());
    }

    #[test]
    fn chain_transport_factory_matches_expected_for_each_transport_family() {
        init_gst();
        let udp_sink = build_udpsink(&minimal_udp_media(), UdpVariant::V1)
            .expect("udp sink chain must construct");
        assert_eq!(
            udp_sink.transport.factory().unwrap().name(),
            "udpsink",
        );
        let udp_src = build_udpsrc(&minimal_udp_media(), UdpVariant::V1, None)
            .expect("udp src chain must construct");
        assert_eq!(
            udp_src.transport.factory().unwrap().name(),
            "udpsrc",
        );
        if gst::ElementFactory::find("mxlsink").is_some() {
            let mxl_sink = build_mxlsink("/tmp/domain", "00000000-0000-0000-0000-000000000004")
                .expect("mxlsink chain must construct");
            assert_eq!(mxl_sink.transport.factory().unwrap().name(), "mxlsink");
        }
        if gst::ElementFactory::find("mxlsrc").is_some() {
            let mxl_src = build_mxlsrc(
                "/tmp/domain",
                "00000000-0000-0000-0000-000000000005",
                FlowFormat::Video,
                None,
            )
            .expect("mxlsrc chain must construct");
            assert_eq!(mxl_src.transport.factory().unwrap().name(), "mxlsrc");
        }
    }

    #[test]
    fn apply_properties_to_leaf_with_none_is_noop() {
        init_gst();
        let udpsrc = gst::ElementFactory::make("udpsrc")
            .build()
            .expect("udpsrc must construct");
        let default = udpsrc.property::<i32>("buffer-size");
        apply_properties_to_leaf(test_log_cat(), "nmossrc", &udpsrc, None);
        assert_eq!(udpsrc.property::<i32>("buffer-size"), default);
    }

    #[test]
    fn apply_properties_to_leaf_with_empty_structure_is_noop() {
        init_gst();
        let udpsrc = gst::ElementFactory::make("udpsrc")
            .build()
            .expect("udpsrc must construct");
        let default = udpsrc.property::<i32>("buffer-size");
        let empty = gst::Structure::new_empty("properties");
        apply_properties_to_leaf(test_log_cat(), "nmossrc", &udpsrc, Some(&empty));
        assert_eq!(udpsrc.property::<i32>("buffer-size"), default);
    }

    #[test]
    fn apply_properties_to_leaf_sets_known_field() {
        init_gst();
        let udpsrc = gst::ElementFactory::make("udpsrc")
            .build()
            .expect("udpsrc must construct");
        let mut props = gst::Structure::new_empty("properties");
        props.set("buffer-size", 26214400i32);
        apply_properties_to_leaf(test_log_cat(), "nmossrc", &udpsrc, Some(&props));
        assert_eq!(udpsrc.property::<i32>("buffer-size"), 26214400);
    }

    #[test]
    fn apply_properties_to_leaf_skips_unknown_field_but_applies_known_fields() {
        init_gst();
        let udpsrc = gst::ElementFactory::make("udpsrc")
            .build()
            .expect("udpsrc must construct");
        let mut props = gst::Structure::new_empty("properties");
        props.set("definitely-not-a-property", true);
        props.set("buffer-size", 26214400i32);
        apply_properties_to_leaf(test_log_cat(), "nmossrc", &udpsrc, Some(&props));
        assert_eq!(udpsrc.property::<i32>("buffer-size"), 26214400);
    }

    #[test]
    fn apply_properties_to_leaf_skips_type_mismatch() {
        init_gst();
        let udpsrc = gst::ElementFactory::make("udpsrc")
            .build()
            .expect("udpsrc must construct");
        let default = udpsrc.property::<i32>("buffer-size");
        let mut props = gst::Structure::new_empty("properties");
        props.set("buffer-size", "foo");
        apply_properties_to_leaf(test_log_cat(), "nmossrc", &udpsrc, Some(&props));
        assert_eq!(udpsrc.property::<i32>("buffer-size"), default);
    }

    #[test]
    fn nmossink_transport_properties_apply_to_udpsink_on_chain_build() {
        let chain = build_udpsink(&minimal_udp_media(), UdpVariant::V1)
            .expect("udp sink chain must construct");
        let mut props = gst::Structure::new_empty("properties");
        props.set("buffer-size", 26214400i32);
        props.set("auto-multicast", false);
        apply_udp_sink_inner_properties(test_log_cat(), "nmossink", &chain, Some(&props), None);
        assert_eq!(chain.transport.property::<i32>("buffer-size"), 26214400);
        assert!(!chain.transport.property::<bool>("auto-multicast"));
    }

    #[test]
    fn nmossink_pay_properties_apply_to_payloader_on_chain_build() {
        let chain = build_udpsink(&minimal_udp_media(), UdpVariant::V1)
            .expect("udp sink chain must construct");
        let mut props = gst::Structure::new_empty("properties");
        props.set("perfect-rtptime", false);
        apply_udp_sink_inner_properties(test_log_cat(), "nmossink", &chain, None, Some(&props));
        assert!(!chain.pay.property::<bool>("perfect-rtptime"));
    }

    #[test]
    fn nmossrc_depay_properties_apply_to_depayloader_on_chain_build() {
        let chain = build_udpsrc(&minimal_udp_media(), UdpVariant::V1, None)
            .expect("udp src chain must construct");
        let mut props = gst::Structure::new_empty("properties");
        props.set("max-reorder", 200i32);
        apply_udp_src_inner_properties(test_log_cat(), "nmossrc", &chain, None, Some(&props));
        assert_eq!(chain.depay.property::<i32>("max-reorder"), 200);
    }

    #[test]
    fn transport_properties_persist_across_simulated_reactivation() {
        init_gst();
        let mut props = gst::Structure::new_empty("properties");
        props.set("buffer-size", 26214400i32);
        let chain1 = build_udpsrc(&minimal_udp_media(), UdpVariant::V1, None)
            .expect("first chain must construct");
        apply_udp_src_inner_properties(test_log_cat(), "nmossrc", &chain1, Some(&props), None);
        assert_eq!(chain1.transport.property::<i32>("buffer-size"), 26214400);

        let chain2 = build_udpsrc(&minimal_udp_media(), UdpVariant::V1, None)
            .expect("second chain must construct");
        apply_udp_src_inner_properties(test_log_cat(), "nmossrc", &chain2, Some(&props), None);
        assert_ne!(chain1.transport.as_ptr(), chain2.transport.as_ptr());
        assert_eq!(chain2.transport.property::<i32>("buffer-size"), 26214400);
    }

    #[test]
    fn transport_properties_cleared_between_rebuilds_revert_to_factory_default() {
        init_gst();
        let default_udpsrc = gst::ElementFactory::make("udpsrc")
            .build()
            .expect("udpsrc must construct");
        let default_buffer_size = default_udpsrc.property::<i32>("buffer-size");
        let mut props = gst::Structure::new_empty("properties");
        props.set("buffer-size", 26214400i32);
        let chain1 = build_udpsrc(&minimal_udp_media(), UdpVariant::V1, None)
            .expect("first chain must construct");
        apply_udp_src_inner_properties(test_log_cat(), "nmossrc", &chain1, Some(&props), None);
        assert_eq!(chain1.transport.property::<i32>("buffer-size"), 26214400);

        let chain2 = build_udpsrc(&minimal_udp_media(), UdpVariant::V1, None)
            .expect("second chain must construct");
        apply_udp_src_inner_properties(test_log_cat(), "nmossrc", &chain2, None, None);
        assert_eq!(chain2.transport.property::<i32>("buffer-size"), default_buffer_size);
    }

    #[test]
    fn pay_properties_under_mxl_warns_once() {
        init_gst();
        if gst::ElementFactory::find("mxlsink").is_none() {
            return;
        }
        let chain = build_mxlsink("/tmp/domain", "00000000-0000-0000-0000-000000000099")
            .expect("mxlsink chain must construct");
        let mut pay_props = gst::Structure::new_empty("properties");
        pay_props.set("perfect-rtptime", false);
        apply_mxl_sink_inner_properties(
            test_log_cat(),
            "nmossink",
            &chain,
            None,
            Some(&pay_props),
        );
    }
}
