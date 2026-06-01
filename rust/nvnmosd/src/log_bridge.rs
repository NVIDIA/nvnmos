// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Forward libnvnmos slog output to `tracing`.
//!
//! Installed on every `NodeServer` the daemon creates so the operator sees
//! the C library's diagnostic stream — `Implementation error for sender …`,
//! mDNS browse errors, IS-04 / IS-05 request access logs, and so on —
//! through the same `tracing-subscriber` plumbing as the daemon's own logs.
//!
//! Messages below the level passed to `NodeConfig::log_level` are dropped by
//! libnvnmos before the callback fires, so we set that to a permissive
//! [`LIBNVNMOS_LOG_LEVEL`] and let `tracing`'s subscriber filter further.
//! `RUST_LOG=libnvnmos=trace` (or a `tracing-subscriber` `EnvFilter`
//! directive equivalent) brings the full firehose into view; the default
//! `info` filter shows just warnings and errors.

use nvnmos::{
    LOG_LEVEL_ERROR, LOG_LEVEL_INFO, LOG_LEVEL_VERBOSE, LOG_LEVEL_WARNING, LogMessage,
};
use tracing::{Level, event};

/// `tracing` target used for every event emitted by the bridge.
///
/// Distinct from any Rust module path so operators can filter libnvnmos
/// chatter independently of the daemon's own logs, e.g.
/// `RUST_LOG=nvnmosd=info,libnvnmos=warn`.
pub const TARGET: &str = "libnvnmos";

/// Severity threshold passed to libnvnmos so we never miss something
/// `tracing` would otherwise want to render. Set permissively because the
/// runtime cost of one extra FFI call per dropped message is negligible
/// compared to losing diagnostics. `LOG_LEVEL_DEVEL` is one step further
/// down (asio/websocketpp/mDNS internals) and is left to a future opt-in
/// because it produces a lot of noise.
pub const LIBNVNMOS_LOG_LEVEL: i32 = LOG_LEVEL_VERBOSE;

/// Forward one libnvnmos log message into `tracing`.
///
/// Installed via [`nvnmos::NodeServerBuilder::on_log`]. The callback runs on
/// a libnvnmos worker thread; `tracing` is `Sync` so this is safe.
pub fn forward(msg: &LogMessage<'_>) {
    // Map libnvnmos's seven-step severity scale onto the five `tracing`
    // levels. `LOG_LEVEL_SEVERE` and `LOG_LEVEL_FATAL` collapse into ERROR
    // because `tracing` has nothing more severe; the original level is
    // preserved as a field so post-hoc filtering can recover the
    // distinction if it ever matters.
    let level = match msg.level {
        n if n >= LOG_LEVEL_ERROR => Level::ERROR,
        n if n >= LOG_LEVEL_WARNING => Level::WARN,
        n if n >= LOG_LEVEL_INFO => Level::INFO,
        n if n >= LOG_LEVEL_VERBOSE => Level::DEBUG,
        _ => Level::TRACE,
    };

    // `event!`'s level is required to be a const, so dispatch by arm.
    // libnvnmos messages already carry SLOG_FLF source location in the
    // body, so we don't try to extract it.
    match level {
        Level::ERROR => event!(
            target: TARGET, Level::ERROR,
            libnvnmos_level = msg.level,
            categories = msg.categories,
            "{}", msg.message,
        ),
        Level::WARN => event!(
            target: TARGET, Level::WARN,
            libnvnmos_level = msg.level,
            categories = msg.categories,
            "{}", msg.message,
        ),
        Level::INFO => event!(
            target: TARGET, Level::INFO,
            libnvnmos_level = msg.level,
            categories = msg.categories,
            "{}", msg.message,
        ),
        Level::DEBUG => event!(
            target: TARGET, Level::DEBUG,
            libnvnmos_level = msg.level,
            categories = msg.categories,
            "{}", msg.message,
        ),
        Level::TRACE => event!(
            target: TARGET, Level::TRACE,
            libnvnmos_level = msg.level,
            categories = msg.categories,
            "{}", msg.message,
        ),
    }
}
