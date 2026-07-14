// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Parsing helpers for `NVNMOSD_*` environment variables.
//!
//! Boolean knobs use a small token vocabulary shared across the daemon:
//!
//! * [`EnvDefault::OptOut`] — unset → **on**; `0` / `false` / `off` / `no` → **off**;
//!   anything else → **on** (`NVNMOSD_MALLOC_TRIM`, `NVNMOSD_SESSION_GC`).
//! * [`EnvDefault::OptIn`] — unset → **off**; `1` / `true` / `yes` / `on` → **on**;
//!   anything else → **off** (`NVNMOSD_MALLOC_INFO`).

use std::time::Duration;

const ENABLE_TOKENS: &[&str] = &["1", "true", "TRUE", "yes", "YES", "on", "ON"];
const DISABLE_TOKENS: &[&str] = &["0", "false", "FALSE", "off", "OFF", "no", "NO"];

/// Default when an environment variable is unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvDefault {
    /// Unset → enabled; known disable tokens → disabled; anything else → enabled.
    OptOut,
    /// Unset → disabled; known enable tokens → enabled; anything else → disabled.
    OptIn,
}

/// Read `name` from the process environment and parse it with [`parse_env_bool`].
pub(crate) fn read_env_bool(name: &str, default: EnvDefault) -> bool {
    parse_env_bool(std::env::var(name).ok().as_deref().map(str::trim), default)
}

/// Parse a boolean environment value. `None` means the variable was unset.
pub(crate) fn parse_env_bool(value: Option<&str>, default: EnvDefault) -> bool {
    match value {
        None => matches!(default, EnvDefault::OptOut),
        Some(value) => match default {
            EnvDefault::OptOut => !DISABLE_TOKENS.contains(&value),
            EnvDefault::OptIn => ENABLE_TOKENS.contains(&value),
        },
    }
}

/// Read a positive timeout in **seconds** from `name`, falling back to `default_secs`
/// when unset or invalid (logs a warning for invalid non-empty values).
pub(crate) fn read_timeout_secs(name: &str, default_secs: u64) -> Duration {
    match std::env::var(name) {
        Err(_) => Duration::from_secs(default_secs),
        Ok(value) => match parse_timeout_secs(value.trim()) {
            Some(duration) => duration,
            None => {
                tracing::warn!(
                    env = name,
                    value = value.trim(),
                    default = default_secs,
                    "invalid timeout; using default"
                );
                Duration::from_secs(default_secs)
            }
        },
    }
}

/// Parse a timeout in **seconds**. Returns `None` for zero and non-numeric values.
pub(crate) fn parse_timeout_secs(value: &str) -> Option<Duration> {
    match value.parse::<u64>() {
        Ok(0) => None,
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(_) => None,
    }
}

/// Parse a TCP port number (`1..=65535`). Returns `None` for zero, out of
/// range, and non-numeric values.
pub(crate) fn parse_tcp_port(value: &str) -> Option<u16> {
    match value.parse::<u32>() {
        Ok(0) => None,
        Ok(port) => u16::try_from(port).ok(),
        Err(_) => None,
    }
}

/// Read a TCP port from `name`, falling back to `default` when unset or
/// invalid (logs a warning for invalid non-empty values).
pub(crate) fn read_tcp_port(name: &str, default: u16) -> u16 {
    match std::env::var(name) {
        Err(_) => default,
        Ok(value) => match parse_tcp_port(value.trim()) {
            Some(port) => port,
            None => {
                tracing::warn!(
                    env = name,
                    value = value.trim(),
                    default,
                    "invalid TCP port; using default"
                );
                default
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_out_unset_is_true() {
        assert!(parse_env_bool(None, EnvDefault::OptOut));
    }

    #[test]
    fn opt_out_disable_tokens() {
        for token in ["0", "false", "FALSE", "off", "OFF", "no", "NO"] {
            assert!(
                !parse_env_bool(Some(token), EnvDefault::OptOut),
                "token {token:?} should disable"
            );
        }
    }

    #[test]
    fn opt_out_other_values_enable() {
        for token in ["1", "true", "yes", "on", "maybe", ""] {
            assert!(
                parse_env_bool(Some(token), EnvDefault::OptOut),
                "token {token:?} should enable"
            );
        }
    }

    #[test]
    fn opt_in_unset_is_false() {
        assert!(!parse_env_bool(None, EnvDefault::OptIn));
    }

    #[test]
    fn opt_in_enable_tokens() {
        for token in ["1", "true", "TRUE", "yes", "YES", "on", "ON"] {
            assert!(
                parse_env_bool(Some(token), EnvDefault::OptIn),
                "token {token:?} should enable"
            );
        }
    }

    #[test]
    fn opt_in_other_values_disable() {
        for token in ["0", "false", "maybe", ""] {
            assert!(
                !parse_env_bool(Some(token), EnvDefault::OptIn),
                "token {token:?} should disable"
            );
        }
    }

    #[test]
    fn timeout_parses_seconds() {
        assert_eq!(parse_timeout_secs("30"), Some(Duration::from_secs(30)));
    }

    #[test]
    fn timeout_rejects_zero_and_garbage() {
        assert_eq!(parse_timeout_secs("0"), None);
        assert_eq!(parse_timeout_secs("nope"), None);
    }

    #[test]
    fn tcp_port_parses_valid_values() {
        assert_eq!(parse_tcp_port("18080"), Some(18_080));
        assert_eq!(parse_tcp_port("65535"), Some(65_535));
    }

    #[test]
    fn tcp_port_rejects_zero_and_garbage() {
        assert_eq!(parse_tcp_port("0"), None);
        assert_eq!(parse_tcp_port("65536"), None);
        assert_eq!(parse_tcp_port("nope"), None);
    }
}
