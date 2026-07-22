// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! NMOS activation observability for `nmossrc` / `nmossink`.
//!
//! Updates the read-only `active` property and posts an element message on
//! the pipeline bus when the data plane becomes live or dormant.

use std::sync::{LazyLock, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "nmos-connection-active",
        gst::DebugColorFlags::empty(),
        Some("NMOS connection-active observability"),
    )
});

const MESSAGE_STRUCTURE: &str = "nmos-activation";

/// Set connection-active state and report the transition to the application.
///
/// Updates the `active` property when the boolean changes and always posts a
/// `nmos-activation` element message (including real→real reconfigurations
/// where `active` stays true).
pub(crate) fn set_connection_active(
    element: &gst::Element,
    active: &Mutex<bool>,
    new_active: bool,
    resource_name: &str,
    reason: &'static str,
) {
    let notify = {
        let mut stored = active.lock().expect("connection active lock");
        let changed = *stored != new_active;
        *stored = new_active;
        changed
    };
    if notify && element.find_property("active").is_some() {
        element.notify("active");
    }

    let structure = gst::Structure::builder(MESSAGE_STRUCTURE)
        .field("active", new_active)
        .field("resource-name", resource_name)
        .field("reason", reason)
        .build();
    let message = gst::message::Element::builder(structure)
        .src(element)
        .build();
    if let Err(e) = element.post_message(message) {
        gst::warning!(
            CAT,
            obj = element,
            "failed to post {MESSAGE_STRUCTURE} element message \
             (active={new_active}, resource-name={resource_name}, reason={reason}): {e}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::init_gst;

    #[test]
    fn posts_each_nmos_activation_element_message() {
        init_gst();
        let pipeline = gst::Pipeline::default();
        let sink = gst::ElementFactory::make("fakesink")
            .build()
            .expect("fakesink");
        pipeline.add(&sink).expect("add fakesink");
        let bus = pipeline.bus().expect("pipeline bus");

        let active = Mutex::new(false);
        set_connection_active(sink.upcast_ref(), &active, true, "video-a", "activate");
        assert!(*active.lock().unwrap());
        set_connection_active(sink.upcast_ref(), &active, true, "video-a", "activate");
        assert!(*active.lock().unwrap());
        set_connection_active(sink.upcast_ref(), &active, false, "video-a", "deactivate");
        assert!(!*active.lock().unwrap());

        for (expected_active, expected_reason) in [
            (true, "activate"),
            (true, "activate"),
            (false, "deactivate"),
        ] {
            let msg = bus
                .timed_pop(gst::ClockTime::from_seconds(1))
                .expect("bus message");
            let gst::MessageView::Element(element_msg) = msg.view() else {
                panic!("expected Element message, got {:?}", msg.type_());
            };
            let structure = element_msg.structure().expect("structure");
            assert_eq!(structure.name(), MESSAGE_STRUCTURE);
            assert_eq!(structure.get::<bool>("active").ok(), Some(expected_active));
            assert_eq!(structure.get::<&str>("resource-name").ok(), Some("video-a"));
            assert_eq!(structure.get::<&str>("reason").ok(), Some(expected_reason));
        }
        assert!(bus.timed_pop(gst::ClockTime::ZERO).is_none());
    }
}
