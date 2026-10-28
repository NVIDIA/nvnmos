// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nmossink` and `nmossrc` publish the NMOS ids the daemon assigns.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use common::{DaemonGuard, init, nic_ip, nvnmosd_skip_reason, require_factories};
use gst::prelude::*;
use gstreamer as gst;
use test_skip::skip;

const CAPS: &str = "video/x-raw,format=UYVP,width=192,height=4,framerate=25/1";

struct Stop<'a>(&'a gst::Pipeline);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
        let _ = self.0.state(gst::ClockTime::from_seconds(5));
    }
}

fn socket(prefix: &str) -> PathBuf {
    let path = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".sock")
        .tempfile_in(std::env::temp_dir())
        .expect("temp socket")
        .into_temp_path();
    path.keep().expect("keep socket path")
}

fn caps() -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .field("format", "UYVP")
        .field("width", 192i32)
        .field("height", 4i32)
        .field("framerate", gst::Fraction::new(25, 1))
        .build()
}

fn pipeline_of(element: &gst::Element) -> gst::Pipeline {
    let pipeline = gst::Pipeline::default();
    pipeline.add(element).expect("add element");
    pipeline
}

fn bus_lines(pipeline: &gst::Pipeline) -> String {
    let Some(bus) = pipeline.bus() else {
        return String::new();
    };
    let mut lines = Vec::new();
    while let Some(msg) = bus.pop() {
        match msg.view() {
            gst::MessageView::Error(err) => lines.push(format!(
                "error: {} debug={}",
                err.error(),
                err.debug().as_deref().unwrap_or("")
            )),
            gst::MessageView::Warning(warn) => lines.push(format!(
                "warning: {} debug={}",
                warn.error(),
                warn.debug().as_deref().unwrap_or("")
            )),
            _ => {}
        }
    }
    lines.join("\n")
}

fn reach(pipeline: &gst::Pipeline, target: gst::State) {
    if let Err(err) = pipeline.set_state(target) {
        panic!(
            "set_state({target:?}) failed: {err:?}\n{}",
            bus_lines(pipeline)
        );
    }
    let (result, current, pending) = pipeline.state(gst::ClockTime::from_seconds(10));
    if let Err(err) = result {
        panic!(
            "waiting for {target:?} failed: {err:?} (current={current:?}, pending={pending:?})\n{}",
            bus_lines(pipeline)
        );
    }
}

fn watch(element: &gst::Element, names: &[&'static str]) -> Arc<Mutex<Vec<(String, String)>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    for name in names {
        let seen = Arc::clone(&seen);
        let name = *name;
        element.connect_notify(Some(name), move |element, _| {
            seen.lock()
                .unwrap()
                .push((name.to_owned(), element.property::<String>(name)));
        });
    }
    seen
}

fn assert_uuid(value: &str) {
    let bytes = value.as_bytes();
    assert_eq!(bytes.len(), 36, "{value}");
    for (index, byte) in bytes.iter().enumerate() {
        let hyphen = matches!(index, 8 | 13 | 18 | 23);
        assert!(
            hyphen == (*byte == b'-') && (hyphen || byte.is_ascii_hexdigit()),
            "{value}"
        );
    }
}

