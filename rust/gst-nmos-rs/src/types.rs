// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GObject enum types and defaults shared between `nmossrc` and
//! `nmossink`.

use gstreamer::glib;

/// Transport family for the inner data path.
///
/// Today only MXL shared-memory transport is wired up. Additional
/// variants (UDP/RTP, nvdsudp/Rivermax) are added by follow-up
/// branches as their inner chains land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, glib::Enum)]
#[repr(i32)]
#[enum_type(name = "GstNmosTransport")]
pub enum Transport {
    /// MXL shared-memory transport (`mxlsrc` and `mxlsink`).
    #[default]
    #[enum_value(name = "MXL shared-memory transport", nick = "mxl")]
    Mxl = 0,
}

/// Default `daemon-uri` if the user doesn't override it.
///
/// Matches `nvnmosd`'s own default UDS path so a developer can run
/// both with zero configuration.
pub(crate) const DEFAULT_DAEMON_URI: &str = "unix:/tmp/nvnmosd.sock";
