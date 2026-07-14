// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Custom request pads with per-pad GObject properties.

use std::sync::LazyLock;

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;

pub(crate) fn register_types() -> Result<(), glib::BoolError> {
    Ok(())
}

glib::wrapper! {
    pub struct NmosAudioChannelMapSinkPad(ObjectSubclass<imp::SinkPad>) @extends gst::GhostPad, gst::ProxyPad, gst::Pad, gst::Object;
}

glib::wrapper! {
    pub struct NmosAudioChannelMapSrcPad(ObjectSubclass<imp::SrcPad>) @extends gst::GhostPad, gst::ProxyPad, gst::Pad, gst::Object;
}

mod imp {
    use super::*;

    #[derive(Debug, Default, Clone)]
    pub struct SinkPadState {
        pub receiver_name: String,
        pub input_id: String,
        pub label: String,
        pub description: String,
        pub channels: u32,
    }

    #[derive(Debug, Default, Clone)]
    pub struct SrcPadState {
        pub sender_name: String,
        pub output_id: String,
        pub label: String,
        pub description: String,
        pub channels: u32,
        pub active_map: Option<gst::Structure>,
    }

    #[derive(Default)]
    pub struct SinkPad {
        state: std::sync::Mutex<SinkPadState>,
        frozen: std::sync::atomic::AtomicBool,
    }

    #[derive(Default)]
    pub struct SrcPad {
        state: std::sync::Mutex<SrcPadState>,
        frozen: std::sync::atomic::AtomicBool,
    }

