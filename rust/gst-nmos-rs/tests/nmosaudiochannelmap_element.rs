// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration smoke: `nmosaudiochannelmap` registers and exposes expected properties.
//!
//! `common::init()` adds this workspace's `libgstnmos.so` to `GST_PLUGIN_PATH`,
//! so the test runs against the freshly built plugin; it self-skips only when
//! that library has not been built.

mod common;

use common::init;
use gstreamer as gst;
use gstreamer::prelude::*;
use test_skip::skip;

#[test]
fn nmosaudiochannelmap_registers_and_inspects() {
    init();
    let Some(factory) = gst::ElementFactory::find("nmosaudiochannelmap") else {
        skip!("nmosaudiochannelmap not in registry — build libgstnmos.so and set GST_PLUGIN_PATH");
    };

    assert_eq!(factory.klass(), "Filter/Audio/Network");
    assert_eq!(factory.num_pad_templates(), 2);

    let element = factory.create().build().expect("create element");
    assert!(element.find_property("channelmapping-name").is_some());
    assert!(element.find_property("restrict-routable-inputs").is_some());
    assert!(element.find_property("node-seed").is_some());
}
