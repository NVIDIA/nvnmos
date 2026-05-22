// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Daemon-wide state: Nodes, sessions, and the operations that mutate them.
//!
//! Shape (matches the design doc, "Naming convention"):
//!
//! * **Nodes** are keyed by `node_seed`. The daemon holds at most one
//!   [`nvnmos::NodeServer`] per seed; multiple sessions may attach to the
//!   same Node by referencing the same seed.
//! * **Sessions** are keyed by daemon-allocated `session_handle` strings.
//!   Each session remembers which `node_seed` it attached to so
//!   [`State::close_session`] can find the right [`NodeEntry`] to
//!   decrement.
//!
//! Lifetime today is always session-refcounted: the last
//! [`State::close_session`] to bring `refcount` to 0 drops the
//! [`NodeServer`], which destroys the underlying C node server. Persistent
//! Nodes (managed by `AddNode`/`RemoveNode`) will land in the next commit
//! and will pin `refcount >= 1` independent of session attachments.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use nvnmos::{NodeConfig, NodeServer};
use nvnmos_rpc::v1::NodeConfig as ProtoNodeConfig;
use tonic::Status;

use crate::log_bridge;

/// A live Node owned by the daemon.
struct NodeEntry {
    /// The C node server itself. Dropped when [`Self::refcount`] hits 0,
    /// which calls `destroy_nmos_node_server` via the wrapper's `Drop`.
    /// `#[allow(dead_code)]` because the field is held purely for its
    /// `Drop` side effect — Rust can't see the FFI-level usage.
    #[allow(dead_code)]
    server: NodeServer,
    /// NMOS `/self` UUID cached at create time so RPC handlers that hold
    /// the state lock don't drop back into FFI for every read. Stable for
    /// the lifetime of the entry — libnvnmos derives it from the seed.
    node_id: String,
    /// Number of attached sessions.
    refcount: usize,
}

/// A live session.
struct SessionEntry {
    /// Key into [`State::nodes`]. The daemon looks the [`NodeEntry`] back
    /// up on every RPC that uses the session, rather than caching a
    /// pointer, so the entry can be moved / re-inserted without
    /// invalidation.
    node_seed: String,
}

/// Outcome of [`State::open_session`], for the caller's log line.
#[derive(Debug)]
pub struct OpenOutcome {
    pub session_handle: String,
    pub node_id: String,
    /// True if this call constructed a new [`NodeServer`]; false if it
    /// merely attached to an existing one (refcount went 1→2, 2→3, …).
    pub created_node: bool,
}

/// Outcome of [`State::close_session`], for the caller's log line.
#[derive(Debug)]
pub struct CloseOutcome {
    pub node_seed: String,
    pub node_id: String,
    /// Refcount *after* the close. 0 means the Node was destroyed.
    pub remaining_refcount: usize,
}

/// All daemon state. Wrapped in `Arc<Mutex<…>>` by the gRPC service.
pub struct State {
    nodes: HashMap<String, NodeEntry>,
    sessions: HashMap<String, SessionEntry>,
    next_session_id: AtomicU64,
}

impl State {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            sessions: HashMap::new(),
            next_session_id: AtomicU64::new(0),
        }
    }

    /// Open a session, attaching to or creating a Node for `seed`.
    ///
    /// `build_node_server` is invoked only when no Node currently exists
    /// for `seed`. The caller passes a translated [`NodeConfig`] plus
    /// whatever callbacks it wants installed (the daemon at minimum
    /// installs [`crate::log_bridge::forward`]).
    ///
    /// Errors:
    /// * `INVALID_ARGUMENT` if `seed` is empty (the empty seed would
    ///   collapse every "default" session onto the same Node and lose
    ///   determinism of the NMOS UUIDs).
    /// * Whatever `build_node_server` returns, surfaced verbatim.
    pub fn open_session(
        &mut self,
        seed: &str,
        build_node_server: impl FnOnce() -> Result<NodeServer, Status>,
    ) -> Result<OpenOutcome, Status> {
        if seed.is_empty() {
            return Err(Status::invalid_argument("node_seed must be non-empty"));
        }

        let (created_node, node_id) = match self.nodes.get_mut(seed) {
            Some(entry) => {
                entry.refcount += 1;
                (false, entry.node_id.clone())
            }
            None => {
                let server = build_node_server()?;
                let node_id = server.node_id().map_err(|e| {
                    Status::internal(format!(
                        "querying node_id from the new NodeServer failed: {e}"
                    ))
                })?;
                self.nodes.insert(
                    seed.to_string(),
                    NodeEntry {
                        server,
                        node_id: node_id.clone(),
                        refcount: 1,
                    },
                );
                (true, node_id)
            }
        };

        let session_handle = self.allocate_session_handle();
        self.sessions.insert(
            session_handle.clone(),
            SessionEntry {
                node_seed: seed.to_string(),
            },
        );
        Ok(OpenOutcome {
            session_handle,
            node_id,
            created_node,
        })
    }

    /// Close a session, decrementing the backing Node's refcount and
    /// destroying it if the refcount hits 0.
    pub fn close_session(&mut self, session_handle: &str) -> Result<CloseOutcome, Status> {
        let session = self.sessions.remove(session_handle).ok_or_else(|| {
            Status::not_found(format!(
                "session_handle {session_handle:?} is not known to this daemon"
            ))
        })?;

        let seed = session.node_seed;
        let entry = self.nodes.get_mut(&seed).ok_or_else(|| {
            Status::internal(format!(
                "session {session_handle:?} referenced seed {seed:?} but no Node entry exists"
            ))
        })?;
        let node_id = entry.node_id.clone();
        entry.refcount = entry.refcount.saturating_sub(1);
        let remaining_refcount = entry.refcount;
        if remaining_refcount == 0 {
            self.nodes.remove(&seed);
        }
        Ok(CloseOutcome {
            node_seed: seed,
            node_id,
            remaining_refcount,
        })
    }

    fn allocate_session_handle(&self) -> String {
        let n = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        format!("sess-{n}")
    }
}

/// Translate a proto [`ProtoNodeConfig`] into the wrapper's
/// [`NodeConfig`].
///
/// `seed` always comes from `OpenSessionRequest.node_seed` (the registry
/// key), so any `seed` inside `proto` is intentionally overridden: the
/// daemon's lookup key and libnvnmos's UUID derivation key must agree.
/// `proto.asset_tags` / `proto.network_services` are accepted but
/// currently ignored — the wrapper doesn't yet expose them; they'll be
/// plumbed through when [`nvnmos::NodeConfig`] grows the corresponding
/// fields.
pub fn translate_config(
    proto: Option<&ProtoNodeConfig>,
    seed: &str,
) -> Result<NodeConfig, Status> {
    let proto = proto.cloned().unwrap_or_default();
    let http_port = u16::try_from(proto.http_port).map_err(|_| {
        Status::invalid_argument(format!(
            "node_config.http_port {} is not a valid TCP port (max 65535)",
            proto.http_port,
        ))
    })?;
    Ok(NodeConfig {
        seed: seed.to_string(),
        host_name: proto.host_name,
        host_addresses: proto.host_addresses,
        http_port,
        label: proto.label,
        description: proto.description,
        log_level: log_bridge::LIBNVNMOS_LOG_LEVEL,
    })
}
