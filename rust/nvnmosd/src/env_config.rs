// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Parsing helpers for `NVNMOSD_*` environment variables.
//!
//! Boolean knobs use a small token vocabulary shared across the daemon:
//!
//! * [`EnvDefault::OptOut`] — unset → **on**; `0` / `false` / `off` / `no` → **off**;
//!   anything else → **on** (`NVNMOSD_MALLOC_TRIM`, `NVNMOSD_SESSION_GC`,
//!   `NVNMOSD_ANNOTATION_API`).
//! * [`EnvDefault::OptIn`] — unset → **off**; `1` / `true` / `yes` / `on` → **on**;
//!   anything else → **off** (`NVNMOSD_MALLOC_INFO`).
//!
//! Integer knobs use [`read_integer`]. Each caller passes its own minimum;
//! a value that is not an integer, or is below that minimum, falls back to
//! the default.

use std::fmt;
use std::str::FromStr;

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

/// Read an integer from `name` and parse it with [`parse_integer`]. Falls
/// back to `default` when unset or invalid, and logs a warning for an
/// invalid value.
pub(crate) fn read_integer<T>(name: &str, default: T, min: T) -> T
where
    T: FromStr + PartialOrd + Copy + fmt::Display,
{
    match std::env::var(name) {
        Err(_) => default,
        Ok(value) => match parse_integer(value.trim(), min) {
            Some(parsed) => parsed,
            None => {
                tracing::warn!(
                    env = name,
                    value = value.trim(),
                    %default,
                    %min,
                    "invalid integer; using default"
                );
                default
            }
        },
    }
}

/// Parse an integer of type `T` that is at least `min`. Returns `None` for
/// non-numeric values, fractions, values outside the range of `T`, and
/// values below `min`.
pub(crate) fn parse_integer<T: FromStr + PartialOrd>(value: &str, min: T) -> Option<T> {
    value.parse::<T>().ok().filter(|parsed| *parsed >= min)
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
    fn integer_parses_values_at_or_above_min() {
        assert_eq!(parse_integer::<u64>("30", 1), Some(30));
        assert_eq!(parse_integer::<u64>("1", 1), Some(1));
        assert_eq!(parse_integer::<u64>("0", 0), Some(0));
    }

    #[test]
    fn integer_rejects_values_below_min() {
        assert_eq!(parse_integer::<u64>("0", 1), None);
        assert_eq!(parse_integer::<u16>("0", 1), None);
    }

    #[test]
    fn integer_rejects_fractions_garbage_and_out_of_range() {
        assert_eq!(parse_integer::<u64>("0.001", 0), None);
        assert_eq!(parse_integer::<u64>("nope", 0), None);
        assert_eq!(parse_integer::<u64>("-1", 0), None);
        assert_eq!(parse_integer::<u16>("65535", 1), Some(65_535));
        assert_eq!(parse_integer::<u16>("65536", 1), None);
    }
}
