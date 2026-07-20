// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Daemon-wide state: Nodes, sessions, resources, and the operations
//! that mutate them.
//!
//! **Shape:** (formalised in the design doc, "Daemon internal state"
//! section of `doc/designs/nvnmosd/README.md`)
//!
//! * **Nodes** are keyed by `node_seed`. The daemon holds at most one
//!   [`nvnmos::NodeServer`] per seed; multiple sessions may attach to the
//!   same Node by referencing the same seed.
//! * **Sessions** are keyed by daemon-allocated `session_handle` strings.
//!   Each session remembers which `node_seed` it attached to (so
//!   [`State::close_session`] can find the right [`NodeEntry`] to detach
//!   from) and which `resource_handle`s it has created (so the same
//!   call can drop them via libnvnmos before the Node itself goes away).
//! * **Resources** (senders and receivers) are keyed by daemon-allocated
//!   `resource_handle` strings. Each entry remembers the owning session,
//!   the Node it lives on, the client-supplied `name` (the
//!   `x-nvnmos-name` SDP attribute or `urn:x-nvnmos:tag:name` flow-def
//!   tag inside the transport file), and the side (sender or receiver).
//!   A secondary `(node_seed, side, name) → resource_handle` index
//!   supports the daemon-level pre-add duplicate check and the
//!   activation router's lookup back from libnvnmos's (side, name) pair
//!   to the owning session. `name` is unique only within a side: a
//!   Sender and a Receiver are permitted to share a name on the same
//!   Node.
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
//!
//! **Locking:** no libnvnmos FFI while `state` is held. Mutations use
//! prepare / deferred `*Ffi` or `*Prep::run_ffi` / commit phases; see
//! `doc/designs/nvnmosd/lock-ordering.md`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use nvnmos::{
    Activation, AssetConfig, ChannelMappingActivation, ChannelMappingActiveMapEntry,
    ChannelMappingConfig, ChannelMappingInput, ChannelMappingOutput, ChannelMappingParentType,
    NetworkServicesConfig, NodeConfig, NodeServer, ReceiverConfig, SenderConfig,
    Side as WrapperSide, Transport,
};
use nvnmos_rpc::v1::{
    ActivationEvent, ActiveMapEntry as ProtoActiveMapEntry, AssetConfig as ProtoAssetConfig,
    ChannelMappingActivationEvent, ChannelMappingInput as ProtoChannelMappingInput,
    ChannelMappingOutput as ProtoChannelMappingOutput,
    ChannelMappingParentType as ProtoChannelMappingParentType,
    NetworkServicesConfig as ProtoNetworkServicesConfig, NodeConfig as ProtoNodeConfig,
    Side as ProtoSide, Transport as ProtoTransport,
};
use tokio::sync::mpsc as tokio_mpsc;
use tonic::Status;

use crate::http_port::{self, PortRange};
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
///
/// Mirrors the C `NvNmosSide` enum surfaced by libnvnmos (via the
/// [`nvnmos::Side`] wrapper, imported here as [`WrapperSide`]) and the
/// [`ProtoSide`] gRPC enum. We carry our own copy here so the daemon's
/// internal types don't bleed into either surface, and convert at the
/// boundary via [`Self::to_proto`] / [`Self::to_wrapper`] /
/// [`Self::from_wrapper`]. (There is no `from_proto` today because the
/// proto carries `side` only outbound, in `ActivationEvent`; inbound
/// RPCs pin the side by which one is called.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// IS-04 / IS-05 sender (`/senders/<id>`).
    Sender,
    /// IS-04 / IS-05 receiver (`/receivers/<id>`).
    Receiver,
}

impl Side {
    /// Short label for log lines and gRPC error messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::Sender => "sender",
            Self::Receiver => "receiver",
        }
    }

    /// Project to a wire-format [`ProtoSide`] for outbound
    /// [`ActivationEvent`]s.
    pub fn to_proto(self) -> ProtoSide {
        match self {
            Self::Sender => ProtoSide::Sender,
            Self::Receiver => ProtoSide::Receiver,
        }
    }

    /// Project to the safe-wrapper [`WrapperSide`] used by libnvnmos calls
    /// (notably [`NodeServer::activate_connection`]).
    pub fn to_wrapper(self) -> WrapperSide {
        match self {
            Self::Sender => WrapperSide::Sender,
            Self::Receiver => WrapperSide::Receiver,
        }
    }

    /// Translate the wrapper's [`WrapperSide`] (e.g. from an inbound
    /// activation callback) into a daemon-local [`Side`].
    pub fn from_wrapper(side: WrapperSide) -> Self {
        match side {
            WrapperSide::Sender => Self::Sender,
            WrapperSide::Receiver => Self::Receiver,
        }
    }

    /// Dispatch the wrapper's `add_*` call for this side.
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
    /// `Ok(None)` when libnvnmos does not have a resource of this side
    /// with the given `name`. Used as the post-add validation
    /// primitive by [`State::add_resource`].
    fn lookup_id(self, server: &NodeServer, name: &str) -> nvnmos::Result<Option<String>> {
        match self {
            Self::Sender => server.sender_id(name),
            Self::Receiver => server.receiver_id(name),
        }
    }

    /// Dispatch the wrapper's `remove_*` call. Used both by
    /// [`State::remove_resource`] and by [`State::close_session`] when
    /// dropping a session's resources before tearing down the Node.
    fn remove_from_server(self, server: &NodeServer, name: &str) -> nvnmos::Result<()> {
        match self {
            Self::Sender => server.remove_sender(name),
            Self::Receiver => server.remove_receiver(name),
        }
    }
}

/// A live resource (sender or receiver) owned by a session.
struct ResourceEntry {
    /// The resource name carried by the transport file (`x-nvnmos-name`
    /// SDP attribute or `urn:x-nvnmos:tag:name` flow-def tag). The daemon
    /// validated at AddSender/AddReceiver time that this matched what
    /// libnvnmos extracted; see the proto's "Resource lifecycle" section
    /// for the validation contract.
    name: String,
    /// Seed of the Node the resource lives on. Stored so
    /// [`State::close_session`] and the activation router
    /// ([`State::dispatch_activation`]) can find the right
    /// [`NodeEntry`] back from a resource.
    node_seed: String,
    /// Session that created this resource. Closing that session drops the
    /// resource; only that session is allowed to remove it. Read by the
    /// activation router ([`State::dispatch_activation`]) to find the
    /// right subscriber stream for an incoming libnvnmos activation.
    session_handle: String,
    /// Sender vs receiver — dispatches the libnvnmos API call.
    side: Side,
}

/// A live Node owned by the daemon.
struct NodeEntry {
    /// The C node server itself, behind an [`Arc`] so an RPC handler can
    /// clone a reference, drop the `state` lock, and then run libnvnmos
    /// FFI (or the `Drop` that calls `destroy_nmos_node_server`) with no
    /// daemon lock held. Dropping the last clone tears down the server.
    server: Arc<NodeServer>,
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
    /// TCP port libnvnmos listens on for this Node's HTTP APIs.
    http_port: u16,
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
    /// `channelmapping_handle`s owned by this session.
    channelmappings: HashSet<String>,
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
    /// Effective HTTP API port of the attached Node.
    pub http_port: u16,
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
    pub device_id: String,
    pub http_port: u16,
}

/// Outcome of [`State::remove_node`].
#[derive(Debug)]
pub struct RemoveNodeOutcome {
    pub node_id: String,
}

/// Outcome of adding a Sender.
#[derive(Debug)]
pub struct AddSenderOutcome {
    pub resource_handle: String,
    /// NMOS Source UUID paired with this Sender.
    pub source_id: String,
    /// NMOS Flow UUID paired with this Sender.
    pub flow_id: String,
    /// NMOS Sender UUID returned by libnvnmos.
    pub sender_id: String,
    pub node_seed: String,
}

/// Outcome of adding a Receiver.
#[derive(Debug)]
pub struct AddReceiverOutcome {
    pub resource_handle: String,
    /// NMOS Receiver UUID returned by libnvnmos.
    pub receiver_id: String,
    pub node_seed: String,
}

/// Outcome of [`State::remove_resource`].
#[derive(Debug)]
pub struct RemoveResourceOutcome {
    pub node_seed: String,
    pub name: String,
    pub side: Side,
}

