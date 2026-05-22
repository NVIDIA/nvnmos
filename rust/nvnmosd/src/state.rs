// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Daemon-wide state: Nodes, sessions, resources, and the operations
//! that mutate them.
//!
//! Shape (matches the design doc, "Naming convention"):
//!
//! * **Nodes** are keyed by `node_seed`. The daemon holds at most one
//!   [`nvnmos::NodeServer`] per seed; multiple sessions may attach to the
//!   same Node by referencing the same seed.
//! * **Sessions** are keyed by daemon-allocated `session_handle` strings.
//!   Each session remembers which `node_seed` it attached to (so
//!   [`State::close_session`] can find the right [`NodeEntry`] to detach
//!   from) and which `resource_handle`s it has registered (so the same
//!   call can drop them via libnvnmos before the Node itself goes away).
//! * **Resources** (senders and receivers) are keyed by daemon-allocated
//!   `resource_handle` strings. Each entry remembers the owning session,
//!   the Node it lives on, the client-supplied `internal_id` (the
//!   `x-nvnmos-id` inside the transport file), and the kind
//!   (sender/receiver). A secondary `(node_seed, internal_id) →
//!   resource_handle` index supports the daemon-level pre-add duplicate
//!   check and the activation router's lookup back from libnvnmos's
//!   `internal_id` to the owning session.
//! * **Activation subscriptions** are keyed by `session_handle`. Each
//!   session may hold at most one subscriber stream at a time
//!   ([`State::subscribe_activations`]); libnvnmos activation callbacks
//!   for resources owned by that session are bridged into the stream by
//!   [`State::dispatch_activation`] and awaited via
//!   [`State::complete_activation`].
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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use nvnmos::{NodeConfig, NodeServer, ReceiverConfig, SenderConfig, Transport};
use nvnmos_rpc::v1::{ActivationEvent, NodeConfig as ProtoNodeConfig, Transport as ProtoTransport};
use tokio::sync::mpsc as tokio_mpsc;
use tonic::Status;

use crate::log_bridge;

/// Upper bound on how long the activation router will wait for a client's
/// `AckActivation` before NACKing the IS-05 controller. The libnvnmos
/// callback blocks the IS-05 PATCH for that long, so this is also the
/// effective IS-05 latency ceiling for a healthy client. Tunable later
/// if real workloads need a different bound.
pub const ACTIVATION_ACK_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Sender or receiver — selects which libnvnmos API the daemon dispatches
/// to for the add / lookup / remove operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// IS-04 / IS-05 sender (`/senders/<id>`).
    Sender,
    /// IS-04 / IS-05 receiver (`/receivers/<id>`).
    Receiver,
}

impl ResourceKind {
    /// Short label for log lines and gRPC error messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::Sender => "sender",
            Self::Receiver => "receiver",
        }
    }

    /// Dispatch the wrapper's `add_*` call for this resource kind.
    fn add_to_server(
        self,
        server: &NodeServer,
        transport: Transport,
        transport_file: &str,
    ) -> nvnmos::Result<()> {
        match self {
            Self::Sender => server.add_sender(&SenderConfig {
                transport,
                transport_file: transport_file.to_string(),
            }),
            Self::Receiver => server.add_receiver(&ReceiverConfig {
                transport,
                transport_file: transport_file.to_string(),
            }),
        }
    }

    /// Dispatch the wrapper's `{sender,receiver}_id` lookup. Returns
    /// `Ok(None)` when libnvnmos does not have a resource of this kind
    /// with the given `internal_id`. Used as the post-add validation
    /// primitive by [`State::add_resource`].
    fn lookup_id(
        self,
        server: &NodeServer,
        internal_id: &str,
    ) -> nvnmos::Result<Option<String>> {
        match self {
            Self::Sender => server.sender_id(internal_id),
            Self::Receiver => server.receiver_id(internal_id),
        }
    }

    /// Dispatch the wrapper's `remove_*` call. Used both by
    /// [`State::remove_resource`] and by [`State::close_session`] when
    /// dropping a session's resources before tearing down the Node.
    fn remove_from_server(
        self,
        server: &NodeServer,
        internal_id: &str,
    ) -> nvnmos::Result<()> {
        match self {
            Self::Sender => server.remove_sender(internal_id),
            Self::Receiver => server.remove_receiver(internal_id),
        }
    }
}

