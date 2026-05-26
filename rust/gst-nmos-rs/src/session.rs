// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Property validation and session lifecycle shared between `nmossrc`
//! and `nmossink`.
//!
//! Each element snapshots its `Settings` into [`CommonSettings`] under
//! its own settings lock, then calls [`validate_and_open`] /
//! [`close`] with that snapshot. The element holds the resulting
//! [`Session`](crate::daemon::Session) under a separate lock to keep
//! the settings critical section short.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, bail};
use gstreamer as gst;

use crate::daemon::Session;
use crate::runtime::SHARED_RUNTIME;
use crate::types::Transport;

/// Open-session timeout. Aligned with the daemon's activation ack
/// timeout — same order of magnitude, no special meaning.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the snapshot came from `nmossink` or `nmossrc`. Surfaces in
/// error/log messages so validation failures point the user at the
/// right property name, and (in a later phase) selects which gRPC
/// AddSender/AddReceiver call the session opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Sender,
    Receiver,
}

impl Side {
    /// Property name the user sets to supply the NMOS name for this
    /// side of element — `sender-name` on `nmossink`, `receiver-name`
    /// on `nmossrc`. Used in validation error messages.
    fn name_property(self) -> &'static str {
        match self {
            Self::Sender => "sender-name",
            Self::Receiver => "receiver-name",
        }
    }
}

/// Snapshot of the properties needed to open a session, taken under
/// the per-element settings lock so the lock isn't held over the
/// blocking RPC.
#[derive(Debug, Clone)]
pub(crate) struct CommonSettings {
    pub(crate) daemon_uri: String,
    pub(crate) node_seed: String,
    pub(crate) transport: Transport,
    /// Whether this snapshot came from `nmossink` (Sender) or `nmossrc`
    /// (Receiver). Pinned by the element that built the snapshot.
    pub(crate) side: Side,
    /// NMOS resource name within the Node, unique per side on the
    /// Node. A Sender on `nmossink` and a Receiver on `nmossrc` are
    /// permitted to share the same name; the daemon scopes its
    /// `by_name` index by `(node_seed, side, name)` and the activation
    /// callback surfaces the side alongside the name.
    pub(crate) name: String,
    pub(crate) mxl_domain_id: String,
}

/// Validate the settings snapshot and open a session via the shared
/// tokio runtime. On success the session is stored under `session`.
pub(crate) fn validate_and_open(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    session: &Mutex<Option<Session>>,
) -> Result<(), anyhow::Error> {
    if settings.node_seed.is_empty() {
        bail!("{element}: `node-seed` is required");
    }
    if settings.name.is_empty() {
        bail!(
            "{element}: `{}` is required",
            settings.side.name_property()
        );
    }
    if settings.mxl_domain_id.is_empty() {
        bail!("{element}: `mxl-domain-id` is required when transport=mxl");
    }

    let new_session = SHARED_RUNTIME
        .block_on(async {
            tokio::time::timeout(
                OPEN_TIMEOUT,
                Session::open(&settings.daemon_uri, &settings.node_seed),
            )
            .await
        })
        .with_context(|| format!("{element}: OpenSession against {} timed out", settings.daemon_uri))?
        .with_context(|| format!("{element}: OpenSession against {}", settings.daemon_uri))?;

    gst::info!(
        cat,
        "session opened: handle={} node_id={} created_node={} (node_seed={}, name={})",
        new_session.session_handle,
        new_session.node_id,
        new_session.created_node,
        settings.node_seed,
        settings.name,
    );

    *session.lock().unwrap() = Some(new_session);
    Ok(())
}

/// Drop the session and tell the daemon to close it. Logged-only on
/// error so state-change cleanup always succeeds.
pub(crate) fn close(cat: &gst::DebugCategory, element: &str, session: &Mutex<Option<Session>>) {
    let to_close = session.lock().unwrap().take();
    if let Some(s) = to_close {
        let handle = s.session_handle.clone();
        let result = SHARED_RUNTIME.block_on(s.close());
        match result {
            Ok(()) => gst::info!(cat, "session closed: handle={handle}"),
            Err(e) => gst::warning!(cat, "{element}: CloseSession (handle={handle}): {e}"),
        }
    }
}