/// Outcome of [`State::sync_resource_state`].
#[derive(Debug)]
pub struct SyncResourceStateOutcome {
    pub node_seed: String,
    pub name: String,
    pub side: Side,
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

struct ChannelMappingEntry {
    name: String,
    node_seed: String,
    session_handle: String,
    output_ids: HashSet<String>,
}

struct ChannelMappingActivationSubscriber {
    tx: tokio_mpsc::Sender<Result<ChannelMappingActivationEvent, Status>>,
}

struct PendingChannelMappingActivation {
    session_handle: String,
    ack_tx: std_mpsc::SyncSender<AckOutcome>,
}

#[derive(Debug)]
pub struct AddChannelMappingOutcome {
    pub channelmapping_handle: String,
    pub input_ids: Vec<String>,
    pub output_ids: Vec<String>,
    pub node_seed: String,
}

#[derive(Debug)]
pub struct RemoveChannelMappingOutcome {
    pub node_seed: String,
    pub name: String,
}

#[derive(Debug)]
pub struct SyncChannelMappingStateOutcome {
    pub node_seed: String,
    pub name: String,
}

pub enum ChannelMappingActivationDispatch {
    Routed {
        activation_handle: String,
        ack_rx: std_mpsc::Receiver<AckOutcome>,
    },
    /// No channel mapping entry exists for `(node_seed, output_id)` —
    /// stray or not yet committed. The router NACKs; it does not remove
    /// the libnvnmos mapping.
    NoChannelMapping,
    NoSubscriber,
    SubscriberBusy,
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
    /// No resource entry exists for `(node_seed, side, name)` — either a
    /// stray (libnvnmos object with no daemon map entry) or a resource
    /// removed between the IS-05 PATCH and the callback. The router NACKs;
    /// it does not remove the libnvnmos object.
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

/// Deferred libnvnmos work returned by a `State::*` bookkeeping phase so
/// the RPC handler can run it **after** dropping the `state` lock.
///
/// Every FFI call takes libnvnmos's internal `model` lock, and the
/// in-band activation callback path takes `model` then `state`; running
/// FFI while `state` is held would be an AB-BA deadlock. These `*Ffi` /
/// `*Prep` types are the seam that keeps FFI off the locked path.
///
/// This one drops a session's resources (and, when the Node is being
/// destroyed, the last [`NodeServer`] reference) with no daemon lock
/// held, so a parked activation thread can take `state`, finish, and let
/// `destroy_nmos_node_server`'s thread-joins return.
#[must_use = "close_session FFI must run outside the state lock"]
pub struct CloseSessionFfi {
    server: Arc<NodeServer>,
    resource_removals: Vec<(Side, String)>,
    channelmapping_removals: Vec<String>,
    session_handle: String,
}

impl CloseSessionFfi {
    pub fn run(self) {
        for (side, name) in &self.resource_removals {
            if let Err(e) = side.remove_from_server(&self.server, name) {
                tracing::warn!(
                    session_handle = %self.session_handle,
                    side = side.label(),
                    name = %name,
                    error = %e,
                    "close_session: libnvnmos remove_sender/remove_receiver failed; \
                     continuing"
                );
            }
        }
        for name in &self.channelmapping_removals {
            if let Err(e) = self.server.remove_channelmapping(name) {
                tracing::warn!(
                    session_handle = %self.session_handle,
                    name = %name,
                    error = %e,
                    "close_session: libnvnmos remove_channelmapping failed; continuing"
                );
            }
        }
        // Dropping the last reference runs `destroy_nmos_node_server`
        // (shutdown + thread-join + listener close) here, outside the
        // lock. If the entry was retained (sessions remain), this is not
        // the last reference and only decrements the count.
        drop(self.server);
    }
}

/// Drops the last [`NodeServer`] reference for a torn-down persistent
/// Node outside the `state` lock. See [`CloseSessionFfi`].
#[must_use = "remove_node FFI must run outside the state lock"]
pub struct RemoveNodeFfi {
    server: Arc<NodeServer>,
}

impl RemoveNodeFfi {
    pub fn run(self) {
        drop(self.server);
    }
}

/// Best-effort libnvnmos removal of one resource, run after the daemon
/// bookkeeping is already consistent. See [`CloseSessionFfi`].
#[must_use = "remove_resource FFI must run outside the state lock"]
pub struct RemoveResourceFfi {
    /// `None` when the Node had already gone (nothing to remove).
    server: Option<Arc<NodeServer>>,
    side: Side,
    name: String,
}

impl RemoveResourceFfi {
    pub fn run(self) {
        let Some(server) = self.server else { return };
        if let Err(e) = self.side.remove_from_server(&server, &self.name) {
            tracing::warn!(
                side = self.side.label(),
                name = %self.name,
                error = %e,
                "remove_resource: libnvnmos remove_sender/remove_receiver failed; \
                 daemon state already cleared"
            );
        }
    }
}

/// Pushes a resource (re)activation / deactivation into libnvnmos. Runs
/// after the `state` lock is dropped; its result is the RPC result. See
/// [`CloseSessionFfi`].
#[must_use = "sync_resource_state FFI must run outside the state lock"]
pub struct SyncResourceFfi {
    server: Arc<NodeServer>,
    side: Side,
    name: String,
    transport_file: Option<String>,
}

impl SyncResourceFfi {
    pub fn run(self) -> Result<(), Status> {
        self.server
            .activate_connection(
                self.side.to_wrapper(),
                &self.name,
                self.transport_file.as_deref(),
            )
            .map_err(|e| {
                let verb = if self.transport_file.is_some() {
                    "activate"
                } else {
                    "deactivate"
                };
                Status::invalid_argument(format!(
                    "libnvnmos {verb} for {} {:?} failed (transport_file \
                     parse error or libnvnmos state mismatch): {e}",
                    self.side.label(),
                    self.name,
                ))
            })
    }
}

/// First phase of `AddSender` / `AddReceiver`: daemon bookkeeping is
/// validated under `state`, then the lock is dropped and
/// [`AddResourcePrep::run_ffi`] performs the libnvnmos add + name-match
/// lookup with no lock held. [`State::commit_add_sender`] /
/// [`State::commit_add_receiver`] finish the bookkeeping. See
/// [`CloseSessionFfi`].
#[must_use = "add_resource must proceed through run_ffi + commit_add_*"]
pub struct AddResourcePrep {
    server: Arc<NodeServer>,
    side: Side,
    transport: Transport,
    transport_file: String,
    name: String,
    node_seed: String,
    session_handle: String,
}

/// Output of [`AddResourcePrep::run_ffi`] for a Sender, fed to
/// [`State::commit_add_sender`].
pub struct AddSenderReady {
    name: String,
    node_seed: String,
    session_handle: String,
    source_id: String,
    flow_id: String,
    sender_id: String,
}

/// Output of [`AddResourcePrep::run_ffi`] for a Receiver, fed to
/// [`State::commit_add_receiver`].
pub struct AddReceiverReady {
    name: String,
    node_seed: String,
    session_handle: String,
    receiver_id: String,
}

/// Side-specific result of [`AddResourcePrep::run_ffi`].
pub enum AddResourceReady {
    Sender(AddSenderReady),
    Receiver(AddReceiverReady),
}

impl AddResourcePrep {
    pub fn run_ffi(self) -> Result<AddResourceReady, Status> {
        self.side
            .add_to_server(&self.server, self.transport, &self.transport_file)
            .map_err(|e| {
                Status::invalid_argument(format!(
                    "libnvnmos add_{} failed (transport_file parse error or \
                     duplicate): {e}",
                    self.side.label(),
                ))
            })?;

        let resource_id = match self.side.lookup_id(&self.server, &self.name) {
            Ok(Some(id)) => id,
            Ok(None) => {
                tracing::error!(
                    side = self.side.label(),
                    claimed_name = %self.name,
                    node_seed = %self.node_seed,
                    "AddSender/AddReceiver: libnvnmos accepted the transport \
                     file but its embedded name does not match the claimed \
                     name; left as a stray until the Node is destroyed"
                );
                return Err(Status::invalid_argument(format!(
                    "{}'s transport_file embeds a different name than {:?}",
                    self.side.label(),
                    self.name,
                )));
            }
            Err(e) => {
                return Err(Status::internal(format!(
                    "querying {} id from libnvnmos failed after add: {e}",
                    self.side.label(),
                )));
            }
        };

        match self.side {
            Side::Sender => {
                let source_id = self
                    .server
                    .source_id(&self.name)
                    .map_err(|e| {
                        Status::internal(format!(
                            "querying source id from libnvnmos failed after add: {e}"
                        ))
                    })?
                    .ok_or_else(|| {
                        Status::internal(
                            "libnvnmos accepted the Sender but source_id lookup returned None",
                        )
                    })?;
                let flow_id = self
                    .server
                    .flow_id(&self.name)
                    .map_err(|e| {
                        Status::internal(format!(
                            "querying flow id from libnvnmos failed after add: {e}"
                        ))
                    })?
                    .ok_or_else(|| {
                        Status::internal(
                            "libnvnmos accepted the Sender but flow_id lookup returned None",
                        )
                    })?;
                Ok(AddResourceReady::Sender(AddSenderReady {
                    name: self.name,
                    node_seed: self.node_seed,
                    session_handle: self.session_handle,
                    source_id,
                    flow_id,
                    sender_id: resource_id,
                }))
            }
            Side::Receiver => Ok(AddResourceReady::Receiver(AddReceiverReady {
                name: self.name,
                node_seed: self.node_seed,
                session_handle: self.session_handle,
                receiver_id: resource_id,
            })),
        }
    }
}

/// Best-effort libnvnmos removal of one channel mapping. See
/// [`CloseSessionFfi`].
#[must_use = "remove_channelmapping FFI must run outside the state lock"]
pub struct RemoveChannelMappingFfi {
    server: Option<Arc<NodeServer>>,
    name: String,
}

impl RemoveChannelMappingFfi {
    pub fn run(self) {
        let Some(server) = self.server else { return };
        if let Err(e) = server.remove_channelmapping(&self.name) {
            tracing::warn!(
                name = %self.name,
                error = %e,
                "remove_channelmapping: libnvnmos remove failed; daemon state already cleared"
            );
        }
    }
}

/// Pushes an IS-08 activation into libnvnmos. See [`SyncResourceFfi`].
#[must_use = "sync_channelmapping_state FFI must run outside the state lock"]
pub struct SyncChannelMappingFfi {
    server: Arc<NodeServer>,
    name: String,
    output_id: String,
    active_map: Vec<ChannelMappingActiveMapEntry>,
}

impl SyncChannelMappingFfi {
    pub fn run(self) -> Result<(), Status> {
        self.server
            .activate_channelmapping(&self.name, &self.output_id, &self.active_map)
            .map_err(|e| {
                Status::invalid_argument(format!(
                    "libnvnmos nmos_channelmapping_activate failed: {e}"
                ))
            })
    }
}

/// First phase of `AddChannelMapping` (parallel to [`AddResourcePrep`]).
#[must_use = "add_channelmapping must proceed through run_ffi + commit_add_channelmapping"]
pub struct AddChannelMappingPrep {
    server: Arc<NodeServer>,
    name: String,
    mapping: ChannelMappingConfig,
    node_seed: String,
    session_handle: String,
    effective_input_ids: Vec<String>,
    effective_output_ids: Vec<String>,
}

/// Output of [`AddChannelMappingPrep::run_ffi`], fed to
/// [`State::commit_add_channelmapping`].
pub struct AddChannelMappingReady {
    name: String,
    node_seed: String,
    session_handle: String,
    effective_input_ids: Vec<String>,
    effective_output_ids: Vec<String>,
}

impl AddChannelMappingPrep {
    pub fn run_ffi(self) -> Result<AddChannelMappingReady, Status> {
        self.server
            .add_channelmapping(&self.name, &self.mapping)
            .map_err(|e| {
                Status::invalid_argument(format!(
                    "libnvnmos add_channelmapping failed (duplicate or invalid geometry): {e}"
                ))
            })?;
        Ok(AddChannelMappingReady {
            name: self.name,
            node_seed: self.node_seed,
            session_handle: self.session_handle,
            effective_input_ids: self.effective_input_ids,
            effective_output_ids: self.effective_output_ids,
        })
    }
}

/// First phase of `OpenSession(Create)` / `AddNode`: a pending node whose
/// port and seed are recorded in daemon state. The caller must
/// [`CreateNodePrep::run_ffi`] outside the `state` lock (supplying
/// daemon-specific activation callbacks), then
/// [`State::commit_open_session`] / [`State::commit_add_node`] (or
/// [`State::abort_pending_node`] on build failure).
#[must_use = "node create must proceed through run_ffi + commit_*"]
pub struct CreateNodePrep {
    pub seed: String,
    pub config: Box<NodeConfig>,
    pub http_port: u16,
}

/// Output of [`CreateNodePrep::run_ffi`], fed to
/// [`State::commit_open_session`] or [`State::commit_add_node`].
pub struct CreateNodeReady {
    pub seed: String,
    pub http_port: u16,
    pub server: Arc<NodeServer>,
    pub node_id: String,
    pub device_id: String,
}

impl CreateNodePrep {
    /// Second phase of `OpenSession(Create)` / `AddNode`: build a
    /// [`NodeServer`] via `create_nmos_node_server` with no `state` lock
    /// held, then query its `node_id`. Activation callbacks are supplied
    /// by the daemon because they route into
    /// [`State::dispatch_activation`].
    pub fn run_ffi<A, C>(
        self,
        on_activation: A,
        on_channelmapping_activation: C,
    ) -> Result<CreateNodeReady, Status>
    where
        A: Fn(&Activation<'_>) -> std::result::Result<(), String> + Send + Sync + 'static,
        C: Fn(&ChannelMappingActivation<'_>) -> std::result::Result<(), String>
            + Send
            + Sync
            + 'static,
    {
        let server = NodeServer::builder(self.config.as_ref())
            .on_log(log_bridge::forward)
            .on_activation(on_activation)
            .on_channelmapping_activation(on_channelmapping_activation)
            .build()
            .map_err(|e| Status::internal(format!("create_nmos_node_server failed: {e}")))?;

        let node_id = server.node_id().map_err(|e| {
            Status::internal(format!(
                "querying node_id from the new NodeServer failed: {e}"
            ))
        })?;
        let device_id = server.device_id().map_err(|e| {
            Status::internal(format!(
                "querying device_id from the new NodeServer failed: {e}"
            ))
        })?;

        Ok(CreateNodeReady {
            seed: self.seed,
            http_port: self.http_port,
            server: Arc::new(server),
            node_id,
            device_id,
        })
    }
}

/// Result of [`State::prepare_open_session`].
pub enum OpenSessionPlan {
    /// A Node already existed for the seed; the session was allocated and
    /// attached entirely under `state`. No FFI needed.
    Attached(OpenOutcome),
    /// No Node existed: a pending node now owns the HTTP port and seed.
    /// The handler must [`CreateNodePrep::run_ffi`] outside the `state`
    /// lock, then call [`State::commit_open_session`] (or
    /// [`State::abort_pending_node`] on failure).
    Create(CreateNodePrep),
}

/// All daemon state. Wrapped in `Arc<Mutex<…>>` by the gRPC service.
pub struct State {
    nodes: HashMap<String, NodeEntry>,
    /// Seeds of pending nodes: nodes prepared but not yet committed. The
    /// port is already in [`State::http_ports`] and the seed is here, but
    /// the [`NodeEntry`] is not yet in [`State::nodes`] — its
    /// [`NodeServer`] is being built outside the `state` lock. Guards
    /// against a second concurrent create for the same seed while the
    /// first is still pending.
    pending_nodes: HashSet<String>,
    sessions: HashMap<String, SessionEntry>,
    /// Live resources keyed by daemon-allocated `resource_handle`.
    resources: HashMap<String, ResourceEntry>,
    /// Secondary index: `(node_seed, side, name) → resource_handle`. The
    /// `side` axis is what permits a Sender and a Receiver to share the
    /// same `name` on the Node. Used for the pre-add duplicate check
    /// and for routing activation callbacks from libnvnmos back to the
    /// owning session.
    by_name: HashMap<(String, Side, String), String>,
    /// At most one `SubscribeActivations` subscriber per session.
    subscriptions: HashMap<String, ActivationSubscriber>,
    /// Activations currently waiting on `AckActivation`, keyed by
    /// daemon-allocated `activation_handle`.
    pending_activations: HashMap<String, PendingActivation>,
    channelmappings: HashMap<String, ChannelMappingEntry>,
    channelmappings_by_name: HashMap<(String, String), String>,
    /// `(node_seed, output_id) → channelmapping_handle` — activation dispatch
    /// index (parallel to IS-05 `by_name → resource_handle`).
    outputs_by_id: HashMap<(String, String), String>,
    channelmapping_subscriptions: HashMap<String, ChannelMappingActivationSubscriber>,
    pending_channelmapping_activations: HashMap<String, PendingChannelMappingActivation>,
    /// `http_port` → `node_seed` for Nodes currently owned by the daemon.
    http_ports: HashMap<u16, String>,
    next_session_id: AtomicU64,
    next_resource_id: AtomicU64,
    next_activation_id: AtomicU64,
    next_channelmapping_id: AtomicU64,
    next_channelmapping_activation_id: AtomicU64,
}

impl State {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            pending_nodes: HashSet::new(),
            sessions: HashMap::new(),
            resources: HashMap::new(),
            by_name: HashMap::new(),
            subscriptions: HashMap::new(),
            pending_activations: HashMap::new(),
            channelmappings: HashMap::new(),
            channelmappings_by_name: HashMap::new(),
            outputs_by_id: HashMap::new(),
            channelmapping_subscriptions: HashMap::new(),
            pending_channelmapping_activations: HashMap::new(),
            http_ports: HashMap::new(),
            next_session_id: AtomicU64::new(0),
            next_resource_id: AtomicU64::new(0),
            next_activation_id: AtomicU64::new(0),
            next_channelmapping_id: AtomicU64::new(0),
            next_channelmapping_activation_id: AtomicU64::new(0),
        }
    }

