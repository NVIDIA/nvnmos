// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! NMOS Receiver (`nmossrc`) element.

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

mod imp;

glib::wrapper! {
    pub struct NmosSrc(ObjectSubclass<imp::NmosSrc>) @extends gst::Bin, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "nmossrc",
        gst::Rank::NONE,
        NmosSrc::static_type(),
    )
}
