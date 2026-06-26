// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration smoke: `nmosaudiochannelmap` registers and exposes expected properties.
//!
//! Set `GST_PLUGIN_PATH` to the directory containing `libgstnmos.so` when running
//! this test outside the smoke script. Pad request/link behaviour is covered by
//! `gst-inspect-1.0` in `scripts/is08-channelmap-smoke.sh`.

use gstreamer as gst;
use gstreamer::prelude::*;

#[test]
fn nmosaudiochannelmap_registers_and_inspects() {
    gst::init().expect("gst::init");
    let Some(factory) = gst::ElementFactory::find("nmosaudiochannelmap") else {
        eprintln!(
            "skip: nmosaudiochannelmap not in registry — set GST_PLUGIN_PATH to gst-nmos-rs target/debug"
        );
        return;
    };

    assert_eq!(factory.klass(), "Filter/Audio/Network");
    assert_eq!(factory.num_pad_templates(), 2);

    let element = factory.create().build().expect("create element");
    assert!(element.find_property("channelmapping-name").is_some());
    assert!(element.find_property("restrict-routable-inputs").is_some());
    assert!(element.find_property("node-seed").is_some());
}