    /// First phase of `OpenSession`: attach to an existing Node or record
    /// a new pending node for `seed`.
    ///
    /// If a Node already exists for `seed` (either [`Lifetime::Persistent`]
    /// or [`Lifetime::SessionRefcounted`]), this attaches to it,
    /// increments its session count, allocates the session, and returns
    /// [`OpenSessionPlan::Attached`] — no FFI needed, so there is no
    /// commit phase. If no Node exists, this records a pending node (its
    /// HTTP port and seed) and returns [`OpenSessionPlan::Create`]; the
    /// caller must [`CreateNodePrep::run_ffi`] outside the `state` lock
    /// and then call
    /// [`State::commit_open_session`] (or [`State::abort_pending_node`]
    /// on build failure).
    ///
    /// Errors:
    /// * `INVALID_ARGUMENT` if `seed` is empty (the empty seed would
    ///   collapse every "default" session onto the same Node and lose
    ///   determinism of the NMOS UUIDs).
    /// * `ABORTED` if a create for the same seed is already pending.
    /// * Whatever [`State::resolve_http_port`] returns.
    pub fn prepare_open_session(
        &mut self,
        mut config: NodeConfig,
        port_range: &PortRange,
    ) -> Result<OpenSessionPlan, Status> {
        let seed = config.seed.clone();
        require_non_empty_seed(&seed)?;

        if let Some(entry) = self.nodes.get_mut(&seed) {
            entry.attached_sessions += 1;
            let node_id = entry.node_id.clone();
            let lifetime = entry.lifetime;
            let http_port = entry.http_port;
            let session_handle = self.allocate_session_handle();
            self.sessions.insert(
                session_handle.clone(),
                SessionEntry {
                    node_seed: seed,
                    resources: HashSet::new(),
                    channelmappings: HashSet::new(),
                },
            );
            return Ok(OpenSessionPlan::Attached(OpenOutcome {
                session_handle,
                node_id,
                lifetime,
                created_node: false,
                http_port,
            }));
        }

        if self.pending_nodes.contains(&seed) {
            return Err(Status::aborted(format!(
                "a Node for seed {seed:?} is already being created; retry"
            )));
        }

        let http_port = self.resolve_http_port(config.http_port, port_range)?;
        config.http_port = http_port;
        self.http_ports.insert(http_port, seed.clone());
        self.pending_nodes.insert(seed.clone());
        Ok(OpenSessionPlan::Create(CreateNodePrep {
            seed,
            config: Box::new(config),
            http_port,
        }))
    }