    macro_rules! pad_frozen_guard {
        ($pad:expr, $nick:expr) => {
            if $pad.frozen.load(std::sync::atomic::Ordering::Acquire) {
                gst::error!(
                    crate::CAT,
                    "pad property `{}` is not writable after fixation",
                    $nick
                );
                return;
            }
        };
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SinkPad {
        const NAME: &'static str = "GstNmosAudioChannelMapSinkPad";
        type Type = super::NmosAudioChannelMapSinkPad;
        type ParentType = gst::GhostPad;
    }

    impl ObjectImpl for SinkPad {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    glib::ParamSpecString::builder("receiver-name")
                        .nick("Receiver name")
                        .blurb(
                            "Caller-chosen Receiver name on this Node; the IS-04 \
                             Receiver id for IS-08 Input `/parent` is derived \
                             from this name and the element's `node-seed` when \
                             non-empty.",
                        )
                        .build(),
                    glib::ParamSpecString::builder("input-id")
                        .nick("Input id")
                        .blurb("IS-08 Input id; empty assigns a default.")
                        .build(),
                    glib::ParamSpecString::builder("label")
                        .nick("IS-08 name")
                        .blurb("IS-08 /properties name (UI label) for this Input.")
                        .build(),
                    glib::ParamSpecString::builder("description")
                        .nick("IS-08 description")
                        .blurb("IS-08 /properties description for this Input.")
                        .build(),
                    glib::ParamSpecUInt::builder("channels")
                        .nick("Channels")
                        .blurb(
                            "0 (default): derive channel count from negotiated audio/x-raw caps \
                             at fixation. >0: declare early; must match negotiated caps when \
                             present.",
                        )
                        .maximum(u32::MAX)
                        .build(),
                ]
            });
            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            let mut state = self.state.lock().unwrap();
            match pspec.name() {
                "receiver-name" => {
                    pad_frozen_guard!(self, "receiver-name");
                    state.receiver_name = value.get().expect("type checked");
                }
                "input-id" => {
                    pad_frozen_guard!(self, "input-id");
                    state.input_id = value.get().expect("type checked");
                }
                "label" => {
                    pad_frozen_guard!(self, "label");
                    state.label = value.get().expect("type checked");
                }
                "description" => {
                    pad_frozen_guard!(self, "description");
                    state.description = value.get().expect("type checked");
                }
                "channels" => {
                    pad_frozen_guard!(self, "channels");
                    state.channels = value.get().expect("type checked");
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            let state = self.state.lock().unwrap();
            match pspec.name() {
                "receiver-name" => state.receiver_name.to_value(),
                "input-id" => state.input_id.to_value(),
                "label" => state.label.to_value(),
                "description" => state.description.to_value(),
                "channels" => state.channels.to_value(),
                _ => unimplemented!(),
            }
        }
    }

    impl GstObjectImpl for SinkPad {}
    impl PadImpl for SinkPad {}
    impl ProxyPadImpl for SinkPad {}
    impl GhostPadImpl for SinkPad {}

    impl SinkPad {
        pub(crate) fn snapshot(&self) -> SinkPadState {
            self.state.lock().unwrap().clone()
        }

        pub(crate) fn freeze(&self) {
            self.frozen
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SrcPad {
        const NAME: &'static str = "GstNmosAudioChannelMapSrcPad";
        type Type = super::NmosAudioChannelMapSrcPad;
        type ParentType = gst::GhostPad;
    }

    impl ObjectImpl for SrcPad {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    glib::ParamSpecString::builder("sender-name")
                        .nick("Sender name")
                        .blurb(
                            "Caller-chosen Sender name on this Node; the IS-04 \
                             Source id for IS-08 Output `/sourceid` is derived\
                             from this name and the element's `node-seed` when \
                             non-empty.",
                        )
                        .build(),
                    glib::ParamSpecString::builder("output-id")
                        .nick("Output id")
                        .blurb("IS-08 Output id; empty assigns a default.")
                        .build(),
                    glib::ParamSpecString::builder("label")
                        .nick("IS-08 name")
                        .blurb("IS-08 /properties name (UI label) for this Output.")
                        .build(),
                    glib::ParamSpecString::builder("description")
                        .nick("IS-08 description")
                        .blurb("IS-08 /properties description for this Output.")
                        .build(),
                    glib::ParamSpecUInt::builder("channels")
                        .nick("Channels")
                        .blurb(
                            "0 (default): derive channel count from negotiated audio/x-raw caps \
                             at fixation. >0: declare early; must match negotiated caps when \
                             present.",
                        )
                        .maximum(u32::MAX)
                        .build(),
                    glib::ParamSpecBoxed::builder::<gst::Structure>("active-map")
                        .nick("Active map")
                        .blurb(
                            "Fixation-time initial /map/active for this Output. A `map` \
                             structure, e.g. `map,0=input0:0,1=input0:1`.",
                        )
                        .build(),
                ]
            });
            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            let mut state = self.state.lock().unwrap();
            match pspec.name() {
                "sender-name" => {
                    pad_frozen_guard!(self, "sender-name");
                    state.sender_name = value.get().expect("type checked");
                }
                "output-id" => {
                    pad_frozen_guard!(self, "output-id");
                    state.output_id = value.get().expect("type checked");
                }
                "label" => {
                    pad_frozen_guard!(self, "label");
                    state.label = value.get().expect("type checked");
                }
                "description" => {
                    pad_frozen_guard!(self, "description");
                    state.description = value.get().expect("type checked");
                }
                "channels" => {
                    pad_frozen_guard!(self, "channels");
                    state.channels = value.get().expect("type checked");
                }
                "active-map" => {
                    pad_frozen_guard!(self, "active-map");
                    state.active_map = value.get::<Option<gst::Structure>>().expect("type checked");
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            let state = self.state.lock().unwrap();
            match pspec.name() {
                "sender-name" => state.sender_name.to_value(),
                "output-id" => state.output_id.to_value(),
                "label" => state.label.to_value(),
                "description" => state.description.to_value(),
                "channels" => state.channels.to_value(),
                "active-map" => state.active_map.to_value(),
                _ => unimplemented!(),
            }
        }
    }

    impl GstObjectImpl for SrcPad {}
    impl PadImpl for SrcPad {}
    impl ProxyPadImpl for SrcPad {}
    impl GhostPadImpl for SrcPad {}

    impl SrcPad {
        pub(crate) fn snapshot(&self) -> SrcPadState {
            self.state.lock().unwrap().clone()
        }

        pub(crate) fn freeze(&self) {
            self.frozen
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

pub(crate) fn sink_pad_templates() -> &'static [gst::PadTemplate] {
    static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
        let caps = gst::Caps::builder("audio/x-raw").build();
        vec![
            gst::PadTemplate::with_gtype(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
                NmosAudioChannelMapSinkPad::static_type(),
            )
            .expect("sink pad template"),
        ]
    });
    TEMPLATES.as_ref()
}

pub(crate) fn src_pad_templates() -> &'static [gst::PadTemplate] {
    static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
        let caps = gst::Caps::builder("audio/x-raw").build();
        vec![
            gst::PadTemplate::with_gtype(
                "src_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Request,
                &caps,
                NmosAudioChannelMapSrcPad::static_type(),
            )
            .expect("src pad template"),
        ]
    });
    TEMPLATES.as_ref()
}
