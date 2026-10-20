// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The GStreamer error an element posts when a state change fails: its code
//! follows the cause, and its debug string gives the reason once.

mod common;

use common::{DaemonGuard, init, nvnmosd_skip_reason, require_factories};
use gst::prelude::*;
use gstreamer as gst;
use test_skip::skip;

fn temp_socket(prefix: &str) -> tempfile::TempPath {
    tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".sock")
        .tempfile_in(std::env::temp_dir())
        .expect("temp socket")
        .into_temp_path()
}

fn channel_map_pipeline(daemon_uri: &str, node_seed: &str) -> gst::Pipeline {
    gst::parse::launch(&format!(
        "audiotestsrc ! audio/x-raw,format=F32LE,rate=48000,channels=2 ! map.sink_0 \
         nmosaudiochannelmap name=map daemon-uri=\"{daemon_uri}\" node-seed={node_seed} \
         channelmapping-name=map sink_0::channels=2 src_0::channels=2 \
         map.src_0 ! fakesink"
    ))
    .expect("parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("pipeline")
}

fn pipeline(description: &str) -> gst::Pipeline {
    gst::parse::launch(description)
        .expect("parse pipeline")
        .downcast::<gst::Pipeline>()
        .expect("pipeline")
}

/// A pipeline around `element` (`nmossink` or `nmossrc`) with `properties`.
/// The elements check these properties before they connect to nvnmosd, so
/// the daemon URI is never used.
fn local_pipeline(element: &str, properties: &str) -> gst::Pipeline {
    let common = "daemon-uri=unix:/nonexistent/nvnmosd.sock node-seed=element-errors-local";
    pipeline(&match element {
        "nmossink" => format!("fakesrc ! nmossink {common} sender-name=sender {properties}"),
        "nmossrc" => format!("nmossrc {common} receiver-name=receiver {properties} ! fakesink"),
        _ => unreachable!("{element}"),
    })
}

fn state_change_error(pipeline: &gst::Pipeline, state: gst::State) -> (gst::glib::Error, String) {
    // A pipeline with sinks to preroll can return ASYNC before the failing
    // element posts its error, so wait for the ERROR message instead.
    let _ = pipeline.set_state(state);
    let msg = pipeline
        .bus()
        .expect("bus")
        .timed_pop_filtered(gst::ClockTime::from_seconds(5), &[gst::MessageType::Error])
        .expect("ERROR message on the bus");
    let gst::MessageView::Error(err) = msg.view() else {
        unreachable!();
    };
    let result = (
        err.error(),
        err.debug().map(|d| d.to_string()).unwrap_or_default(),
    );
    let _ = pipeline.set_state(gst::State::Null);
    result
}

#[test]
fn unreachable_daemon_is_not_found() {
    init();
    require_factories(&["nmosaudiochannelmap", "audiotestsrc", "fakesink"]);
    let socket = temp_socket("nvnmos_element_errors_missing_");
    let uri = format!("unix:{}", socket.display());
    std::fs::remove_file(&socket).expect("remove placeholder socket file");

    let pipeline = channel_map_pipeline(&uri, "element-errors-missing");
    let (error, debug) = state_change_error(&pipeline, gst::State::Ready);
    assert!(
        error.matches(gst::ResourceError::NotFound),
        "{error:?}: {debug}"
    );
    assert_eq!(
        debug.matches("No such file or directory").count(),
        1,
        "{debug}"
    );
}

#[test]
fn missing_node_seed_is_settings() {
    init();
    require_factories(&["nmosaudiochannelmap", "audiotestsrc", "fakesink"]);
    let pipeline = channel_map_pipeline("unix:/nonexistent/nvnmosd.sock", "\"\"");
    let (error, debug) = state_change_error(&pipeline, gst::State::Ready);
    assert!(
        error.matches(gst::ResourceError::Settings),
        "{error:?}: {debug}"
    );
    assert!(debug.contains("`node-seed` is required"), "{debug}");
}

#[test]
fn unreadable_transport_file_path_is_open_read() {
    init();
    require_factories(&["nmossink", "nmossrc", "fakesrc", "fakesink"]);
    for element in ["nmossink", "nmossrc"] {
        let pipeline = local_pipeline(element, "transport-file-path=/nonexistent/transport.sdp");
        let (error, debug) = state_change_error(&pipeline, gst::State::Ready);
        assert!(
            error.matches(gst::ResourceError::OpenRead),
            "{element}: {error:?}: {debug}"
        );
        assert!(
            debug.contains("reading `transport-file-path`"),
            "{element}: {debug}"
        );
    }
}

#[test]
fn unparsable_transport_file_is_settings() {
    init();
    require_factories(&["nmossink", "nmossrc", "fakesrc", "fakesink"]);
    for element in ["nmossink", "nmossrc"] {
        let pipeline = local_pipeline(element, "transport-file=\"not an SDP file\"");
        let (error, debug) = state_change_error(&pipeline, gst::State::Ready);
        assert!(
            error.matches(gst::ResourceError::Settings),
            "{element}: {error:?}: {debug}"
        );
    }
}

#[test]
fn declared_channels_that_contradict_caps_are_negotiation() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&[
        "nmosaudiochannelmap",
        "audiotestsrc",
        "fakesink",
        "audiomixer",
        "audiomixmatrix",
    ]);
    let socket = temp_socket("nvnmos_element_errors_channels_");
    let daemon = DaemonGuard::new(socket.to_path_buf());
    let pipeline = pipeline(&format!(
        "audiotestsrc ! audio/x-raw,format=F32LE,rate=48000,channels=2 ! map.sink_0 \
         nmosaudiochannelmap name=map daemon-uri=\"{}\" node-seed=element-errors-channels \
         channelmapping-name=map sink_0::channels=4 src_0::channels=2 \
         map.src_0 ! fakesink",
        daemon.uri()
    ));
    let (error, debug) = state_change_error(&pipeline, gst::State::Paused);
    assert!(
        error.matches(gst::CoreError::Negotiation),
        "{error:?}: {debug}"
    );
    assert!(debug.contains("declared channels=4"), "{debug}");
}

