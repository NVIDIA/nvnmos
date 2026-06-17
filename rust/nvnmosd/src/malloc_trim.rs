// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Optional glibc `malloc_trim` after node teardown (Linux only).
//!
//! Controlled by `NVNMOSD_MALLOC_TRIM` (default on). Full `malloc_info` XML dumps
//! are opt-in via `NVNMOSD_MALLOC_INFO=1`.

use std::sync::OnceLock;

use crate::env_config::{self, EnvDefault};
use crate::state::{CloseOutcome, State};

/// True unless `NVNMOSD_MALLOC_TRIM` is set to a disabling value (`0`, `false`, `off`, `no`).
pub fn trim_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_config::read_env_bool("NVNMOSD_MALLOC_TRIM", EnvDefault::OptOut))
}

fn info_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_config::read_env_bool("NVNMOSD_MALLOC_INFO", EnvDefault::OptIn))
}

/// After [`State::close_session`] (explicit RPC or session GC).
pub fn maybe_after_close_session(state: &State, outcome: &CloseOutcome) {
    maybe_trim(
        state,
        &outcome.node_seed,
        "close_session",
        outcome.node_destroyed,
    );
}

/// After [`State::remove_resource`].
pub fn maybe_after_remove_resource(state: &State, node_seed: &str) {
    maybe_trim(state, node_seed, "remove_resource", false);
}

/// After [`State::remove_node`].
pub fn maybe_after_remove_node(state: &State, node_seed: &str) {
    maybe_trim(state, node_seed, "remove_node", true);
}

fn maybe_trim(state: &State, node_seed: &str, via: &'static str, node_removed: bool) {
    if !trim_enabled() {
        return;
    }
    let should_trim = node_removed || state.resource_count_for_node(node_seed) == 0;
    if should_trim {
        run(via, node_seed);
    }
}

/// Run `malloc_trim` when enabled. `via` names the RPC that triggered the check
/// (`close_session`, `remove_resource`, or `remove_node`). Safe to call from any
/// thread; does not hold daemon state.
pub fn run(via: &'static str, node_seed: &str) {
    if !trim_enabled() {
        return;
    }
    run_impl(via, node_seed);
}

#[cfg(target_os = "linux")]
fn run_impl(via: &str, node_seed: &str) {
    if info_enabled() {
        log_malloc_info(via, "before");
    }

    // SAFETY: glibc allocator API; no Rust invariants to uphold.
    let released = unsafe { libc::malloc_trim(0) != 0 };

    if info_enabled() {
        log_malloc_info(via, "after");
    }

    tracing::info!(
        via,
        node_seed,
        released,
        "malloc_trim"
    );
}

#[cfg(target_os = "linux")]
fn log_malloc_info(via: &str, phase: &str) {
    // SAFETY: open_memstream/malloc_info/free are standard glibc extensions.
    unsafe {
        let mut buf: *mut libc::c_char = std::ptr::null_mut();
        let mut len: usize = 0;
        let stream = libc::open_memstream(&mut buf, &mut len);
        if stream.is_null() {
            tracing::warn!(via, phase, "malloc_info: open_memstream failed");
            return;
        }
        if libc::malloc_info(0, stream) != 0 {
            libc::fclose(stream);
            libc::free(buf.cast());
            tracing::warn!(via, phase, "malloc_info failed");
            return;
        }
        libc::fclose(stream);
        if buf.is_null() {
            return;
        }
        let info = std::ffi::CStr::from_ptr(buf).to_string_lossy();
        tracing::debug!(via, phase, malloc_info = %info, "malloc_info");
        libc::free(buf.cast());
    }
}

#[cfg(not(target_os = "linux"))]
fn run_impl(_via: &str, node_seed: &str) {
    tracing::debug!(
        node_seed,
        "malloc_trim skipped (not Linux)"
    );
}
