// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Receive-side, transport-independent AV-sync measurement. Input pads accept
//! raw video/audio and optional ST2038 ANC or CDP captions on a shared timeline.
/**
 * SECTION:element-avsyncanalyzer
 * @see_also: avsyncvideotestsrc, avsyncaudiotestsrc
 *
 * Measures the received white-bar center crossing, audio pip and ancillary
 * caption timing. Request `video`, `audio` and optionally `anc` (ST2038) or
 * `caption` (CEA-708 CDP) sink pads. The input timestamps must share a media
 * clock; use `timestamp-mode=reference` to require matching reference metadata.
 *
 * Posts `avsync-event` and `avsync-summary` element messages with typed
 * GstStructure fields. Summaries are emitted at EOS or transition to READY.
 * Missing events and insufficient timing precision cannot produce a pass.
 * Set `require-anc=true` when ancillary captions are mandatory, including when
 * an upstream extractor creates its ancillary output pad dynamically.
 */
use gst::{glib, prelude::*};
use gstreamer as gst;

pub mod analysis;
mod imp;

glib::wrapper! {
    pub struct AvSyncAnalyzer(ObjectSubclass<imp::AvSyncAnalyzer>)
        @extends gst::Bin, gst::Element, gst::Object,
        @implements gst::ChildProxy;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    imp::TimestampMode::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    gst::Element::register(
        Some(plugin),
        "avsyncanalyzer",
        gst::Rank::NONE,
        AvSyncAnalyzer::static_type(),
    )
}