/// A live resource (sender or receiver) owned by a session.
struct ResourceEntry {
    /// The `x-nvnmos-id` carried by the resource's transport file. The
    /// daemon validated at AddSender/AddReceiver time that this matched
    /// what libnvnmos extracted; see the proto's "Resource lifecycle"
    /// section for the validation contract.
    internal_id: String,
    /// Seed of the Node the resource lives on. Stored so [`State::close_session`]
    /// and (in the next slice) the activation router can find the right
    /// [`NodeEntry`] back from a resource.
    node_seed: String,
    /// Session that created this resource. Closing that session drops the
    /// resource; only that session is allowed to remove it. Read by the
    /// activation router ([`State::dispatch_activation`]) to find the
    /// right subscriber stream for an incoming libnvnmos activation.
    session_handle: String,
    /// Sender vs receiver — dispatches the libnvnmos API call.
    kind: ResourceKind,
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
    /// `resource_handle`s currently owned by this session. Used by
    /// [`State::close_session`] for O(1)-per-resource cleanup without
    /// scanning the global [`State::resources`] map.
    resources: HashSet<String>,
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

/// Outcome of [`State::add_sender`] / [`State::add_receiver`].
#[derive(Debug)]
pub struct AddResourceOutcome {
    pub resource_handle: String,
    /// NMOS UUID returned by libnvnmos for the resource. Equal to
    /// `nvnmos::make_{sender,receiver}_id(node_seed, internal_id)` — we
    /// pull it out of libnvnmos directly so we don't recompute.
    pub resource_id: String,
    pub kind: ResourceKind,
    pub node_seed: String,
}

/// Outcome of [`State::remove_resource`].
#[derive(Debug)]
pub struct RemoveResourceOutcome {
    pub node_seed: String,
    pub internal_id: String,
    pub kind: ResourceKind,
}

/// Outcome of [`State::sync_resource_state`].
#[derive(Debug)]
pub struct SyncResourceStateOutcome {
    pub node_seed: String,
    pub internal_id: String,
    pub kind: ResourceKind,
    /// `true` when the call was an (re)activation (`transport_file =
    /// Some`), `false` when it was a deactivation. Surfaces in the
    /// daemon's log line so operators can tell the two paths apart.
    pub activated: bool,
}

/// Client-side outcome of an activation, passed by the `AckActivation`
/// RPC handler to the activation router via the pending-activation
/// channel. `success = true` propagates as IS-05 success; `false` plus
/// `failure_reason` propagates as IS-05 failure (the reason is logged
/// today; libnvnmos's callback contract has no place to surface it
/// directly, so this is best-effort context for operators).
#[derive(Debug)]
pub struct AckOutcome {
    pub success: bool,
    pub failure_reason: String,
}

/// One session's `SubscribeActivations` stream slot. The sender end of
/// the tokio mpsc channel is shared with the streaming RPC handler;
/// `try_send` from the activation router pushes events out without
/// blocking the libnvnmos worker thread.
struct ActivationSubscriber {
    tx: tokio_mpsc::Sender<Result<ActivationEvent, Status>>,
}

/// One in-flight activation waiting on an `AckActivation`. Inserted by
/// [`State::dispatch_activation`], drained by [`State::complete_activation`]
/// (or by [`State::cleanup_pending_activation`] on the trampoline's
/// timeout / disconnect path).
struct PendingActivation {
    /// Session that owns the resource. The ack must come from this
    /// session — otherwise we'd let a peer ack another session's
    /// activations.
    session_handle: String,
    /// Sync channel back to the libnvnmos worker thread that is
    /// blocked waiting on the activation outcome. Dropping the sender
    /// (e.g. on `close_session`) wakes the worker with a
    /// `Disconnected` error and NACKs the activation.
    ack_tx: std_mpsc::SyncSender<AckOutcome>,
}

/// Result of [`State::dispatch_activation`], returned to the activation
/// router on the libnvnmos worker thread. Successful routing yields a
/// receiver the router blocks on; every other variant maps to an
/// immediate NACK with a logged reason.
pub enum ActivationDispatch {
    /// The event was placed in the subscriber's stream and a pending
    /// entry was recorded; the router should block on `ack_rx` and
    /// then call [`State::cleanup_pending_activation`].
    Routed {
        activation_handle: String,
        ack_rx: std_mpsc::Receiver<AckOutcome>,
    },
    /// No resource is registered for `(node_seed, internal_id)`. Either
    /// the activation is for a stray (a resource that survived the
    /// `AddSender`/`AddReceiver` mismatch path) or for one that was
    /// removed between the IS-05 PATCH arriving and the callback firing.
    NoResource,
    /// The owning session has no `SubscribeActivations` stream attached.
    /// (Either the client never subscribed or its earlier stream was
    /// torn down; we reaped a closed subscription in the same call.)
    NoSubscriber,
    /// The subscriber's bounded channel was full. The router NACKs
    /// without enqueueing; libnvnmos will typically retry on the next
    /// IS-05 PATCH.
    SubscriberBusy,
}

/// All daemon state. Wrapped in `Arc<Mutex<…>>` by the gRPC service.
pub struct State {
    nodes: HashMap<String, NodeEntry>,
    sessions: HashMap<String, SessionEntry>,
    /// Live resources keyed by daemon-allocated `resource_handle`.
    resources: HashMap<String, ResourceEntry>,
    /// Secondary index: `(node_seed, internal_id) → resource_handle`.
    /// Used for the pre-add duplicate check and for routing activation
    /// callbacks from libnvnmos back to the owning session.
    by_internal_id: HashMap<(String, String), String>,
    /// At most one `SubscribeActivations` subscriber per session.
    subscriptions: HashMap<String, ActivationSubscriber>,
    /// Activations currently waiting on `AckActivation`, keyed by
    /// daemon-allocated `activation_handle`.
    pending_activations: HashMap<String, PendingActivation>,
    next_session_id: AtomicU64,
    next_resource_id: AtomicU64,
    next_activation_id: AtomicU64,
}

impl State {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            sessions: HashMap::new(),
            resources: HashMap::new(),
            by_internal_id: HashMap::new(),
            subscriptions: HashMap::new(),
            pending_activations: HashMap::new(),
            next_session_id: AtomicU64::new(0),
            next_resource_id: AtomicU64::new(0),
            next_activation_id: AtomicU64::new(0),
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
                resources: HashSet::new(),
            },
        );
        Ok(OpenOutcome {
            session_handle,
            node_id,
            lifetime,
            created_node,
        })
    }

    /// Close a session, detaching from the backing Node. Drops every
    /// resource the session contributed (through libnvnmos) before
    /// detaching, so a subsequent destroy of the Node never has to
    /// reckon with stale resources. For [`Lifetime::SessionRefcounted`]
    /// Nodes, also destroys the Node when the last session detaches.
    /// [`Lifetime::Persistent`] Nodes are never destroyed by this call.
    pub fn close_session(&mut self, session_handle: &str) -> Result<CloseOutcome, Status> {
        let session = self.sessions.remove(session_handle).ok_or_else(|| {
            Status::not_found(format!(
                "session_handle {session_handle:?} is not known to this daemon"
            ))
        })?;

        // Drop the activation subscription, if any. The tokio mpsc tx is
        // released here; the streaming RPC handler observes `rx` going
        // empty and closed and returns.
        self.subscriptions.remove(session_handle);

        // Abort any in-flight activations belonging to this session.
        // Dropping the sync-channel sender wakes the libnvnmos worker
        // blocked in `recv_timeout` with a `Disconnected` error, which
        // surfaces to IS-05 as activation failure. Collect the keys
        // first to avoid mutating while iterating.
        let aborted: Vec<String> = self
            .pending_activations
            .iter()
            .filter(|(_, p)| p.session_handle == session_handle)
            .map(|(h, _)| h.clone())
            .collect();
        for handle in aborted {
            self.pending_activations.remove(&handle);
            tracing::warn!(
                session_handle,
                activation_handle = %handle,
                "close_session: aborting pending activation",
            );
        }

        let seed = session.node_seed;
        let node = self.nodes.get(&seed).ok_or_else(|| {
            Status::internal(format!(
                "session {session_handle:?} referenced seed {seed:?} but no Node entry exists"
            ))
        })?;

        // Drop the session's resources via libnvnmos before we touch the
        // Node's lifetime. Errors are logged but not fatal — the session
        // is going away regardless, and leaking a libnvnmos resource is
        // less bad than refusing to close. Resources are dropped from
        // the registry whether or not libnvnmos's removal succeeded.
        for resource_handle in session.resources {
            let Some(resource) = self.resources.remove(&resource_handle) else {
                tracing::warn!(
                    %resource_handle,
                    session_handle,
                    "close_session: session referenced unknown resource_handle"
                );
                continue;
            };
            self.by_internal_id
                .remove(&(resource.node_seed.clone(), resource.internal_id.clone()));
            if let Err(e) =
                resource.kind.remove_from_server(&node.server, &resource.internal_id)
            {
                tracing::warn!(
                    %resource_handle,
                    session_handle,
                    kind = resource.kind.label(),
                    internal_id = %resource.internal_id,
                    error = %e,
                    "close_session: libnvnmos remove_sender/remove_receiver failed; \
                     continuing"
                );
            }
        }

        let entry = self
            .nodes
            .get_mut(&seed)
            .expect("checked above and we never removed it in this scope");
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

    /// Register a sender. Thin wrapper around [`State::add_resource`].
    pub fn add_sender(
        &mut self,
        session_handle: &str,
        transport: Transport,
        transport_file: &str,
        claimed_internal_id: &str,
    ) -> Result<AddResourceOutcome, Status> {
        self.add_resource(
            ResourceKind::Sender,
            session_handle,
            transport,
            transport_file,
            claimed_internal_id,
        )
    }

    /// Register a receiver. Thin wrapper around [`State::add_resource`].
    pub fn add_receiver(
        &mut self,
        session_handle: &str,
        transport: Transport,
        transport_file: &str,
        claimed_internal_id: &str,
    ) -> Result<AddResourceOutcome, Status> {
        self.add_resource(
            ResourceKind::Receiver,
            session_handle,
            transport,
            transport_file,
            claimed_internal_id,
        )
    }

    /// Implementation shared by [`State::add_sender`] /
    /// [`State::add_receiver`]. The two only differ in which libnvnmos
    /// add/lookup APIs `kind` dispatches to.
    ///
    /// The validation flow:
    ///
    /// 1. Daemon-registry pre-check — refuses a duplicate `internal_id`
    ///    on the same Node before any FFI happens.
    /// 2. `add_{sender,receiver}` into libnvnmos. libnvnmos parses the
    ///    transport file and registers the resource under its embedded
    ///    `x-nvnmos-id`.
    /// 3. `{sender,receiver}_id(claimed_internal_id)` — uses libnvnmos
    ///    itself as the oracle: success proves the transport file's id
    ///    equalled the claim. Failure means a mismatch.
    /// 4. On mismatch, error `INVALID_ARGUMENT` and log. The libnvnmos
    ///    resource exists as a stray; the activation router will reap
    ///    it on first activation. See the proto's "Resource lifecycle"
    ///    section.
    fn add_resource(
        &mut self,
        kind: ResourceKind,
        session_handle: &str,
        transport: Transport,
        transport_file: &str,
        claimed_internal_id: &str,
    ) -> Result<AddResourceOutcome, Status> {
        if claimed_internal_id.is_empty() {
            return Err(Status::invalid_argument("internal_id must be non-empty"));
        }

        let session = self.sessions.get(session_handle).ok_or_else(|| {
            Status::not_found(format!(
                "session_handle {session_handle:?} is not known to this daemon"
            ))
        })?;
        let node_seed = session.node_seed.clone();
        let key = (node_seed.clone(), claimed_internal_id.to_string());

        if let Some(existing) = self.by_internal_id.get(&key) {
            return Err(Status::already_exists(format!(
                "a {} with internal_id {claimed_internal_id:?} is already \
                 registered on node_seed {node_seed:?} as resource_handle \
                 {existing:?}",
                kind.label(),
            )));
        }

        let node = self.nodes.get(&node_seed).ok_or_else(|| {
            Status::internal(format!(
                "session {session_handle:?} referenced seed {node_seed:?} \
                 but no Node entry exists"
            ))
        })?;

        kind.add_to_server(&node.server, transport, transport_file)
            .map_err(|e| {
                Status::invalid_argument(format!(
                    "libnvnmos add_{} failed (transport_file parse error or \
                     duplicate): {e}",
                    kind.label(),
                ))
            })?;

        let resource_id = match kind.lookup_id(&node.server, claimed_internal_id) {
            Ok(Some(id)) => id,
            Ok(None) => {
                tracing::error!(
                    kind = kind.label(),
                    claimed_internal_id,
                    %node_seed,
                    "AddSender/AddReceiver: libnvnmos accepted the transport \
                     file but its x-nvnmos-id does not match the claimed \
                     internal_id; left as stray, will be reaped at first \
                     activation"
                );
                return Err(Status::invalid_argument(format!(
                    "{}'s transport_file embeds a different x-nvnmos-id than \
                     internal_id {claimed_internal_id:?}",
                    kind.label(),
                )));
            }
            Err(e) => {
                return Err(Status::internal(format!(
                    "querying {} id from libnvnmos failed after add: {e}",
                    kind.label(),
                )));
            }
        };

        let resource_handle = self.allocate_resource_handle();
        self.resources.insert(
            resource_handle.clone(),
            ResourceEntry {
                internal_id: claimed_internal_id.to_string(),
                node_seed: node_seed.clone(),
                session_handle: session_handle.to_string(),
                kind,
            },
        );
        self.by_internal_id.insert(key, resource_handle.clone());
        self.sessions
            .get_mut(session_handle)
            .expect("session existed at the start of this method")
            .resources
            .insert(resource_handle.clone());

        Ok(AddResourceOutcome {
            resource_handle,
            resource_id,
            kind,
            node_seed,
        })
    }

    /// Remove a resource. Only the owning session is allowed to remove
    /// it; cross-session removal returns `NOT_FOUND` to avoid leaking
    /// the existence of other sessions' handles.
    pub fn remove_resource(
        &mut self,
        session_handle: &str,
        resource_handle: &str,
    ) -> Result<RemoveResourceOutcome, Status> {
        let session = self.sessions.get(session_handle).ok_or_else(|| {
            Status::not_found(format!(
                "session_handle {session_handle:?} is not known to this daemon"
            ))
        })?;
        if !session.resources.contains(resource_handle) {
            return Err(Status::not_found(format!(
                "session {session_handle:?} does not own resource_handle \
                 {resource_handle:?}"
            )));
        }

        let resource = self.resources.remove(resource_handle).ok_or_else(|| {
            Status::internal(format!(
                "session {session_handle:?} owns resource_handle \
                 {resource_handle:?} but no resource entry exists"
            ))
        })?;
        self.by_internal_id
            .remove(&(resource.node_seed.clone(), resource.internal_id.clone()));
        self.sessions
            .get_mut(session_handle)
            .expect("checked above")
            .resources
            .remove(resource_handle);

        // The daemon registry is consistent at this point. libnvnmos
        // removal is best-effort — a failure here would leak a resource
        // in libnvnmos's IS-04 model, but our state stays clean.
        if let Some(node) = self.nodes.get(&resource.node_seed) {
            if let Err(e) =
                resource.kind.remove_from_server(&node.server, &resource.internal_id)
            {
                tracing::warn!(
                    resource_handle,
                    kind = resource.kind.label(),
                    internal_id = %resource.internal_id,
                    error = %e,
                    "remove_resource: libnvnmos remove_sender/remove_receiver failed; \
                     daemon registry already cleared"
                );
            }
        }

        Ok(RemoveResourceOutcome {
            node_seed: resource.node_seed,
            internal_id: resource.internal_id,
            kind: resource.kind,
        })
    }

    /// Push an out-of-band data-plane state change through libnvnmos so
    /// the Node's IS-04 / IS-05 model reflects it. `transport_file =
    /// Some(_)` (re)activates the resource with that transport file;
    /// `transport_file = None` deactivates it. Maps onto
    /// `nmos_connection_activate` via [`NodeServer::activate_connection`],
    /// which (per the C contract) does **not** invoke the activation
    /// callback — this RPC is the *out-of-band* path.
    ///
    /// Only the owning session may sync a resource; cross-session calls
    /// return `NOT_FOUND`, matching [`State::remove_resource`].
    pub fn sync_resource_state(
        &mut self,
        session_handle: &str,
        resource_handle: &str,
        transport_file: Option<&str>,
    ) -> Result<SyncResourceStateOutcome, Status> {
        let session = self.sessions.get(session_handle).ok_or_else(|| {
            Status::not_found(format!(
                "session_handle {session_handle:?} is not known to this daemon"
            ))
        })?;
        if !session.resources.contains(resource_handle) {
            return Err(Status::not_found(format!(
                "session {session_handle:?} does not own resource_handle \
                 {resource_handle:?}"
            )));
        }

        let resource = self.resources.get(resource_handle).ok_or_else(|| {
            Status::internal(format!(
                "session {session_handle:?} owns resource_handle \
                 {resource_handle:?} but no resource entry exists"
            ))
        })?;
        let node = self.nodes.get(&resource.node_seed).ok_or_else(|| {
            Status::internal(format!(
                "resource {resource_handle:?} references seed {:?} but no \
                 Node entry exists",
                resource.node_seed,
            ))
        })?;

        node.server
            .activate_connection(&resource.internal_id, transport_file)
            .map_err(|e| {
                let verb = if transport_file.is_some() {
                    "activate"
                } else {
                    "deactivate"
                };
                Status::invalid_argument(format!(
                    "libnvnmos {verb} for {} {:?} failed (transport_file \
                     parse error or libnvnmos state mismatch): {e}",
                    resource.kind.label(),
                    resource.internal_id,
                ))
            })?;

        Ok(SyncResourceStateOutcome {
            node_seed: resource.node_seed.clone(),
            internal_id: resource.internal_id.clone(),
            kind: resource.kind,
            activated: transport_file.is_some(),
        })
    }

    /// Register a `SubscribeActivations` stream for `session_handle`.
    ///
    /// At most one subscription per session. If an existing slot is
    /// present but its receiver has been dropped (e.g. the client
    /// cancelled the previous stream), it is silently replaced; an
    /// active slot returns `ALREADY_EXISTS`.
    pub fn subscribe_activations(
        &mut self,
        session_handle: &str,
        tx: tokio_mpsc::Sender<Result<ActivationEvent, Status>>,
    ) -> Result<(), Status> {
        if !self.sessions.contains_key(session_handle) {
            return Err(Status::not_found(format!(
                "session_handle {session_handle:?} is not known to this daemon"
            )));
        }
        if let Some(existing) = self.subscriptions.get(session_handle) {
            if !existing.tx.is_closed() {
                return Err(Status::already_exists(format!(
                    "session {session_handle:?} already has an active \
                     SubscribeActivations stream"
                )));
            }
        }
        self.subscriptions
            .insert(session_handle.to_string(), ActivationSubscriber { tx });
        Ok(())
    }

    /// Activation router — called synchronously from a libnvnmos worker
    /// thread via the activation trampoline installed at NodeServer
    /// creation. Looks up the resource by `(node_seed, internal_id)`,
    /// finds its owning session's subscription, places the event on the
    /// subscriber's stream, and records a pending entry the caller
    /// blocks on for the ack.
    ///
    /// Holds `&mut self` for the duration, so the AckActivation
    /// handler can't race ahead: the pending entry is always visible
    /// once the event has been enqueued.
    pub fn dispatch_activation(
        &mut self,
        node_seed: &str,
        internal_id: &str,
        transport_file: Option<&str>,
    ) -> ActivationDispatch {
        let key = (node_seed.to_string(), internal_id.to_string());
        let resource_handle = match self.by_internal_id.get(&key) {
            Some(h) => h.clone(),
            None => return ActivationDispatch::NoResource,
        };
        let resource = match self.resources.get(&resource_handle) {
            Some(r) => r,
            None => {
                // by_internal_id is supposed to be in lockstep with
                // resources; surface the inconsistency in the log path
                // and NACK rather than panic.
                tracing::error!(
                    %node_seed,
                    internal_id,
                    %resource_handle,
                    "dispatch_activation: by_internal_id pointed at a \
                     resource_handle with no entry; treating as stray",
                );
                return ActivationDispatch::NoResource;
            }
        };
        let session_handle = resource.session_handle.clone();

        // Reap a closed subscription slot before consulting it, so a
        // dropped stream from an earlier subscribe doesn't permanently
        // mask a new one (and so a follow-up `subscribe_activations`
        // sees no stale entry).
        if self
            .subscriptions
            .get(&session_handle)
            .is_some_and(|s| s.tx.is_closed())
        {
            self.subscriptions.remove(&session_handle);
        }

        let tx = match self.subscriptions.get(&session_handle) {
            Some(s) => s.tx.clone(),
            None => return ActivationDispatch::NoSubscriber,
        };

        let activation_handle = self.allocate_activation_handle();
        let (ack_tx, ack_rx) = std_mpsc::sync_channel::<AckOutcome>(1);

        let event = ActivationEvent {
            resource_handle: resource_handle.clone(),
            activation_handle: activation_handle.clone(),
            transport_file: transport_file.map(str::to_string),
        };

        // Non-blocking — we must not stall the libnvnmos worker thread
        // on a slow subscriber. A full channel is treated as "subscriber
        // can't keep up"; a closed channel as "subscriber gone".
        match tx.try_send(Ok(event)) {
            Ok(()) => {}
            Err(tokio_mpsc::error::TrySendError::Full(_)) => {
                return ActivationDispatch::SubscriberBusy;
            }
            Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
                self.subscriptions.remove(&session_handle);
                return ActivationDispatch::NoSubscriber;
            }
        }

        self.pending_activations.insert(
            activation_handle.clone(),
            PendingActivation {
                session_handle,
                ack_tx,
            },
        );
        ActivationDispatch::Routed {
            activation_handle,
            ack_rx,
        }
    }

    /// Apply an `AckActivation`: validate that the session owns the
    /// pending activation, then forward the outcome to the libnvnmos
    /// worker thread blocked on the activation router.
    ///
    /// `NOT_FOUND` is used for both "no such activation" and "wrong
    /// session" so we don't leak the existence of other sessions'
    /// pending handles.
    pub fn complete_activation(
        &mut self,
        session_handle: &str,
        activation_handle: &str,
        outcome: AckOutcome,
    ) -> Result<(), Status> {
        if !self.sessions.contains_key(session_handle) {
            return Err(Status::not_found(format!(
                "session_handle {session_handle:?} is not known to this daemon"
            )));
        }
        let owns = self
            .pending_activations
            .get(activation_handle)
            .is_some_and(|p| p.session_handle == session_handle);
        if !owns {
            return Err(Status::not_found(format!(
                "activation_handle {activation_handle:?} is not pending on \
                 session {session_handle:?}"
            )));
        }
        let pending = self
            .pending_activations
            .remove(activation_handle)
            .expect("checked above");

        // The sync_channel has capacity 1 and we just drained it from
        // the map, so this send can only fail if the libnvnmos worker
        // already gave up (timeout) and dropped the receiver. Surface
        // that case as a warning — the IS-05 controller has already
        // seen a NACK and there is nothing we can do.
        if pending.ack_tx.send(outcome).is_err() {
            tracing::warn!(
                activation_handle,
                session_handle,
                "AckActivation: activation router already gave up; \
                 ack discarded",
            );
        }
        Ok(())
    }

    /// Idempotent removal of a pending activation entry, used by the
    /// activation router after its `recv_timeout` returns (whether ok,
    /// timed out, or disconnected). The ack handler may have removed
    /// the entry already — that's fine.
    pub fn cleanup_pending_activation(&mut self, activation_handle: &str) {
        let _ = self.pending_activations.remove(activation_handle);
    }

    fn allocate_session_handle(&self) -> String {
        let n = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        format!("sess-{n}")
    }

    fn allocate_resource_handle(&self) -> String {
        let n = self.next_resource_id.fetch_add(1, Ordering::Relaxed);
        format!("res-{n}")
    }

    fn allocate_activation_handle(&self) -> String {
        let n = self.next_activation_id.fetch_add(1, Ordering::Relaxed);
        format!("act-{n}")
    }
}

fn require_non_empty_seed(seed: &str) -> Result<(), Status> {
    if seed.is_empty() {
        Err(Status::invalid_argument("node_seed must be non-empty"))
    } else {
        Ok(())
    }
}

/// Translate a proto [`ProtoTransport`] into the wrapper's [`Transport`].
/// `TRANSPORT_UNSPECIFIED` (proto3's zero value) is treated as a
/// caller error.
pub fn translate_transport(proto: ProtoTransport) -> Result<Transport, Status> {
    match proto {
        ProtoTransport::Unspecified => Err(Status::invalid_argument(
            "transport must be specified (TRANSPORT_RTP or TRANSPORT_MXL)",
        )),
        ProtoTransport::Rtp => Ok(Transport::Rtp),
        ProtoTransport::Mxl => Ok(Transport::Mxl),
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