    /// Third phase of `OpenSession`: commit the pending node built for
    /// [`OpenSessionPlan::Create`] into a live [`NodeEntry`], and allocate
    /// the first session attached to it.
    pub fn commit_open_session(&mut self, ready: CreateNodeReady) -> OpenOutcome {
        self.pending_nodes.remove(&ready.seed);
        self.nodes.insert(
            ready.seed.clone(),
            NodeEntry {
                server: ready.server,
                node_id: ready.node_id.clone(),
                lifetime: Lifetime::SessionRefcounted,
                attached_sessions: 1,
                http_port: ready.http_port,
            },
        );
        let session_handle = self.allocate_session_handle();
        self.sessions.insert(
            session_handle.clone(),
            SessionEntry {
                node_seed: ready.seed,
                resources: HashSet::new(),
                channelmappings: HashSet::new(),
            },
        );
        OpenOutcome {
            session_handle,
            node_id: ready.node_id,
            lifetime: Lifetime::SessionRefcounted,
            created_node: true,
            http_port: ready.http_port,
        }
    }

    /// Abort a pending node ([`OpenSessionPlan::Create`] / [`CreateNodePrep`])
    /// when the FFI build failed: its seed and its port are removed.
    pub fn abort_pending_node(&mut self, seed: &str, http_port: u16) {
        self.pending_nodes.remove(seed);
        if self.http_ports.get(&http_port).is_some_and(|s| s == seed) {
            self.http_ports.remove(&http_port);
        }
    }

    /// Close a session, detaching from the backing Node.
    ///
    /// All daemon bookkeeping (removing the session, its resources and
    /// channel mappings, its subscriptions, and aborting its in-flight
    /// activations) happens under the `state` lock. The libnvnmos work —
    /// removing each resource / channel mapping and, when this was the
    /// last session on a [`Lifetime::SessionRefcounted`] Node, destroying
    /// the [`NodeServer`] — is deferred to the returned [`CloseSessionFfi`]
    /// so it runs with no daemon lock held. That is what lets a parked
    /// activation thread take `state`, finish, and let
    /// `destroy_nmos_node_server`'s thread-joins return (otherwise the
    /// HTTP listener is stranded in `LISTEN`). [`Lifetime::Persistent`]
    /// Nodes are never destroyed by this call.
    pub fn close_session(
        &mut self,
        session_handle: &str,
    ) -> Result<(CloseOutcome, CloseSessionFfi), Status> {
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
        let server = self
            .nodes
            .get(&seed)
            .ok_or_else(|| {
                Status::internal(format!(
                    "session {session_handle:?} referenced seed {seed:?} but no Node entry exists"
                ))
            })?
            .server
            .clone();

        // Clear the session's resources from daemon state and collect the
        // libnvnmos removals to run after the lock is dropped. Resources
        // are dropped from daemon state regardless of the deferred FFI
        // outcome — the session is going away.
        let mut resource_removals: Vec<(Side, String)> = Vec::new();
        for resource_handle in session.resources {
            let Some(resource) = self.resources.remove(&resource_handle) else {
                tracing::warn!(
                    %resource_handle,
                    session_handle,
                    "close_session: session referenced unknown resource_handle"
                );
                continue;
            };
            self.by_name.remove(&(
                resource.node_seed.clone(),
                resource.side,
                resource.name.clone(),
            ));
            resource_removals.push((resource.side, resource.name));
        }

        let mut channelmapping_removals: Vec<String> = Vec::new();
        for channelmapping_handle in session.channelmappings {
            if let Some(entry) = self.channelmappings.remove(&channelmapping_handle) {
                self.channelmappings_by_name
                    .remove(&(entry.node_seed.clone(), entry.name.clone()));
                for output_id in &entry.output_ids {
                    self.outputs_by_id
                        .remove(&(entry.node_seed.clone(), output_id.clone()));
                }
                channelmapping_removals.push(entry.name);
            }
        }

        self.channelmapping_subscriptions.remove(session_handle);
        let aborted_cm: Vec<String> = self
            .pending_channelmapping_activations
            .iter()
            .filter(|(_, p)| p.session_handle == session_handle)
            .map(|(h, _)| h.clone())
            .collect();
        for handle in aborted_cm {
            self.pending_channelmapping_activations.remove(&handle);
        }

        let entry = self
            .nodes
            .get_mut(&seed)
            .expect("checked above and we never removed it in this scope");
        let node_id = entry.node_id.clone();
        let lifetime = entry.lifetime;
        entry.attached_sessions = entry.attached_sessions.saturating_sub(1);
        let remaining_sessions = entry.attached_sessions;
        let node_destroyed = lifetime == Lifetime::SessionRefcounted && remaining_sessions == 0;
        if node_destroyed {
            self.http_ports.remove(&entry.http_port);
            self.nodes.remove(&seed);
        }
        let outcome = CloseOutcome {
            node_seed: seed,
            node_id,
            lifetime,
            remaining_sessions,
            node_destroyed,
        };
        let ffi = CloseSessionFfi {
            server,
            resource_removals,
            channelmapping_removals,
            session_handle: session_handle.to_string(),
        };
        Ok((outcome, ffi))
    }

    /// Count live senders/receivers created on `node_seed`.
    pub fn resource_count_for_node(&self, node_seed: &str) -> usize {
        self.resources
            .values()
            .filter(|resource| resource.node_seed == node_seed)
            .count()
    }

    /// First phase of `AddNode`: record a pending node (its HTTP port and
    /// seed) for a persistent Node and return a [`CreateNodePrep`]. The
    /// caller must [`CreateNodePrep::run_ffi`] outside the `state` lock
    /// and then call
    /// [`State::commit_add_node`] (or [`State::abort_pending_node`] on
    /// build failure).
    ///
    /// Errors:
    /// * `INVALID_ARGUMENT` if `seed` is empty.
    /// * `ALREADY_EXISTS` if any Node (persistent or session-refcounted)
    ///   currently exists for `seed`, or a create for it is already pending.
    /// * Whatever [`State::resolve_http_port`] returns.
    pub fn prepare_add_node(
        &mut self,
        mut config: NodeConfig,
        port_range: &PortRange,
    ) -> Result<CreateNodePrep, Status> {
        let seed = config.seed.clone();
        require_non_empty_seed(&seed)?;
        if let Some(entry) = self.nodes.get(&seed) {
            return Err(Status::already_exists(format!(
                "a {} Node already exists for seed {seed:?}",
                entry.lifetime.label(),
            )));
        }
        if self.pending_nodes.contains(&seed) {
            return Err(Status::already_exists(format!(
                "a Node for seed {seed:?} is already being created"
            )));
        }
        let http_port = self.resolve_http_port(config.http_port, port_range)?;
        config.http_port = http_port;
        self.http_ports.insert(http_port, seed.clone());
        self.pending_nodes.insert(seed.clone());
        Ok(CreateNodePrep {
            seed,
            config: Box::new(config),
            http_port,
        })
    }

