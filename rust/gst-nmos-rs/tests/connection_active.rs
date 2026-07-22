// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Regression: auto-activate must not deadlock NULL→READY.
//!
//! `nmossink::activate_inner` used to hold the `settings` mutex across
//! `set_connection_active`, which locks `settings` again for the resource
//! name — a same-thread deadlock that wedged `av_sync_via_udp` in CI. This
//! test drives one auto-activating `nmossink` through NULL→READY with a
//! watchdog so a hang aborts instead of stalling the suite.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{DaemonGuard, init, nvnmosd_skip_reason, require_factories};
use gst::prelude::*;
use gstreamer as gst;
use test_skip::skip;

const HANG_TIMEOUT: Duration = Duration::from_secs(20);

fn nic_ip() -> String {
    if let Ok(ip) = std::env::var("NVNMOS_TEST_NIC_IP") {
        return ip;
    }
    let out = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show"])
        .output();
    if let Ok(out) = out {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 4 && f[1] != "lo" {
                if let Some(ip) = f[3].split('/').next() {
                    return ip.to_owned();
                }
            }
        }
    }
    "127.0.0.1".to_owned()
}

#[test]
fn auto_activate_null_to_ready_does_not_hang() {
    init();
    if let Some(why) = nvnmosd_skip_reason() {
        skip!(why);
    }
    require_factories(&["nmossink", "udpsink", "rtpvrawpay"]);

    let pid = std::process::id();
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = PathBuf::from(format!("/tmp/nvnmos_conn_active_{pid}_{now_nanos}.sock"));
    let _daemon = DaemonGuard::new(socket);
    let uri = _daemon.uri();
    let nic = nic_ip();

    // GStreamer state changes must run on this thread; the watchdog only
    // converts a wedged transition into a hard failure for CI.
    let finished = Arc::new(AtomicBool::new(false));
    {
        let finished = Arc::clone(&finished);
        std::thread::spawn(move || {
            std::thread::sleep(HANG_TIMEOUT);
            if !finished.load(Ordering::SeqCst) {
                eprintln!(
                    "nmossink NULL→READY with auto-activate hung for {HANG_TIMEOUT:?} — \
                     likely settings mutex re-lock in set_connection_active"
                );
                std::process::abort();
            }
        });
    }

    let desc = format!(
        "nmossink name=s daemon-uri=\"{uri}\" transport=udp \
         node-seed=conn-active-test sender-name=video-a auto-activate=true \
         caps=\"video/x-raw,format=UYVP,width=192,height=4,framerate=25/1\" \
         destination-ip={nic} destination-port=15040 source-ip={nic}"
    );
    // Single-element launch returns the element itself, not a Pipeline.
    let sink = gst::parse::launch(&desc).expect("parse nmossink");
    let pipeline = gst::Pipeline::default();
    pipeline.add(&sink).expect("add nmossink");
    let bus = pipeline.bus().expect("bus");

    pipeline.set_state(gst::State::Ready).expect("NULL→READY");
    let (res, ..) = pipeline.state(gst::ClockTime::from_seconds(10));
    res.expect("reached READY");

    assert!(
        sink.property::<bool>("active"),
        "auto-activate READY must set active=true"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_activation = false;
    while std::time::Instant::now() < deadline {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) else {
            continue;
        };
        let gst::MessageView::Element(element_msg) = msg.view() else {
            continue;
        };
        let Some(structure) = element_msg.structure() else {
            continue;
        };
        if structure.name() != "nmos-activation" {
            continue;
        }
        assert_eq!(structure.get::<bool>("active").ok(), Some(true));
        assert_eq!(structure.get::<&str>("resource-name").ok(), Some("video-a"));
        assert_eq!(structure.get::<&str>("reason").ok(), Some("auto-activate"));
        saw_activation = true;
        break;
    }
    assert!(
        saw_activation,
        "expected nmos-activation element message on auto-activate"
    );

    pipeline.set_state(gst::State::Null).expect("READY→NULL");
    finished.store(true, Ordering::SeqCst);
}
