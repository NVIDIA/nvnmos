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
//!   [`State::close_session`] can find the right [`NodeEntry`] to detach
//!   from.
//!
//! Every Node has a [`Lifetime`]:
//!
//! * [`Lifetime::SessionRefcounted`] — Nodes created implicitly by
//!   [`State::open_session`]. Destroyed when the last attached session
//!   closes.
//! * [`Lifetime::Persistent`] — Nodes created explicitly by
//!   [`State::add_node`]. Survive every [`State::close_session`]; only
//!   [`State::remove_node`] tears them down, and only when no sessions
//!   are currently attached.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use nvnmos::{NodeConfig, NodeServer};
use nvnmos_rpc::v1::NodeConfig as ProtoNodeConfig;
use tonic::Status;

use crate::log_bridge;

/// What governs a Node's destruction.
///
/// Exposed publicly because the daemon's log lines and the per-RPC
/// outcomes surface it to operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    /// Created by [`State::open_session`] — destroyed when the last
    /// attached session closes.
    SessionRefcounted,
    /// Created by [`State::add_node`] — survives every
    /// [`State::close_session`]; only [`State::remove_node`] tears it
    /// down (and only when no sessions are attached).
    Persistent,
}

impl Lifetime {
    /// Short string for log lines.
    pub fn label(self) -> &'static str {
        match self {
            Self::SessionRefcounted => "session-refcounted",
            Self::Persistent => "persistent",
        }
    }
}

/// A live Node owned by the daemon.
struct NodeEntry {
    /// The C node server itself. Dropped when the entry is removed,
    /// which calls `destroy_nmos_node_server` via the wrapper's `Drop`.
    /// `#[allow(dead_code)]` because the field is held purely for its
    /// `Drop` side effect — Rust can't see the FFI-level usage.
    #[allow(dead_code)]
    server: NodeServer,
    /// NMOS `/self` UUID cached at create time so RPC handlers that hold
    /// the state lock don't drop back into FFI for every read. Stable for
    /// the lifetime of the entry — libnvnmos derives it from the seed.
    node_id: String,
    /// What governs this Node's destruction.
    lifetime: Lifetime,
    /// Number of sessions currently attached. For
    /// [`Lifetime::SessionRefcounted`], destruction happens when this
    /// hits 0. For [`Lifetime::Persistent`], this is consulted by
    /// [`State::remove_node`] to refuse teardown while sessions are
    /// still around.
    attached_sessions: usize,
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
    /// Lifetime of the Node the session is attached to (existing or
    /// newly-created).
    pub lifetime: Lifetime,
    /// True if this call constructed a new [`NodeServer`] (necessarily
    /// `SessionRefcounted`); false if it merely attached to an existing
    /// one.
    pub created_node: bool,
}

/// Outcome of [`State::close_session`], for the caller's log line.
#[derive(Debug)]
pub struct CloseOutcome {
    pub node_seed: String,
    pub node_id: String,
    /// Lifetime of the Node the session was attached to.
    pub lifetime: Lifetime,
    /// Sessions still attached *after* the close.
    pub remaining_sessions: usize,
    /// True iff this call destroyed the Node — only possible for
    /// [`Lifetime::SessionRefcounted`] when the last session detached.
    pub node_destroyed: bool,
}

/// Outcome of [`State::add_node`].
#[derive(Debug)]
pub struct AddNodeOutcome {
    pub node_id: String,
}

/// Outcome of [`State::remove_node`].
#[derive(Debug)]
pub struct RemoveNodeOutcome {
    pub node_id: String,
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
    /// If a Node already exists for `seed` (either [`Lifetime::Persistent`]
    /// or [`Lifetime::SessionRefcounted`]), this attaches to it and
    /// increments its session count; `build_node_server` is *not* invoked
    /// and any `node_config` the caller supplied is ignored. If no Node
    /// exists, this constructs a new [`Lifetime::SessionRefcounted`] Node
    /// via `build_node_server`.
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
        require_non_empty_seed(seed)?;

