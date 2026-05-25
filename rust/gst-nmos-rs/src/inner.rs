// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Helpers for the inner data path of `nmossink` / `nmossrc`.
//!
//! Each element is a `GstBin` with a single ghost pad re-targeted at
//! whichever inner element is currently in use: a placeholder
//! `fakesink` / `fakesrc` while the configuration is incomplete, or
//! a real `mxlsink` / `mxlsrc` once a Domain path and a Flow id are
//! pinned.
//!
//! This module owns the swap mechanics (remove the previous inner,
//! add the new one, re-target the ghost pad, sync state with parent)
//! plus the factory helpers for both kinds of inner element.

use anyhow::{Context, anyhow};
use gst::glib;
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::types::FlowFormat;

/// Replace the inner element behind `ghost`. If there's already a
/// target, it's transitioned to `NULL` and removed from `bin` before
/// `new_inner` is added and ghosted. `new_inner` is left at the
/// state it inherits from the bin via `sync_state_with_parent`.
pub(crate) fn swap_inner(
    bin: &gst::Bin,
    ghost: &gst::GhostPad,
    new_inner: &gst::Element,
    pad_name: &str,
) -> Result<(), anyhow::Error> {
    let old = ghost.target().and_then(|p| p.parent_element());

    ghost
        .set_target(None::<&gst::Pad>)
        .map_err(|e| anyhow!("clearing ghost pad target: {e}"))?;

    if let Some(old) = old {
        let _ = old.set_state(gst::State::Null);
        bin.remove(&old).with_context(|| {
            format!("removing previous inner element `{}`", old.name())
        })?;
    }

    bin.add(new_inner)
        .with_context(|| format!("adding inner element `{}` to bin", new_inner.name()))?;

    let new_pad = new_inner.static_pad(pad_name).ok_or_else(|| {
        anyhow!(
            "inner element `{}` has no `{pad_name}` pad",
            new_inner.name(),
        )
    })?;
    ghost
        .set_target(Some(&new_pad))
        .map_err(|e| anyhow!("setting ghost pad target: {e}"))?;

    new_inner
        .sync_state_with_parent()
        .with_context(|| format!("syncing state of `{}` with parent", new_inner.name()))?;

    Ok(())
}

/// Build the `nmossink` placeholder data path: a `fakesink` so the
/// element looks valid in the pipeline before configuration is
/// complete (it sinks any caps and drops everything).
pub(crate) fn build_placeholder_sink() -> Result<gst::Element, anyhow::Error> {
    gst::ElementFactory::make("fakesink")
        .name("nmossink-placeholder")
        .property("sync", true)
        .property("async", false)
        .build()
        .map_err(|e| anyhow!("creating fakesink placeholder: {e}"))
}

/// Build the `nmossrc` placeholder data path: an infinite live
/// `fakesrc`.
pub(crate) fn build_placeholder_src() -> Result<gst::Element, anyhow::Error> {
    let elem = gst::ElementFactory::make("fakesrc")
        .name("nmossrc-placeholder")
        .property("is-live", true)
        .build()
        .map_err(|e| anyhow!("creating fakesrc placeholder: {e}"))?;
    elem.set_property_from_str("num-buffers", "-1");
    Ok(elem)
}

/// Build the inner `mxlsink` for `nmossink`. Fails with a clear
/// message if the `mxl` plugin isn't on `GST_PLUGIN_PATH` or the
/// element factory rejects the supplied properties.
pub(crate) fn build_mxlsink(domain_path: &str, flow_id: &str) -> Result<gst::Element, anyhow::Error> {
    require_mxl_factory("mxlsink")?;
    gst::ElementFactory::make("mxlsink")
        .name("nmossink-mxl")
        .property("domain", domain_path)
        .property("flow-id", flow_id)
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
/// caller is responsible for falling back to the placeholder before
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
                "build_mxlsrc called with FlowFormat::Unspecified; caller should have built a placeholder",
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

/// Build the initial inner element + activated ghost pad for an
/// element whose bin has just been constructed. Returns the ghost
/// pad so the caller can add it to the bin's pad list.
pub(crate) fn build_initial(
    bin: &gst::Bin,
    placeholder: gst::Element,
    pad_name: &str,
    direction: gst::PadDirection,
) -> Result<gst::GhostPad, glib::BoolError> {
    bin.add(&placeholder)
        .map_err(|e| glib::bool_error!("adding placeholder to bin: {e}"))?;
    let inner_pad = placeholder
        .static_pad(pad_name)
        .ok_or_else(|| glib::bool_error!("placeholder missing `{pad_name}` pad"))?;
    let ghost = gst::GhostPad::builder(direction)
        .name(pad_name)
        .build();
    ghost
        .set_target(Some(&inner_pad))
        .map_err(|e| glib::bool_error!("setting initial ghost pad target: {e}"))?;
    ghost
        .set_active(true)
        .map_err(|e| glib::bool_error!("activating ghost pad: {e}"))?;
    Ok(ghost)
}
