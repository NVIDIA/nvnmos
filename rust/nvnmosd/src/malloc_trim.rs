// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Optional glibc `malloc_trim` after node teardown (Linux only).
//!
//! Controlled by `NVNMOSD_MALLOC_TRIM` (default on). Full `malloc_info` XML dumps
//! are opt-in via `NVNMOSD_MALLOC_INFO=1`.

use std::sync::OnceLock;

const ENABLE_TOKENS: &[&str] = &["1", "true", "TRUE", "yes", "YES", "on", "ON"];
const DISABLE_TOKENS: &[&str] = &["0", "false", "FALSE", "off", "OFF", "no", "NO"];

/// How an env var is interpreted when unset vs when set to a known token.
enum EnvDefault {
    /// Unset → enabled; known disable tokens → disabled; anything else → enabled.
    OptOut,
    /// Unset → disabled; known enable tokens → enabled; anything else → disabled.
    OptIn,
}

fn read_env_bool(name: &str, default: EnvDefault) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim();
            match default {
                EnvDefault::OptOut => !DISABLE_TOKENS.contains(&value),
                EnvDefault::OptIn => ENABLE_TOKENS.contains(&value),
            }
        }
        Err(_) => matches!(default, EnvDefault::OptOut),
    }
}

/// True unless `NVNMOSD_MALLOC_TRIM` is set to a disabling value (`0`, `false`, `off`, `no`).
pub fn trim_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| read_env_bool("NVNMOSD_MALLOC_TRIM", EnvDefault::OptOut))
}

fn info_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| read_env_bool("NVNMOSD_MALLOC_INFO", EnvDefault::OptIn))
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