    /// Third phase of `AddNode`: commit the pending node built for a
    /// [`CreateNodePrep`] into a live persistent [`NodeEntry`].
    pub fn commit_add_node(&mut self, ready: CreateNodeReady) -> AddNodeOutcome {
        self.pending_nodes.remove(&ready.seed);
        self.nodes.insert(
            ready.seed,
            NodeEntry {
                server: ready.server,
                node_id: ready.node_id.clone(),
                lifetime: Lifetime::Persistent,
                attached_sessions: 0,
                http_port: ready.http_port,
            },
        );
        AddNodeOutcome {
            node_id: ready.node_id,
            device_id: ready.device_id,
            http_port: ready.http_port,
        }
    }

    /// Tear down a persistent Node.
    ///
    /// Errors:
    /// * `INVALID_ARGUMENT` if `seed` is empty.
    /// * `NOT_FOUND` if no Node exists for `seed`.
    /// * `FAILED_PRECONDITION` if the Node is [`Lifetime::SessionRefcounted`]
    ///   (the caller must close sessions instead) or if any sessions are
    ///   currently attached (the caller must close them first).
    pub fn remove_node(
        &mut self,
        seed: &str,
    ) -> Result<(RemoveNodeOutcome, RemoveNodeFfi), Status> {
        require_non_empty_seed(seed)?;
        let entry = self
            .nodes
            .get(seed)
            .ok_or_else(|| Status::not_found(format!("no Node exists for seed {seed:?}")))?;
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
        self.http_ports.remove(&entry.http_port);
        // Defer the `NodeServer` drop (destroy + thread-join) to run
        // outside the `state` lock; see [`RemoveNodeFfi`].
        Ok((
            RemoveNodeOutcome {
                node_id: entry.node_id,
            },
            RemoveNodeFfi {
                server: entry.server,
            },
        ))
    }

    /// First phase of `AddSender` / `AddReceiver`: validate daemon-side
    /// bookkeeping and hand back an [`AddResourcePrep`] carrying an
    /// [`Arc<NodeServer>`] for the FFI add.
    ///
    /// The full validation flow (spanning the three phases):
    ///
    /// 1. Pre-check in daemon state — refuses a duplicate `name` on the
    ///    same Node before any FFI happens (here).
    /// 2. `add_{sender,receiver}` into libnvnmos, then
    ///    `{sender,receiver}_id(claimed_name)` — libnvnmos is the oracle:
    ///    success proves the transport file's id equalled the claim
    ///    ([`AddResourcePrep::run_ffi`], no lock held).
    /// 3. On success, finalise the maps ([`State::commit_add_sender`] /
    ///    [`State::commit_add_receiver`]).
    ///    On a name mismatch the libnvnmos resource is left as a stray
    ///    (no compensating remove); activations NACK with
    ///    [`ActivationDispatch::NoResource`]; the stray is torn down when
    ///    the Node is destroyed. See the proto's "Resource lifecycle"
    ///    section.
    pub fn prepare_add_resource(
        &mut self,
        side: Side,
        session_handle: &str,
        transport: Transport,
        transport_file: &str,
        claimed_name: &str,
    ) -> Result<AddResourcePrep, Status> {
        if claimed_name.is_empty() {
            return Err(Status::invalid_argument("name must be non-empty"));
        }

        self.reap_closed_subscription(session_handle);
        if !self.has_active_subscription(session_handle) {
            return Err(Status::failed_precondition(
                "SubscribeActivations required before adding a resource",
            ));
        }

        let session = self.sessions.get(session_handle).ok_or_else(|| {
            Status::not_found(format!(
                "session_handle {session_handle:?} is not known to this daemon"
            ))
        })?;
        let node_seed = session.node_seed.clone();
        let key = (node_seed.clone(), side, claimed_name.to_string());

        if let Some(existing) = self.by_name.get(&key) {
            return Err(Status::already_exists(format!(
                "a {} with name {claimed_name:?} is already \
                 created on node_seed {node_seed:?} as resource_handle \
                 {existing:?}",
                side.label(),
            )));
        }

        let server = self
            .nodes
            .get(&node_seed)
            .ok_or_else(|| {
                Status::internal(format!(
                    "session {session_handle:?} referenced seed {node_seed:?} \
                     but no Node entry exists"
                ))
            })?
            .server
            .clone();

        Ok(AddResourcePrep {
            server,
            side,
            transport,
            transport_file: transport_file.to_string(),
            name: claimed_name.to_string(),
            node_seed,
            session_handle: session_handle.to_string(),
        })
    }

    /// Third phase of `AddSender`: record the Sender in daemon state after
    /// the libnvnmos add + id lookups succeeded.
    ///
    /// The session must still exist; if it was closed while the FFI ran
    /// (a concurrent `CloseSession`), the libnvnmos resource is left as a
    /// stray (no compensating remove; torn down when the Node is destroyed)
    /// and this returns `NOT_FOUND`.
    pub fn commit_add_sender(&mut self, ready: AddSenderReady) -> Result<AddSenderOutcome, Status> {
        let resource_handle = self.commit_added_resource(
            Side::Sender,
            &ready.name,
            &ready.node_seed,
            &ready.session_handle,
        )?;
        Ok(AddSenderOutcome {
            resource_handle,
            source_id: ready.source_id,
            flow_id: ready.flow_id,
            sender_id: ready.sender_id,
            node_seed: ready.node_seed,
        })
    }

    /// Third phase of `AddReceiver`: record the Receiver in daemon state
    /// after the libnvnmos add + id lookup succeeded.
    ///
    /// The session must still exist; if it was closed while the FFI ran
    /// (a concurrent `CloseSession`), the libnvnmos resource is left as a
    /// stray (no compensating remove; torn down when the Node is destroyed)
    /// and this returns `NOT_FOUND`.
    pub fn commit_add_receiver(
        &mut self,
        ready: AddReceiverReady,
    ) -> Result<AddReceiverOutcome, Status> {
        let resource_handle = self.commit_added_resource(
            Side::Receiver,
            &ready.name,
            &ready.node_seed,
            &ready.session_handle,
        )?;
        Ok(AddReceiverOutcome {
            resource_handle,
            receiver_id: ready.receiver_id,
            node_seed: ready.node_seed,
        })
    }

    fn commit_added_resource(
        &mut self,
        side: Side,
        name: &str,
        node_seed: &str,
        session_handle: &str,
    ) -> Result<String, Status> {
        if !self.sessions.contains_key(session_handle) {
            tracing::warn!(
                session_handle,
                side = side.label(),
                name = %name,
                %node_seed,
                "AddSender/AddReceiver: session closed while libnvnmos add was \
                 in flight; libnvnmos resource left as stray"
            );
            return Err(Status::not_found(format!(
                "session {session_handle:?} closed before the add completed"
            )));
        }

        let resource_handle = self.allocate_resource_handle();
        self.resources.insert(
            resource_handle.clone(),
            ResourceEntry {
                name: name.to_string(),
                node_seed: node_seed.to_string(),
                session_handle: session_handle.to_string(),
                side,
            },
        );
        self.by_name.insert(
            (node_seed.to_string(), side, name.to_string()),
            resource_handle.clone(),
        );
        self.sessions
            .get_mut(session_handle)
            .expect("checked above")
            .resources
            .insert(resource_handle.clone());
        Ok(resource_handle)
    }