        let (created_node, node_id, lifetime) = match self.nodes.get_mut(seed) {
            Some(entry) => {
                entry.attached_sessions += 1;
                (false, entry.node_id.clone(), entry.lifetime)
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
                        lifetime: Lifetime::SessionRefcounted,
                        attached_sessions: 1,
                    },
                );
                (true, node_id, Lifetime::SessionRefcounted)
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
            lifetime,
            created_node,
        })
    }

    /// Close a session, detaching from the backing Node. For
    /// [`Lifetime::SessionRefcounted`] Nodes, also destroys the Node when
    /// the last session detaches. [`Lifetime::Persistent`] Nodes are
    /// never destroyed by this call.
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
        let lifetime = entry.lifetime;
        entry.attached_sessions = entry.attached_sessions.saturating_sub(1);
        let remaining_sessions = entry.attached_sessions;
        let node_destroyed =
            lifetime == Lifetime::SessionRefcounted && remaining_sessions == 0;
        if node_destroyed {
            self.nodes.remove(&seed);
        }
        Ok(CloseOutcome {
            node_seed: seed,
            node_id,
            lifetime,
            remaining_sessions,
            node_destroyed,
        })
    }

    /// Create a persistent Node for `seed`.
    ///
    /// Errors:
    /// * `INVALID_ARGUMENT` if `seed` is empty.
    /// * `ALREADY_EXISTS` if any Node (persistent or session-refcounted)
    ///   currently exists for `seed`.
    /// * Whatever `build_node_server` returns, surfaced verbatim.
    pub fn add_node(
        &mut self,
        seed: &str,
        build_node_server: impl FnOnce() -> Result<NodeServer, Status>,
    ) -> Result<AddNodeOutcome, Status> {
        require_non_empty_seed(seed)?;
        if let Some(entry) = self.nodes.get(seed) {
            return Err(Status::already_exists(format!(
                "a {} Node already exists for seed {seed:?}",
                entry.lifetime.label(),
            )));
        }
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
                lifetime: Lifetime::Persistent,
                attached_sessions: 0,
            },
        );
        Ok(AddNodeOutcome { node_id })
    }

    /// Tear down a persistent Node.
    ///
    /// Errors:
    /// * `INVALID_ARGUMENT` if `seed` is empty.
    /// * `NOT_FOUND` if no Node exists for `seed`.
    /// * `FAILED_PRECONDITION` if the Node is [`Lifetime::SessionRefcounted`]
    ///   (the caller must close sessions instead) or if any sessions are
    ///   currently attached (the caller must close them first).
    pub fn remove_node(&mut self, seed: &str) -> Result<RemoveNodeOutcome, Status> {
        require_non_empty_seed(seed)?;
        let entry = self.nodes.get(seed).ok_or_else(|| {
            Status::not_found(format!("no Node exists for seed {seed:?}"))
        })?;
        match entry.lifetime {
            Lifetime::Persistent => {}
            Lifetime::SessionRefcounted => {
                return Err(Status::failed_precondition(format!(
                    "Node for seed {seed:?} is session-refcounted (created by OpenSession); \
                     close its sessions to tear it down"
                )));
            }
        }
        if entry.attached_sessions != 0 {
            return Err(Status::failed_precondition(format!(
                "Node for seed {seed:?} still has {} attached session(s); close them first",
                entry.attached_sessions,
            )));
        }
        // Safe to unwrap because we just successfully got() the entry and
        // we still hold the &mut self lock.
        let entry = self.nodes.remove(seed).expect("checked above");
        Ok(RemoveNodeOutcome {
            node_id: entry.node_id,
        })
    }

    fn allocate_session_handle(&self) -> String {
        let n = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        format!("sess-{n}")
    }
}

fn require_non_empty_seed(seed: &str) -> Result<(), Status> {
    if seed.is_empty() {
        Err(Status::invalid_argument("node_seed must be non-empty"))
    } else {
        Ok(())
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
