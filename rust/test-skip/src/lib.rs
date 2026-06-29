// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Runtime test skipping for libtest.
//!
//! libtest has no stable "skip at runtime" status, and it captures the output
//! of passing tests, so an `eprintln!` skip notice is swallowed unless the
//! suite runs with `--show-output` (or `--nocapture`). [`skip!`] prints a line
//! in libtest's own grammar — `test <name> ... skipped, <reason>` — mirroring
//! the `... ignored, <reason>` line a static `#[ignore = "..."]` produces, and
//! returns from the test.

/// Print a libtest-style "skipped" line for the current test and return.
///
/// The test name is taken from the current thread (libtest names the test
/// thread after the test), so the line carries the real test path without
/// repeating it by hand. Visible under `cargo test -- --show-output`.
///
/// Only usable from a test that returns `()`, since it expands to `return;`.
///
/// ```
/// fn maybe_run() {
///     if std::env::var("PREREQ").is_err() {
///         test_skip::skip!("PREREQ not set");
///     }
///     unreachable!("prerequisite present");
/// }
/// maybe_run();
/// ```
#[macro_export]
macro_rules! skip {
    ($reason:expr $(,)?) => {{
        let thread = ::std::thread::current();
        let name = thread.name().unwrap_or("<unknown test>");
        ::std::eprintln!("test {name} ... skipped, {}", $reason);
        return;
    }};
}