    /// Remove a resource. Only the owning session is allowed to remove
    /// it; cross-session removal returns `NOT_FOUND` to avoid leaking
    /// the existence of other sessions' handles.
    pub fn remove_resource(
        &mut self,
        session_handle: &str,
        resource_handle: &str,
    ) -> Result<(RemoveResourceOutcome, RemoveResourceFfi), Status> {
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
        self.by_name.remove(&(
            resource.node_seed.clone(),
            resource.side,
            resource.name.clone(),
        ));
        self.sessions
            .get_mut(session_handle)
            .expect("checked above")
            .resources
            .remove(resource_handle);

        // Daemon state is consistent at this point. The libnvnmos removal
        // is best-effort and deferred to run outside the lock — a failure
        // there would leak a resource in libnvnmos's IS-04 model, but our
        // state stays clean.
        let server = self
            .nodes
            .get(&resource.node_seed)
            .map(|n| n.server.clone());
        let ffi = RemoveResourceFfi {
            server,
            side: resource.side,
            name: resource.name.clone(),
        };
        Ok((
            RemoveResourceOutcome {
                node_seed: resource.node_seed,
                name: resource.name,
                side: resource.side,
            },
            ffi,
        ))
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
    ) -> Result<(SyncResourceStateOutcome, SyncResourceFfi), Status> {
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
        let server = self
            .nodes
            .get(&resource.node_seed)
            .ok_or_else(|| {
                Status::internal(format!(
                    "resource {resource_handle:?} references seed {:?} but no \
                     Node entry exists",
                    resource.node_seed,
                ))
            })?
            .server
            .clone();

        let outcome = SyncResourceStateOutcome {
            node_seed: resource.node_seed.clone(),
            name: resource.name.clone(),
            side: resource.side,
            activated: transport_file.is_some(),
        };
        let ffi = SyncResourceFfi {
            server,
            side: resource.side,
            name: resource.name.clone(),
            transport_file: transport_file.map(str::to_string),
        };
        Ok((outcome, ffi))
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

    /// True when `session_handle` has a live activation subscription
    /// (IS-05 and/or IS-08).
    pub fn has_any_activation_subscription(&self, session_handle: &str) -> bool {
        self.has_active_subscription(session_handle)
            || self.has_active_channelmapping_subscription(session_handle)
    }

    /// True when `session_handle` has a `SubscribeActivations` slot whose
    /// receiver is still connected.
    pub fn has_active_subscription(&self, session_handle: &str) -> bool {
        self.subscriptions
            .get(session_handle)
            .is_some_and(|s| !s.tx.is_closed())
    }

    /// Remove a subscription entry whose receiver has been dropped.
    pub fn reap_closed_subscription(&mut self, session_handle: &str) {
        if self
            .subscriptions
            .get(session_handle)
            .is_some_and(|s| s.tx.is_closed())
        {
            self.subscriptions.remove(session_handle);
        }
    }

    /// Remove a channel-mapping subscription entry whose receiver has been dropped.
    pub fn reap_closed_channelmapping_subscription(&mut self, session_handle: &str) {
        if self
            .channelmapping_subscriptions
            .get(session_handle)
            .is_some_and(|s| s.tx.is_closed())
        {
            self.channelmapping_subscriptions.remove(session_handle);
        }
    }

    /// Called when a `SubscribeActivations` server stream is dropped.
    /// Returns `true` when the session still exists and has no live
    /// subscription (caller should arm the resubscribe watchdog).
    pub fn on_subscription_stream_ended(&mut self, session_handle: &str) -> bool {
        self.reap_closed_subscription(session_handle);
        self.sessions.contains_key(session_handle)
            && !self.has_any_activation_subscription(session_handle)
    }

    /// Whether `session_handle` is still open.
    pub fn sessions_contains(&self, session_handle: &str) -> bool {
        self.sessions.contains_key(session_handle)
    }

    /// Activation router — called synchronously from a libnvnmos worker
    /// thread via the activation trampoline installed at NodeServer
    /// creation. Looks up the resource by `(node_seed, side, name)`,
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
        side: Side,
        name: &str,
        transport_file: Option<&str>,
    ) -> ActivationDispatch {
        let key = (node_seed.to_string(), side, name.to_string());
        let resource_handle = match self.by_name.get(&key) {
            Some(h) => h.clone(),
            None => return ActivationDispatch::NoResource,
        };
        let resource = match self.resources.get(&resource_handle) {
            Some(r) => r,
            None => {
                // by_name is supposed to be in lockstep with
                // resources; surface the inconsistency in the log path
                // and NACK rather than panic.
                tracing::error!(
                    %node_seed,
                    side = side.label(),
                    name,
                    %resource_handle,
                    "dispatch_activation: by_name pointed at a \
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
            side: side.to_proto() as i32,
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

    /// Resolve `requested` (`0` = allocate from `port_range`) to an
    /// available TCP port. Skips ports already assigned to another Node
    /// and runs a bind-only host probe before Node creation.
    fn resolve_http_port(&self, requested: u16, port_range: &PortRange) -> Result<u16, Status> {
        if requested != 0 {
            if let Some(owner) = self.http_ports.get(&requested) {
                return Err(Status::already_exists(format!(
                    "http_port {requested} is already assigned to node_seed {owner:?}"
                )));
            }
            if !http_port::is_tcp_port_bindable(requested) {
                return Err(Status::failed_precondition(format!(
                    "http_port {requested} is not available on this host"
                )));
            }
            return Ok(requested);
        }

        for port in port_range.iter() {
            if self.http_ports.contains_key(&port) {
                continue;
            }
            if http_port::is_tcp_port_bindable(port) {
                return Ok(port);
            }
        }
        Err(Status::resource_exhausted(format!(
            "no available http_port in daemon range {port_range}"
        )))
    }

    #[cfg(test)]
    pub(super) fn resolve_http_port_for_test(
        &self,
        requested: u16,
        port_range: &PortRange,
    ) -> Result<u16, Status> {
        self.resolve_http_port(requested, port_range)
    }

    #[cfg(test)]
    pub(super) fn assign_http_port_for_test(&mut self, port: u16, seed: &str) {
        self.http_ports.insert(port, seed.to_string());
    }

    fn allocate_channelmapping_handle(&self) -> String {
        let n = self.next_channelmapping_id.fetch_add(1, Ordering::Relaxed);
        format!("cm-{n}")
    }

    fn allocate_channelmapping_activation_handle(&self) -> String {
        let n = self
            .next_channelmapping_activation_id
            .fetch_add(1, Ordering::Relaxed);
        format!("cm-act-{n}")
    }

    pub fn has_active_channelmapping_subscription(&self, session_handle: &str) -> bool {
        self.channelmapping_subscriptions
            .get(session_handle)
            .is_some_and(|s| !s.tx.is_closed())
    }

    pub fn subscribe_channelmapping_activations(
        &mut self,
        session_handle: &str,
        tx: tokio_mpsc::Sender<Result<ChannelMappingActivationEvent, Status>>,
    ) -> Result<(), Status> {
        if !self.sessions.contains_key(session_handle) {
            return Err(Status::not_found(format!(
                "session_handle {session_handle:?} is not known to this daemon"
            )));
        }
        if let Some(existing) = self.channelmapping_subscriptions.get(session_handle) {
            if !existing.tx.is_closed() {
                return Err(Status::already_exists(format!(
                    "session {session_handle:?} already has an active \
                     SubscribeChannelMappingActivations stream"
                )));
            }
        }
        self.channelmapping_subscriptions.insert(
            session_handle.to_string(),
            ChannelMappingActivationSubscriber { tx },
        );
        Ok(())
    }

    pub fn on_channelmapping_subscription_stream_ended(&mut self, session_handle: &str) -> bool {
        if self
            .channelmapping_subscriptions
            .get(session_handle)
            .is_some_and(|s| s.tx.is_closed())
        {
            self.channelmapping_subscriptions.remove(session_handle);
        }
        self.sessions.contains_key(session_handle)
            && !self.has_any_activation_subscription(session_handle)
    }

    /// First phase of `AddChannelMapping`, the IS-08 analogue of
    /// [`State::prepare_add_resource`].
    ///
    /// The validation flow (spanning the phases):
    ///
    /// 1. Pre-check in daemon state — refuses a duplicate channel mapping
    ///    `name` on the same Node before any FFI happens (here).
    /// 2. Assign default IS-08 ids when proto `id` is empty (nvnmosd
    ///    policy); `routable_inputs` is forwarded like libnvnmos C
    ///    (empty → unrestricted caps) (here).
    /// 3. `add_channelmapping` into libnvnmos, which validates geometry,
    ///    duplicate ids, labels, and parent/sender linkage
    ///    ([`AddChannelMappingPrep::run_ffi`], no lock held); then record
    ///    it in daemon state ([`State::commit_add_channelmapping`]). If
    ///    the session closed during FFI, commit returns `NOT_FOUND` and
    ///    the libnvnmos mapping is left as a stray (no compensating
    ///    remove; torn down when the Node is destroyed).
    pub fn prepare_add_channelmapping(
        &mut self,
        session_handle: &str,
        name: &str,
        inputs: &[ProtoChannelMappingInput],
        outputs: &[ProtoChannelMappingOutput],
    ) -> Result<AddChannelMappingPrep, Status> {
        if name.is_empty() {
            return Err(Status::invalid_argument("name must be non-empty"));
        }

        self.reap_closed_channelmapping_subscription(session_handle);
        if !self.has_active_channelmapping_subscription(session_handle) {
            return Err(Status::failed_precondition(
                "SubscribeChannelMappingActivations required before AddChannelMapping",
            ));
        }

        let session = self.sessions.get(session_handle).ok_or_else(|| {
            Status::not_found(format!(
                "session_handle {session_handle:?} is not known to this daemon"
            ))
        })?;
        let node_seed = session.node_seed.clone();
        if self
            .channelmappings_by_name
            .contains_key(&(node_seed.clone(), name.to_string()))
        {
            return Err(Status::already_exists(format!(
                "channel mapping name {name:?} already exists on node_seed \
                 {node_seed:?}"
            )));
        }

        let first_on_node = !self
            .channelmappings
            .values()
            .any(|entry| entry.node_seed == node_seed);

        let effective_input_ids = effective_channelmapping_input_ids(name, inputs, first_on_node);
        let effective_output_ids =
            effective_channelmapping_output_ids(name, outputs, first_on_node);
        let mapping =
            build_channelmapping(inputs, outputs, &effective_input_ids, &effective_output_ids);

        let server = self
            .nodes
            .get(&node_seed)
            .ok_or_else(|| {
                Status::internal(format!(
                    "session {session_handle:?} referenced seed {node_seed:?} but no Node entry \
                     exists"
                ))
            })?
            .server
            .clone();

        Ok(AddChannelMappingPrep {
            server,
            name: name.to_string(),
            mapping,
            node_seed,
            session_handle: session_handle.to_string(),
            effective_input_ids,
            effective_output_ids,
        })
    }

    /// Third phase of `AddChannelMapping`: record the channel mapping in
    /// daemon state after the libnvnmos add succeeded.
    ///
    /// The session must still exist; if it was closed while the FFI ran
    /// (a concurrent `CloseSession`), the libnvnmos mapping is left as a
    /// stray (no compensating remove; torn down when the Node is destroyed)
    /// and this returns `NOT_FOUND`.
    pub fn commit_add_channelmapping(
        &mut self,
        ready: AddChannelMappingReady,
    ) -> Result<AddChannelMappingOutcome, Status> {
        let AddChannelMappingReady {
            name,
            node_seed,
            session_handle,
            effective_input_ids,
            effective_output_ids,
        } = ready;

        if !self.sessions.contains_key(&session_handle) {
            tracing::warn!(
                session_handle,
                name = %name,
                %node_seed,
                "AddChannelMapping: session closed while libnvnmos add was in \
                 flight; libnvnmos channel mapping left as stray"
            );
            return Err(Status::not_found(format!(
                "session {session_handle:?} closed before the add completed"
            )));
        }

        let channelmapping_handle = self.allocate_channelmapping_handle();
        let output_id_set: HashSet<String> = effective_output_ids.iter().cloned().collect();
        for id in &effective_output_ids {
            self.outputs_by_id.insert(
                (node_seed.clone(), id.clone()),
                channelmapping_handle.clone(),
            );
        }

        self.channelmappings.insert(
            channelmapping_handle.clone(),
            ChannelMappingEntry {
                name: name.clone(),
                node_seed: node_seed.clone(),
                session_handle: session_handle.clone(),
                output_ids: output_id_set,
            },
        );
        self.channelmappings_by_name
            .insert((node_seed.clone(), name), channelmapping_handle.clone());
        self.sessions
            .get_mut(&session_handle)
            .expect("checked above")
            .channelmappings
            .insert(channelmapping_handle.clone());

        Ok(AddChannelMappingOutcome {
            channelmapping_handle,
            input_ids: effective_input_ids,
            output_ids: effective_output_ids,
            node_seed,
        })
    }

    pub fn remove_channelmapping(
        &mut self,
        session_handle: &str,
        channelmapping_handle: &str,
    ) -> Result<(RemoveChannelMappingOutcome, RemoveChannelMappingFfi), Status> {
        let session = self.sessions.get(session_handle).ok_or_else(|| {
            Status::not_found(format!(
                "session_handle {session_handle:?} is not known to this daemon"
            ))
        })?;
        if !session.channelmappings.contains(channelmapping_handle) {
            return Err(Status::not_found(format!(
                "session {session_handle:?} does not own channelmapping_handle \
                 {channelmapping_handle:?}"
            )));
        }

        let entry = self
            .channelmappings
            .remove(channelmapping_handle)
            .ok_or_else(|| {
                Status::internal(format!(
                    "session {session_handle:?} owns channelmapping_handle \
                     {channelmapping_handle:?} but no entry exists"
                ))
            })?;
        self.channelmappings_by_name
            .remove(&(entry.node_seed.clone(), entry.name.clone()));
        for output_id in &entry.output_ids {
            self.outputs_by_id
                .remove(&(entry.node_seed.clone(), output_id.clone()));
        }
        self.sessions
            .get_mut(session_handle)
            .expect("checked above")
            .channelmappings
            .remove(channelmapping_handle);

        let server = self.nodes.get(&entry.node_seed).map(|n| n.server.clone());
        let ffi = RemoveChannelMappingFfi {
            server,
            name: entry.name.clone(),
        };
        Ok((
            RemoveChannelMappingOutcome {
                node_seed: entry.node_seed,
                name: entry.name,
            },
            ffi,
        ))
    }

    pub fn sync_channelmapping_state(
        &mut self,
        session_handle: &str,
        channelmapping_handle: &str,
        output_id: &str,
        active_map: &[ProtoActiveMapEntry],
    ) -> Result<(SyncChannelMappingStateOutcome, SyncChannelMappingFfi), Status> {
        let session = self.sessions.get(session_handle).ok_or_else(|| {
            Status::not_found(format!(
                "session_handle {session_handle:?} is not known to this daemon"
            ))
        })?;
        if !session.channelmappings.contains(channelmapping_handle) {
            return Err(Status::not_found(format!(
                "session {session_handle:?} does not own channelmapping_handle \
                 {channelmapping_handle:?}"
            )));
        }

        let entry = self
            .channelmappings
            .get(channelmapping_handle)
            .ok_or_else(|| {
                Status::internal(format!(
                    "session {session_handle:?} owns channelmapping_handle \
                 {channelmapping_handle:?} but no entry exists"
                ))
            })?;
        let server = self
            .nodes
            .get(&entry.node_seed)
            .ok_or_else(|| {
                Status::internal(format!(
                    "channelmapping {channelmapping_handle:?} references seed {:?} but no Node \
                     entry exists",
                    entry.node_seed,
                ))
            })?
            .server
            .clone();

        let outcome = SyncChannelMappingStateOutcome {
            node_seed: entry.node_seed.clone(),
            name: entry.name.clone(),
        };
        let ffi = SyncChannelMappingFfi {
            server,
            name: entry.name.clone(),
            output_id: output_id.to_string(),
            active_map: active_map_from_proto(active_map),
        };
        Ok((outcome, ffi))
    }

    pub fn dispatch_channelmapping_activation(
        &mut self,
        node_seed: &str,
        output_id: &str,
        active_map: Vec<ProtoActiveMapEntry>,
    ) -> ChannelMappingActivationDispatch {
        let channelmapping_handle = match self
            .outputs_by_id
            .get(&(node_seed.to_string(), output_id.to_string()))
        {
            Some(h) => h.clone(),
            None => return ChannelMappingActivationDispatch::NoChannelMapping,
        };
        let entry = match self.channelmappings.get(&channelmapping_handle) {
            Some(e) => e,
            None => return ChannelMappingActivationDispatch::NoChannelMapping,
        };
        let session_handle = entry.session_handle.clone();

        if self
            .channelmapping_subscriptions
            .get(&session_handle)
            .is_some_and(|s| s.tx.is_closed())
        {
            self.channelmapping_subscriptions.remove(&session_handle);
        }

        let tx = match self.channelmapping_subscriptions.get(&session_handle) {
            Some(s) => s.tx.clone(),
            None => return ChannelMappingActivationDispatch::NoSubscriber,
        };

        let activation_handle = self.allocate_channelmapping_activation_handle();
        let (ack_tx, ack_rx) = std_mpsc::sync_channel::<AckOutcome>(1);
        let event = ChannelMappingActivationEvent {
            channelmapping_handle,
            activation_handle: activation_handle.clone(),
            output_id: output_id.to_string(),
            active_map,
        };

        match tx.try_send(Ok(event)) {
            Ok(()) => {}
            Err(tokio_mpsc::error::TrySendError::Full(_)) => {
                return ChannelMappingActivationDispatch::SubscriberBusy;
            }
            Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
                self.channelmapping_subscriptions.remove(&session_handle);
                return ChannelMappingActivationDispatch::NoSubscriber;
            }
        }

        self.pending_channelmapping_activations.insert(
            activation_handle.clone(),
            PendingChannelMappingActivation {
                session_handle,
                ack_tx,
            },
        );

        ChannelMappingActivationDispatch::Routed {
            activation_handle,
            ack_rx,
        }
    }

    pub fn complete_channelmapping_activation(
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
            .pending_channelmapping_activations
            .get(activation_handle)
            .is_some_and(|p| p.session_handle == session_handle);
        if !owns {
            return Err(Status::not_found(format!(
                "activation_handle {activation_handle:?} is not pending on \
                 session {session_handle:?}"
            )));
        }
        let pending = self
            .pending_channelmapping_activations
            .remove(activation_handle)
            .expect("checked above");

        if pending.ack_tx.send(outcome).is_err() {
            tracing::warn!(
                activation_handle,
                session_handle,
                "AckChannelMappingActivation: activation router already gave up; \
                 ack discarded",
            );
        }
        Ok(())
    }

    pub fn cleanup_pending_channelmapping_activation(&mut self, activation_handle: &str) {
        self.pending_channelmapping_activations
            .remove(activation_handle);
    }
}

fn require_non_empty_seed(seed: &str) -> Result<(), Status> {
    if seed.is_empty() {
        Err(Status::invalid_argument("node_seed must be non-empty"))
    } else {
        Ok(())
    }
}

fn default_channelmapping_input_id(name: &str, index: usize, first_on_node: bool) -> String {
    if first_on_node {
        format!("input{index}")
    } else {
        format!("{name}_input{index}")
    }
}

fn default_channelmapping_output_id(name: &str, index: usize, first_on_node: bool) -> String {
    if first_on_node {
        format!("output{index}")
    } else {
        format!("{name}_output{index}")
    }
}

fn effective_channelmapping_input_ids(
    name: &str,
    inputs: &[ProtoChannelMappingInput],
    first_on_node: bool,
) -> Vec<String> {
    inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            if input.id.is_empty() {
                default_channelmapping_input_id(name, index, first_on_node)
            } else {
                input.id.clone()
            }
        })
        .collect()
}

fn effective_channelmapping_output_ids(
    name: &str,
    outputs: &[ProtoChannelMappingOutput],
    first_on_node: bool,
) -> Vec<String> {
    outputs
        .iter()
        .enumerate()
        .map(|(index, output)| {
            if output.id.is_empty() {
                default_channelmapping_output_id(name, index, first_on_node)
            } else {
                output.id.clone()
            }
        })
        .collect()
}

fn channelmapping_parent_type(raw: i32) -> ChannelMappingParentType {
    if raw == ProtoChannelMappingParentType::Source as i32 {
        ChannelMappingParentType::Source
    } else {
        ChannelMappingParentType::Receiver
    }
}

fn build_channelmapping(
    inputs: &[ProtoChannelMappingInput],
    outputs: &[ProtoChannelMappingOutput],
    effective_input_ids: &[String],
    effective_output_ids: &[String],
) -> ChannelMappingConfig {
    let mapped_inputs = inputs
        .iter()
        .zip(effective_input_ids.iter())
        .map(|(input, id)| ChannelMappingInput {
            id: id.clone(),
            name: input.name.clone(),
            description: input.description.clone(),
            channel_labels: input.channel_labels.clone(),
            parent_name: input.parent_name.clone(),
            parent_type: channelmapping_parent_type(input.parent_type),
            reordering: input.reordering,
            block_size: input.block_size,
        })
        .collect();

    let mapped_outputs = outputs
        .iter()
        .zip(effective_output_ids.iter())
        .map(|(output, id)| {
            let routable_inputs = if output.routable_inputs.is_empty() {
                None
            } else {
                Some(output.routable_inputs.clone())
            };
            ChannelMappingOutput {
                id: id.clone(),
                name: output.name.clone(),
                description: output.description.clone(),
                channel_labels: output.channel_labels.clone(),
                sender_name: output.sender_name.clone(),
                routable_inputs,
            }
        })
        .collect();

    ChannelMappingConfig {
        inputs: mapped_inputs,
        outputs: mapped_outputs,
    }
}

pub fn active_map_from_proto(entries: &[ProtoActiveMapEntry]) -> Vec<ChannelMappingActiveMapEntry> {
    entries
        .iter()
        .map(|entry| ChannelMappingActiveMapEntry {
            input_id: entry.input_id.clone(),
            input_channel: entry.input_channel,
        })
        .collect()
}

pub fn active_map_to_proto(entries: &[ChannelMappingActiveMapEntry]) -> Vec<ProtoActiveMapEntry> {
    entries
        .iter()
        .map(|entry| ProtoActiveMapEntry {
            input_id: entry.input_id.clone(),
            input_channel: entry.input_channel,
        })
        .collect()
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
/// `node_config.seed` is the canonical Node identifier; the RPC
/// handler reads it back off the returned [`NodeConfig`] and passes it
/// to [`State::add_node`] / [`State::open_session`] as the lookup
/// key, ensuring the daemon's lookup key and libnvnmos's UUID
/// derivation key always agree.
pub fn translate_config(proto: Option<&ProtoNodeConfig>) -> Result<NodeConfig, Status> {
    let proto = proto.cloned().unwrap_or_default();
    let http_port = u16::try_from(proto.http_port).map_err(|_| {
        Status::invalid_argument(format!(
            "node_config.http_port {} is not a valid TCP port (max 65535)",
            proto.http_port,
        ))
    })?;
    let asset_tags = translate_asset_tags(proto.asset_tags.as_ref())?;
    let network_services = translate_network_services(proto.network_services.as_ref())?;
    Ok(NodeConfig {
        seed: proto.seed,
        host_name: proto.host_name,
        host_addresses: proto.host_addresses,
        http_port,
        label: proto.label,
        description: proto.description,
        asset_tags,
        network_services,
        log_level: log_bridge::LIBNVNMOS_LOG_LEVEL,
    })
}

/// Translate a proto [`ProtoAssetConfig`] into the wrapper's
/// [`AssetConfig`].
///
/// An entirely-default submessage (every string empty, no functions) is
/// indistinguishable from "not set" on the wire and is treated as
/// absent. A *partially* filled submessage is rejected with
/// `INVALID_ARGUMENT`: libnvnmos requires all four fields when asset
/// tags are present at all, so failing here gives the client a clearer
/// error than letting the wrapper trip over an empty string later.
fn translate_asset_tags(proto: Option<&ProtoAssetConfig>) -> Result<Option<AssetConfig>, Status> {
    let Some(proto) = proto else { return Ok(None) };
    let all_empty = proto.manufacturer.is_empty()
        && proto.product.is_empty()
        && proto.instance_id.is_empty()
        && proto.functions.is_empty();
    if all_empty {
        return Ok(None);
    }
    if proto.manufacturer.is_empty() {
        return Err(Status::invalid_argument(
            "node_config.asset_tags.manufacturer must be non-empty when asset_tags is set",
        ));
    }
    if proto.product.is_empty() {
        return Err(Status::invalid_argument(
            "node_config.asset_tags.product must be non-empty when asset_tags is set",
        ));
    }
    if proto.instance_id.is_empty() {
        return Err(Status::invalid_argument(
            "node_config.asset_tags.instance_id must be non-empty when asset_tags is set",
        ));
    }
    if proto.functions.is_empty() {
        return Err(Status::invalid_argument(
            "node_config.asset_tags.functions must contain at least one entry when \
             asset_tags is set",
        ));
    }
    for (i, f) in proto.functions.iter().enumerate() {
        if f.is_empty() {
            return Err(Status::invalid_argument(format!(
                "node_config.asset_tags.functions[{i}] must be non-empty",
            )));
        }
    }
    Ok(Some(AssetConfig {
        manufacturer: proto.manufacturer.clone(),
        product: proto.product.clone(),
        instance_id: proto.instance_id.clone(),
        functions: proto.functions.clone(),
    }))
}

/// Translate a proto [`ProtoNetworkServicesConfig`] into the wrapper's
/// [`NetworkServicesConfig`].
///
/// Unlike `asset_tags`, every inner field is genuinely optional —
/// libnvnmos accepts any combination, with each "unset" field falling
/// back to its own default. Only the port-range validation is enforced
/// here; an entirely-default submessage is treated as absent.
fn translate_network_services(
    proto: Option<&ProtoNetworkServicesConfig>,
) -> Result<Option<NetworkServicesConfig>, Status> {
    let Some(proto) = proto else { return Ok(None) };
    let all_default = proto.domain.is_empty()
        && proto.registration_address.is_empty()
        && proto.registration_port == 0
        && proto.registration_version.is_empty()
        && proto.system_address.is_empty()
        && proto.system_port == 0
        && proto.system_version.is_empty();
    if all_default {
        return Ok(None);
    }
    let registration_port = u16::try_from(proto.registration_port).map_err(|_| {
        Status::invalid_argument(format!(
            "node_config.network_services.registration_port {} is not a valid TCP port \
             (max 65535)",
            proto.registration_port,
        ))
    })?;
    let system_port = u16::try_from(proto.system_port).map_err(|_| {
        Status::invalid_argument(format!(
            "node_config.network_services.system_port {} is not a valid TCP port \
             (max 65535)",
            proto.system_port,
        ))
    })?;
    Ok(Some(NetworkServicesConfig {
        domain: proto.domain.clone(),
        registration_address: proto.registration_address.clone(),
        registration_port,
        registration_version: proto.registration_version.clone(),
        system_address: proto.system_address.clone(),
        system_port,
        system_version: proto.system_version.clone(),
    }))
}

#[cfg(test)]
mod http_port_allocation_tests {
    use super::State;
    use crate::http_port::PortRange;

    #[test]
    fn explicit_port_rejects_conflict_with_assigned_port() {
        let mut state = State::new();
        let range = PortRange::new(18_080, 18_099).expect("range");
        state.assign_http_port_for_test(18_010, "other-seed");
        let err = state
            .resolve_http_port_for_test(18_010, &range)
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
    }
}
