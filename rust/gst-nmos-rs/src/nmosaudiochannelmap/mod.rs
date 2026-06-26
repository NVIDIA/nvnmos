// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! NMOS IS-08 channel mapping element (`nmosaudiochannelmap`).

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

mod caps;
mod imp;
mod internals;
mod pad;

glib::wrapper! {
    pub struct NmosAudioChannelMap(ObjectSubclass<imp::NmosAudioChannelMap>)
        @extends gst::Bin, gst::Element, gst::Object,
        @implements gst::ChildProxy;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    pad::register_types()?;
    gst::Element::register(
        Some(plugin),
        "nmosaudiochannelmap",
        gst::Rank::NONE,
        NmosAudioChannelMap::static_type(),
    )
}