#[test]
fn sender_and_receiver_share_node_and_device_ids() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&["nmossink", "nmossrc"]);

    let socket = socket("nvnmos_resource_ids_");
    let daemon = DaemonGuard::new(socket);
    let uri = daemon.uri();

    let nic = nic_ip();
    let sink = gst::ElementFactory::make("nmossink")
        .property("daemon-uri", &uri)
        .property("node-seed", "resource-ids")
        .property("sender-name", "video")
        .property("source-ip", &nic)
        .property("destination-ip", "233.252.0.1")
        .property("destination-port", 5004u32)
        .property("caps", caps())
        .build()
        .expect("nmossink");
    let src = gst::ElementFactory::make("nmossrc")
        .property("daemon-uri", &uri)
        .property("node-seed", "resource-ids")
        .property("receiver-name", "video")
        .property("interface-ip", &nic)
        .property("destination-port", 5004u32)
        .property("caps", caps())
        .build()
        .expect("nmossrc");

    let sink_ids = ["node-id", "device-id", "sender-id", "source-id", "flow-id"];
    let src_ids = ["node-id", "device-id", "receiver-id"];
    for name in sink_ids {
        assert_eq!(sink.property::<String>(name), "");
    }
    for name in src_ids {
        assert_eq!(src.property::<String>(name), "");
    }
    let sink_seen = watch(&sink, &sink_ids);
    let src_seen = watch(&src, &src_ids);

    let sink_pipeline = pipeline_of(&sink);
    let src_pipeline = pipeline_of(&src);
    let _stop_sink = Stop(&sink_pipeline);
    let _stop_src = Stop(&src_pipeline);
    reach(&sink_pipeline, gst::State::Ready);
    reach(&src_pipeline, gst::State::Ready);

    let node_id = sink.property::<String>("node-id");
    let device_id = sink.property::<String>("device-id");
    assert_uuid(&node_id);
    assert_uuid(&device_id);
    assert_ne!(node_id, device_id);
    assert_eq!(src.property::<String>("node-id"), node_id);
    assert_eq!(src.property::<String>("device-id"), device_id);

    let sender_id = sink.property::<String>("sender-id");
    let source_id = sink.property::<String>("source-id");
    let flow_id = sink.property::<String>("flow-id");
    let receiver_id = src.property::<String>("receiver-id");
    for id in [
        sender_id.as_str(),
        source_id.as_str(),
        flow_id.as_str(),
        receiver_id.as_str(),
    ] {
        assert_uuid(id);
        assert_ne!(id, node_id.as_str());
        assert_ne!(id, device_id.as_str());
    }
    assert_ne!(sender_id, source_id);
    assert_ne!(sender_id, flow_id);
    assert_ne!(source_id, flow_id);
    assert_ne!(receiver_id, sender_id);

    assert!(
        sink_seen
            .lock()
            .unwrap()
            .iter()
            .any(|(name, value)| name == "sender-id" && value == &sender_id),
        "sender-id notify: {:?}",
        sink_seen.lock().unwrap()
    );
    assert!(
        src_seen
            .lock()
            .unwrap()
            .iter()
            .any(|(name, value)| name == "receiver-id" && value == &receiver_id),
        "receiver-id notify: {:?}",
        src_seen.lock().unwrap()
    );

    reach(&sink_pipeline, gst::State::Null);
    reach(&src_pipeline, gst::State::Null);
    for name in sink_ids {
        assert_eq!(sink.property::<String>(name), "", "{name} after NULL");
    }
    for name in src_ids {
        assert_eq!(src.property::<String>(name), "", "{name} after NULL");
    }
    assert!(
        sink_seen
            .lock()
            .unwrap()
            .iter()
            .any(|(name, value)| name == "flow-id" && value.is_empty())
    );
    assert!(
        src_seen
            .lock()
            .unwrap()
            .iter()
            .any(|(name, value)| name == "device-id" && value.is_empty())
    );
}

#[test]
fn deferred_sender_ids_stay_empty_until_the_sender_is_added() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&["nmossink", "fakesrc"]);

    let socket = socket("nvnmos_resource_ids_deferred_");
    let daemon = DaemonGuard::new(socket);
    let pipeline = gst::parse::launch(&format!(
        "fakesrc num-buffers=1 ! {CAPS} ! nmossink name=sink \
         daemon-uri=\"{}\" node-seed=resource-ids-deferred sender-name=video \
         source-ip={}",
        daemon.uri(),
        nic_ip()
    ))
    .expect("parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("pipeline");
    let _stop = Stop(&pipeline);
    let sink = pipeline.by_name("sink").expect("nmossink");

    reach(&pipeline, gst::State::Ready);
    assert_uuid(&sink.property::<String>("node-id"));
    assert_uuid(&sink.property::<String>("device-id"));
    assert_eq!(sink.property::<String>("sender-id"), "");
    assert_eq!(sink.property::<String>("source-id"), "");
    assert_eq!(sink.property::<String>("flow-id"), "");

    reach(&pipeline, gst::State::Paused);
    assert_uuid(&sink.property::<String>("sender-id"));
    assert_uuid(&sink.property::<String>("source-id"));
    assert_uuid(&sink.property::<String>("flow-id"));
    assert_ne!(
        sink.property::<String>("sender-id"),
        sink.property::<String>("flow-id")
    );

    reach(&pipeline, gst::State::Null);
}
