// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! NMOS Sender (`nmossink`) element.

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

mod imp;

glib::wrapper! {
    pub struct NmosSink(ObjectSubclass<imp::NmosSink>) @extends gst::Bin, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "nmossink",
        gst::Rank::NONE,
        NmosSink::static_type(),
    )
}