#[test]
fn channel_map_without_request_pads_is_state_change() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&["nmosaudiochannelmap", "audiomixer", "audiomixmatrix"]);
    let socket = temp_socket("nvnmos_element_errors_no_pads_");
    let daemon = DaemonGuard::new(socket.to_path_buf());
    let map = gst::ElementFactory::make("nmosaudiochannelmap")
        .property("daemon-uri", daemon.uri())
        .property("node-seed", "element-errors-no-pads")
        .property("channelmapping-name", "map")
        .build()
        .expect("nmosaudiochannelmap");
    let pipeline = gst::Pipeline::new();
    pipeline.add(&map).expect("add nmosaudiochannelmap");
    let (error, debug) = state_change_error(&pipeline, gst::State::Paused);
    assert!(
        error.matches(gst::CoreError::StateChange),
        "{error:?}: {debug}"
    );
}

#[test]
fn deferred_sender_with_any_caps_is_negotiation() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&["nmossink", "fakesrc"]);
    let socket = temp_socket("nvnmos_element_errors_any_caps_");
    let daemon = DaemonGuard::new(socket.to_path_buf());
    let pipeline = gst::parse::launch(&format!(
        "fakesrc ! nmossink daemon-uri=\"{}\" node-seed=element-errors-any-caps \
         sender-name=sender",
        daemon.uri()
    ))
    .expect("parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("pipeline");
    let (error, debug) = state_change_error(&pipeline, gst::State::Paused);
    assert!(
        error.matches(gst::CoreError::Negotiation),
        "{error:?}: {debug}"
    );
    assert!(debug.contains("ANY caps"), "{debug}");
}

#[test]
fn rejected_channel_mapping_is_settings_with_reason_once() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&[
        "nmosaudiochannelmap",
        "audiotestsrc",
        "fakesink",
        "audiomixer",
        "audiomixmatrix",
    ]);
    let socket = temp_socket("nvnmos_element_errors_rejected_");
    let daemon = DaemonGuard::new(socket.to_path_buf());

    let first = channel_map_pipeline(&daemon.uri(), "element-errors-rejected");
    first
        .set_state(gst::State::Paused)
        .expect("first pipeline to PAUSED");
    let (result, _, _) = first.state(gst::ClockTime::from_seconds(5));
    result.expect("first pipeline prerolls");

    // nvnmosd rejects a second mapping with the same name on the same node.
    let second = channel_map_pipeline(&daemon.uri(), "element-errors-rejected");
    let (error, debug) = state_change_error(&second, gst::State::Paused);
    first
        .set_state(gst::State::Null)
        .expect("first pipeline to NULL");
    assert!(
        error.matches(gst::ResourceError::Settings),
        "{error:?}: {debug}"
    );
    assert_eq!(
        debug
            .matches(r#"channel mapping name "map" already exists"#)
            .count(),
        1,
        "{debug}"
    );
    assert!(!debug.contains("metadata"), "{debug}");
}
