// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Read-only NMOS resource id properties on `nmossink` and `nmossrc`.
//!
//! The stored strings start empty. [`publish`] copies the session's ids
//! and emits `notify` for each property whose value changed. Publishing
//! [`ResourceIds::default`] after the session is gone clears them.

use std::sync::Mutex;

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::daemon::ResourceIds;

pub(crate) fn publish(element: &gst::Element, stored: &Mutex<ResourceIds>, next: &ResourceIds) {
    let changed = {
        let mut stored = stored.lock().unwrap();
        let mut changed = Vec::new();
        assign(&mut stored.node_id, &next.node_id, "node-id", &mut changed);
        assign(
            &mut stored.device_id,
            &next.device_id,
            "device-id",
            &mut changed,
        );
        assign(
            &mut stored.sender_id,
            &next.sender_id,
            "sender-id",
            &mut changed,
        );
        assign(
            &mut stored.source_id,
            &next.source_id,
            "source-id",
            &mut changed,
        );
        assign(&mut stored.flow_id, &next.flow_id, "flow-id", &mut changed);
        assign(
            &mut stored.receiver_id,
            &next.receiver_id,
            "receiver-id",
            &mut changed,
        );
        changed
    };
    for name in changed {
        if element.find_property(name).is_some() {
            element.notify(name);
        }
    }
}

pub(crate) fn get(stored: &Mutex<ResourceIds>, name: &str) -> glib::Value {
    let ids = stored.lock().unwrap();
    match name {
        "node-id" => ids.node_id.to_value(),
        "device-id" => ids.device_id.to_value(),
        "sender-id" => ids.sender_id.to_value(),
        "source-id" => ids.source_id.to_value(),
        "flow-id" => ids.flow_id.to_value(),
        "receiver-id" => ids.receiver_id.to_value(),
        _ => unimplemented!("unknown resource id property {name}"),
    }
}

fn assign(slot: &mut String, new: &str, name: &'static str, changed: &mut Vec<&'static str>) {
    if slot != new {
        *slot = new.to_owned();
        changed.push(name);
    }
}
