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
//!   wrapping `mxlsrc ! capsfilter`) once enough configuration is
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
/// bins. For wrapped sub-bins (e.g. the `mxlsrc ! capsfilter` bin
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
/// `mxlsrc ! capsfilter` sub-bin) once an IS-05 activation pins a
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
pub(crate) fn build_mxlsink(domain_path: &str, flow_id: &str) -> Result<gst::Element, anyhow::Error> {
    require_mxl_factory("mxlsink")?;
    gst::ElementFactory::make("mxlsink")
        .name("nmossink-mxl")
        .property("domain", domain_path)
        .property("flow-id", flow_id)
        .property("async", false)
        .build()
        .with_context(|| {
            format!(
                "instantiating `mxlsink` with domain={domain_path:?}, flow-id={flow_id}"
            )
        })
}

/// Build the inner `mxlsrc` for `nmossrc`. `format` picks which of
/// `video-flow-id` / `audio-flow-id` / `data-flow-id` receives
/// `flow_id`; [`FlowFormat::Unspecified`] is rejected because the
/// caller is responsible for falling back to the fake chain before
/// reaching this helper.
///
/// When `advertise_caps` is `Some`, the returned element is a small
/// `Bin` containing `mxlsrc ! capsfilter caps=advertise_caps` with
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
) -> Result<gst::Element, anyhow::Error> {
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
        return Ok(mxlsrc);
    };

    let capsfilter = gst::ElementFactory::make("capsfilter")
        .name("nmossrc-caps")
        .property("caps", caps)
        .build()
        .map_err(|e| anyhow!("instantiating `capsfilter` for nmossrc caps advertisement: {e}"))?;
    let bin = gst::Bin::with_name("nmossrc-inner");
    bin.add_many([&mxlsrc, &capsfilter])
        .map_err(|e| anyhow!("adding mxlsrc + capsfilter to inner bin: {e}"))?;
    mxlsrc
        .link(&capsfilter)
        .with_context(|| "linking mxlsrc to inner capsfilter")?;
    let capsfilter_src = capsfilter
        .static_pad("src")
        .ok_or_else(|| anyhow!("capsfilter missing src pad"))?;
    let ghost = gst::GhostPad::builder(gst::PadDirection::Src)
        .name("src")
        .build();
    ghost
        .set_target(Some(&capsfilter_src))
        .map_err(|e| anyhow!("setting inner ghost src target: {e}"))?;
    ghost
        .set_active(true)
        .map_err(|e| anyhow!("activating inner ghost src: {e}"))?;
    bin.add_pad(&ghost)
        .map_err(|e| anyhow!("adding ghost src to inner bin: {e}"))?;
    Ok(bin.upcast())
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
