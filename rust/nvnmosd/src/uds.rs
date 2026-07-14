// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Unix-domain socket path preparation for `nvnmosd` startup.
//!
//! A second daemon must not unlink a path that still has a live listener:
//! clients would keep talking to the old process while HTTP ports stay
//! pinned. We probe with [`std::os::unix::net::UnixStream::connect`] and
//! only remove a stale pathname when nothing accepts connections.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::Context;

/// Ensure `path` is safe to bind: fail if another listener is already
/// accepting connections; remove only a stale socket file.
///
/// Always probes with [`UnixStream::connect`], even when the pathname is
/// missing. Relying on [`Path::exists`] first allowed a second `nvnmosd` to
/// `bind()` in the window before the demo daemon created the socket file,
/// or when a live listener held an unlinked inode after a careless `rm`.
pub fn prepare_listen_path(path: &Path) -> anyhow::Result<()> {
    match UnixStream::connect(path) {
        Ok(_stream) => {
            anyhow::bail!(
                "UDS socket {} is already in use by another listener \
                 (another nvnmosd is probably still running); stop it \
                 before starting a new instance",
                path.display()
            );
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // Nothing listening at this path yet (no file, or racing creator).
        }
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
            // Stale socket file with no acceptor.
            if path.exists() {
                std::fs::remove_file(path)
                    .with_context(|| format!("removing stale UDS socket at {}", path.display()))?;
            }
        }
        Err(e) => {
            anyhow::bail!("cannot probe UDS socket at {}: {e}", path.display());
        }
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent directory for {}", path.display()))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener as StdUnixListener;

    use super::*;

    #[test]
    fn rejects_live_listener() {
        let dir = std::env::temp_dir().join(format!("nvnmosd-uds-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("nvnmosd.sock");

        let listener = StdUnixListener::bind(&sock).expect("bind test socket");
        let _guard = listener;

        let err = prepare_listen_path(&sock).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("already in use"), "unexpected error: {msg}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn probes_even_when_pathname_is_missing() {
        let dir = std::env::temp_dir().join(format!("nvnmosd-uds-missing-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("nvnmosd.sock");

        prepare_listen_path(&sock).expect("missing path should be bindable");

        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn removes_stale_socket_file() {
        let dir = std::env::temp_dir().join(format!("nvnmosd-uds-stale-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("nvnmosd.sock");
        std::fs::write(&sock, b"").expect("create stale socket file");

        prepare_listen_path(&sock).expect("stale socket should be removed");

        assert!(!sock.exists(), "stale socket file should have been removed");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_while_background_listener_holds_path() {
        let dir = std::env::temp_dir().join(format!("nvnmosd-uds-bg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("nvnmosd.sock");

        let _listener = StdUnixListener::bind(&sock).expect("bind");

        let err = prepare_listen_path(&sock).unwrap_err();
        assert!(
            format!("{err:#}").contains("already in use"),
            "expected in-use error"
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
