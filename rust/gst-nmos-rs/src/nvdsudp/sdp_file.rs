// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Temporary SDP file for `nvdsudpsink.sdp-file` (path-based API).
//!
//! `nvdsudpsink` stores only the path and re-reads the file from disk in
//! `start` (NULL→READY) and on later NULL→READY cycles — not at property
//! set time. The guard must therefore outlive the Rust `NvDsUdpSinkChain`
//! wrapper (callers keep only `chain.bin`). Attach it to the inner
//! `GstElement` via [`attach_to_element`] so the file is deleted when the
//! element is finalized after chain teardown.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use tempfile::Builder;
use glib::object::ObjectExt;
use gstreamer as gst;

/// GObject qdata key for [`SdpFileGuard`] on the inner `nvdsudpsink`.
const SDP_FILE_GUARD_KEY: &str = "nvnmos-nvdsudp-sdp-file";

/// Owns a temp SDP file written for `nvdsudpsink`. Deleted on drop.
#[derive(Debug)]
pub(crate) struct SdpFileGuard {
    path: PathBuf,
}

impl SdpFileGuard {
    /// Write `sdp_text` to a unique file under the system temp dir.
    ///
    /// Uses `tempfile` (`O_CREAT | O_EXCL`) so concurrent writers cannot
    /// clobber each other's paths.
    pub(crate) fn write(prefix: &str, sdp_text: &str) -> Result<Self, anyhow::Error> {
        let mut file = Builder::new()
            .prefix(&format!("{prefix}-"))
            .suffix(".sdp")
            .tempfile_in(std::env::temp_dir())
            .context("creating nvdsudpsink sdp temp file")?;
        file.write_all(sdp_text.as_bytes())
            .with_context(|| format!("writing nvdsudpsink sdp-file `{}`", file.path().display()))?;
        let (_, path) = file
            .keep()
            .context("keeping nvdsudpsink sdp temp file")?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Tie temp-file lifetime to `element` (the inner `nvdsudpsink`).
    ///
    /// GObject drops the guard (and unlinks the file) when the element is
    /// finalized — after `rebuild_chain` sets it NULL and removes it from
    /// the bin.
    pub(crate) fn attach_to_element(element: &gst::Element, guard: Self) {
        // SAFETY: this key is only written here; nothing else reads or
        // steals the qdata with a different type.
        unsafe {
            element.set_data(SDP_FILE_GUARD_KEY, guard);
        }
    }
}

impl Drop for SdpFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sdp_file_guard_removes_on_drop() {
        let path = {
            let guard = SdpFileGuard::write("nvnmos-test", "v=0\r\n").expect("write sdp");
            let path = guard.path().to_owned();
            assert!(path.exists());
            path
        };
        assert!(!path.exists());
    }

    #[test]
    fn sdp_file_guard_unique_paths() {
        let g1 = SdpFileGuard::write("nvnmos-test", "a").expect("write sdp");
        let g2 = SdpFileGuard::write("nvnmos-test", "b").expect("write sdp");
        assert_ne!(g1.path(), g2.path());
    }

    #[test]
    fn sdp_file_guard_roundtrip_contents() {
        let text = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n";
        let guard = SdpFileGuard::write("nvnmos-test", text).expect("write sdp");
        let read = fs::read_to_string(guard.path()).expect("read sdp");
        assert_eq!(read, text);
    }

    #[test]
    fn sdp_file_guard_survives_chain_struct_drop_until_element_finalized() {
        gst::init().expect("gst init");
        let element = gst::ElementFactory::make("fakesink")
            .build()
            .expect("fakesink");
        let path = {
            let guard = SdpFileGuard::write("nvnmos-test", "v=0\r\n").expect("write sdp");
            let path = guard.path().to_owned();
            SdpFileGuard::attach_to_element(&element, guard);
            path
        };
        assert!(path.exists(), "file must outlive the Rust guard move");
        drop(element);
        assert!(!path.exists(), "file must be removed when element is finalized");
    }
}
