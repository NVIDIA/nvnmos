// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nvnmosd` — the NMOS daemon.
//!
//! This binary listens on a UDS socket and serves the `NvnmosDaemon` gRPC
//! service. Node lifecycle (`OpenSession` / `CloseSession`, `AddNode` /
//! `RemoveNode`), resource lifecycle (`AddSender` / `AddReceiver` /
//! `RemoveResource`), out-of-band state sync (`SyncResourceState`), and
//! the IS-05 activation callback path (`SubscribeActivations` /
//! `AckActivation`) all drive real [`nvnmos::NodeServer`]s with
//! session-based ownership.
//!
//! See `rust/nvnmosd/README.md` for operator docs and
//! `doc/designs/nvnmosd/README.md` for the full design.

// `tonic::Status` is intentionally large (it carries gRPC metadata) so every
// `Result<T, Status>` trips `result_large_err`. The alternative is to box
// `Status` everywhere, which penalises the happy path; tonic-using crates
// uniformly allow the lint at the crate root instead.
#![allow(clippy::result_large_err)]

mod env_config;
mod http_port;
mod log_bridge;
mod malloc_trim;
mod session_gc;
mod state;

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use anyhow::Context;
use clap::Parser;
use nvnmos::{Activation, ChannelMappingActivation, Transport};
use nvnmos_rpc::v1::nvnmos_daemon_server::{NvnmosDaemon, NvnmosDaemonServer};
use nvnmos_rpc::v1::{
    AckActivationRequest, AckChannelMappingActivationRequest, ActivationEvent,
    AddChannelMappingRequest, AddChannelMappingResponse, AddNodeRequest, AddNodeResponse,
    AddReceiverRequest, AddReceiverResponse, AddSenderRequest, AddSenderResponse,
    CloseSessionRequest, Empty, OpenSessionRequest, OpenSessionResponse,
    RemoveChannelMappingRequest, RemoveNodeRequest, RemoveResourceRequest,
    SubscribeActivationsRequest, SubscribeChannelMappingActivationsRequest,
    SyncChannelMappingStateRequest, SyncResourceStateRequest, Transport as ProtoTransport,
};
use tokio::net::UnixListener;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::{Request, Response, Status};

use crate::http_port::read_http_port_range;
use crate::session_gc::SessionGc;
use crate::state::{AckOutcome, ActivationDispatch, ChannelMappingActivationDispatch, Side, State};

/// Bound on the per-session activations stream. Small because activations
/// are rare (one per IS-05 PATCH) and the consumer is expected to ack
/// each one promptly; a backed-up channel almost always means the client
/// stopped reading, in which case NACKing further activations is the
/// right behaviour.
const SUBSCRIPTION_BUFFER: usize = 16;

#[derive(Parser, Debug)]
#[command(version, about = "NMOS daemon (nvnmosd)")]
struct Args {
    /// Path to the UDS socket to listen on. Fails at startup if another
    /// listener is already accepting connections on this path; removes
    /// only a stale socket file left behind by a crashed process.
    #[arg(long, env = "NVNMOSD_UDS", default_value = "/tmp/nvnmosd.sock")]
    uds: PathBuf,
}

struct Daemon {
    state: Arc<Mutex<State>>,
    session_gc: SessionGc,
    http_port_range: http_port::PortRange,
}

impl Daemon {
    fn new() -> Self {
        let http_port_range = read_http_port_range();
        tracing::info!(
            http_port_range = %http_port_range,
            "HTTP port allocation range for node_config.http_port=0"
        );
        let state = Arc::new(Mutex::new(State::new()));
        let session_gc = SessionGc::new(state.clone());
        Self {
            state,
            session_gc,
            http_port_range,
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        // Daemon state is held by a single Mutex; poisoning would mean a
        // panic on another RPC. Surface that as a panic here too — there's
        // no useful recovery and silently continuing risks compounding the
        // inconsistency that triggered the original panic.
        self.state.lock().expect("daemon state mutex poisoned")
    }

    /// Second phase of node-creating RPCs (no `state` lock held): wire daemon
    /// activation callbacks into [`CreateNodePrep::run_ffi`]. On failure,
    /// abort the pending node ([`State::abort_pending_node`], a brief
    /// lock). The caller commits via [`State::commit_open_session`] or
    /// [`State::commit_add_node`].
    fn run_ffi(&self, prep: state::CreateNodePrep) -> Result<state::CreateNodeReady, Status> {
        let seed = prep.seed.clone();
        let http_port = prep.http_port;
        prep.run_ffi(
            {
                let state = self.state.clone();
                let seed = seed.clone();
                move |act| route_activation(&state, &seed, act)
            },
            {
                let state = self.state.clone();
                let seed = seed.clone();
                move |act| route_channelmapping_activation(&state, &seed, act)
            },
        )
        .inspect_err(|_| {
            self.lock_state().abort_pending_node(&seed, http_port);
        })
    }

    /// `AddNode` orchestration (persistent-Node analogue of
    /// [`Daemon::add_resource`]): validate daemon bookkeeping under the
    /// lock, build the [`nvnmos::NodeServer`] with no lock held, then
    /// commit under the lock.
    fn add_node(&self, config: nvnmos::NodeConfig) -> Result<state::AddNodeOutcome, Status> {
        let prep = {
            let mut state = self.lock_state();
            state.prepare_add_node(config, &self.http_port_range)?
        };
        let ready = self.run_ffi(prep)?;
        let mut state = self.lock_state();
        Ok(state.commit_add_node(ready))
    }

    /// `OpenSession` orchestration (session-refcounted analogue of
    /// [`Daemon::add_node`]): validate daemon bookkeeping under the lock
    /// and either attach to an existing Node (no FFI) or, for a new Node,
    /// build the [`nvnmos::NodeServer`] with no lock held and commit under
    /// the lock.
    fn open_session(&self, config: nvnmos::NodeConfig) -> Result<state::OpenOutcome, Status> {
        // Bookkeeping under the lock; the blocking libnvnmos create (mDNS
        // / bind / worker spawn) runs afterwards with no lock held.
        let plan = {
            let mut state = self.lock_state();
            state.prepare_open_session(config, &self.http_port_range)?
        };
        match plan {
            state::OpenSessionPlan::Attached(outcome) => Ok(outcome),
            state::OpenSessionPlan::Create(prep) => {
                let ready = self.run_ffi(prep)?;
                Ok(self.lock_state().commit_open_session(ready))
            }
        }
    }

    /// `AddSender` orchestration: validate daemon bookkeeping under the
    /// lock, run the libnvnmos add + id lookups with no lock held, then
    /// commit under the lock.
    fn add_sender(
        &self,
        session_handle: &str,
        transport: Transport,
        transport_file: &str,
        claimed_name: &str,
    ) -> Result<state::AddSenderOutcome, Status> {
        let prep = {
            let mut state = self.lock_state();
            state.prepare_add_resource(
                Side::Sender,
                session_handle,
                transport,
                transport_file,
                claimed_name,
            )?
        };
        match prep.run_ffi()? {
            state::AddResourceReady::Sender(ready) => {
                let mut state = self.lock_state();
                state.commit_add_sender(ready)
            }
            state::AddResourceReady::Receiver(_) => Err(Status::internal(
                "AddSender prep produced a Receiver ready result",
            )),
        }
    }

    /// `AddReceiver` orchestration: validate daemon bookkeeping under the
    /// lock, run the libnvnmos add + id lookup with no lock held, then
    /// commit under the lock.
    fn add_receiver(
        &self,
        session_handle: &str,
        transport: Transport,
        transport_file: &str,
        claimed_name: &str,
    ) -> Result<state::AddReceiverOutcome, Status> {
        let prep = {
            let mut state = self.lock_state();
            state.prepare_add_resource(
                Side::Receiver,
                session_handle,
                transport,
                transport_file,
                claimed_name,
            )?
        };
        match prep.run_ffi()? {
            state::AddResourceReady::Receiver(ready) => {
                let mut state = self.lock_state();
                state.commit_add_receiver(ready)
            }
            state::AddResourceReady::Sender(_) => Err(Status::internal(
                "AddReceiver prep produced a Sender ready result",
            )),
        }
    }

    /// `AddChannelMapping` orchestration (IS-08 analogue of
    /// [`Daemon::add_sender`] / [`Daemon::add_receiver`]): validate daemon
    /// bookkeeping under the lock, run the libnvnmos add with no lock held,
    /// then commit under the lock.
    fn add_channelmapping(
        &self,
        session_handle: &str,
        name: &str,
        inputs: &[nvnmos_rpc::v1::ChannelMappingInput],
        outputs: &[nvnmos_rpc::v1::ChannelMappingOutput],
    ) -> Result<state::AddChannelMappingOutcome, Status> {
        let prep = {
            let mut state = self.lock_state();
            state.prepare_add_channelmapping(session_handle, name, inputs, outputs)?
        };
        let ready = prep.run_ffi()?;
        let mut state = self.lock_state();
        state.commit_add_channelmapping(ready)
    }
}

#[tonic::async_trait]
impl NvnmosDaemon for Daemon {
    async fn add_node(
        &self,
        request: Request<AddNodeRequest>,
    ) -> Result<Response<AddNodeResponse>, Status> {
        let req = request.into_inner();
        let config = state::translate_config(req.node_config.as_ref())?;
        let seed = config.seed.clone();
        let outcome = self.add_node(config)?;
        tracing::info!(
            node_seed = %seed,
            node_id = %outcome.node_id,
            device_id = %outcome.device_id,
            http_port = outcome.http_port,
            "AddNode",
        );
        Ok(Response::new(AddNodeResponse {
            node_id: outcome.node_id,
            http_port: u32::from(outcome.http_port),
            device_id: outcome.device_id,
        }))
    }

    async fn remove_node(
        &self,
        request: Request<RemoveNodeRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        let (outcome, ffi) = {
            let mut state = self.lock_state();
            state.remove_node(&req.node_seed)?
        };
        // Drop the NodeServer (destroy + thread-join) outside the lock.
        ffi.run();
        {
            let state = self.lock_state();
            malloc_trim::maybe_after_remove_node(&state, &req.node_seed);
        }
        tracing::info!(
            node_seed = %req.node_seed,
            node_id = %outcome.node_id,
            "RemoveNode",
        );
        Ok(Response::new(Empty {}))
    }

    async fn open_session(
        &self,
        request: Request<OpenSessionRequest>,
    ) -> Result<Response<OpenSessionResponse>, Status> {
        let req = request.into_inner();

        // Translate the proto config outside the state lock — it can fail
        // (bad port), and there's no reason to hold the lock for it.
        let config = state::translate_config(req.node_config.as_ref())?;
        let seed = config.seed.clone();
        let outcome = self.open_session(config)?;
        self.session_gc
            .start_subscribe_timeout(&outcome.session_handle);

        tracing::info!(
            node_seed = %seed,
            session_handle = %outcome.session_handle,
            node_id = %outcome.node_id,
            http_port = outcome.http_port,
            lifetime = outcome.lifetime.label(),
            created_node = outcome.created_node,
            "OpenSession",
        );
        Ok(Response::new(OpenSessionResponse {
            session_handle: outcome.session_handle,
            node_id: outcome.node_id,
            created_node: outcome.created_node,
            http_port: u32::from(outcome.http_port),
        }))
    }

    async fn close_session(
        &self,
        request: Request<CloseSessionRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        self.session_gc.cancel_timeout(&req.session_handle);
        let (outcome, ffi) = {
            let mut state = self.lock_state();
            state.close_session(&req.session_handle)?
        };
        // Remove the session's libnvnmos resources and (if this was the
        // last session) destroy the NodeServer outside the lock, so a
        // parked activation thread can take the lock and let the
        // thread-joins in destroy return.
        ffi.run();
        {
            let state = self.lock_state();
            malloc_trim::maybe_after_close_session(&state, &outcome);
        }
        tracing::info!(
            session_handle = %req.session_handle,
            node_seed = %outcome.node_seed,
            node_id = %outcome.node_id,
            lifetime = outcome.lifetime.label(),
            remaining_sessions = outcome.remaining_sessions,
            node_destroyed = outcome.node_destroyed,
            "CloseSession",
        );
        Ok(Response::new(Empty {}))
    }

    async fn add_sender(
        &self,
        request: Request<AddSenderRequest>,
    ) -> Result<Response<AddSenderResponse>, Status> {
        let req = request.into_inner();
        let transport = state::translate_transport(decode_proto_transport(req.transport)?)?;
        let outcome = self.add_sender(
            &req.session_handle,
            transport,
            &req.transport_file,
            &req.name,
        )?;
        tracing::info!(
            session_handle = %req.session_handle,
            node_seed = %outcome.node_seed,
            resource_handle = %outcome.resource_handle,
            source_id = %outcome.source_id,
            flow_id = %outcome.flow_id,
            sender_id = %outcome.sender_id,
            name = %req.name,
            "AddSender",
        );
        Ok(Response::new(AddSenderResponse {
            resource_handle: outcome.resource_handle,
            source_id: outcome.source_id,
            flow_id: outcome.flow_id,
            sender_id: outcome.sender_id,
        }))
    }

    async fn add_receiver(
        &self,
        request: Request<AddReceiverRequest>,
    ) -> Result<Response<AddReceiverResponse>, Status> {
        let req = request.into_inner();
        let transport = state::translate_transport(decode_proto_transport(req.transport)?)?;
        let outcome = self.add_receiver(
            &req.session_handle,
            transport,
            &req.transport_file,
            &req.name,
        )?;
        tracing::info!(
            session_handle = %req.session_handle,
            node_seed = %outcome.node_seed,
            resource_handle = %outcome.resource_handle,
            receiver_id = %outcome.receiver_id,
            name = %req.name,
            "AddReceiver",
        );
        Ok(Response::new(AddReceiverResponse {
            resource_handle: outcome.resource_handle,
            receiver_id: outcome.receiver_id,
        }))
    }

    async fn remove_resource(
        &self,
        request: Request<RemoveResourceRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        let (outcome, ffi) = {
            let mut state = self.lock_state();
            state.remove_resource(&req.session_handle, &req.resource_handle)?
        };
        // libnvnmos removal is best-effort and runs outside the lock.
        ffi.run();
        {
            let state = self.lock_state();
            malloc_trim::maybe_after_remove_resource(&state, &outcome.node_seed);
        }
        tracing::info!(
            session_handle = %req.session_handle,
            resource_handle = %req.resource_handle,
            node_seed = %outcome.node_seed,
            name = %outcome.name,
            side = outcome.side.label(),
            "RemoveResource",
        );
        Ok(Response::new(Empty {}))
    }

    type SubscribeActivationsStream = SubscriptionStream;

    async fn subscribe_activations(
        &self,
        request: Request<SubscribeActivationsRequest>,
    ) -> Result<Response<Self::SubscribeActivationsStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio_mpsc::channel(SUBSCRIPTION_BUFFER);
        {
            let mut state = self.lock_state();
            state.subscribe_activations(&req.session_handle, tx)?;
            self.session_gc.cancel_timeout(&req.session_handle);
        }
        tracing::info!(
            session_handle = %req.session_handle,
            "SubscribeActivations",
        );
        let stream = SubscriptionStream {
            inner: ReceiverStream::new(rx),
            session_handle: req.session_handle,
            state: self.state.clone(),
            session_gc: self.session_gc.clone(),
        };
        Ok(Response::new(stream))
    }

    async fn ack_activation(
        &self,
        request: Request<AckActivationRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        {
            let mut state = self.lock_state();
            state.complete_activation(
                &req.session_handle,
                &req.activation_handle,
                AckOutcome {
                    success: req.success,
                    failure_reason: req.failure_reason.clone(),
                },
            )?;
        }
        tracing::info!(
            session_handle = %req.session_handle,
            activation_handle = %req.activation_handle,
            success = req.success,
            "AckActivation",
        );
        Ok(Response::new(Empty {}))
    }

    async fn sync_resource_state(
        &self,
        request: Request<SyncResourceStateRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        let (outcome, ffi) = {
            let mut state = self.lock_state();
            state.sync_resource_state(
                &req.session_handle,
                &req.resource_handle,
                req.transport_file.as_deref(),
            )?
        };
        // The libnvnmos activate/deactivate runs outside the lock; its
        // result is the RPC result.
        ffi.run()?;
        tracing::info!(
            session_handle = %req.session_handle,
            resource_handle = %req.resource_handle,
            node_seed = %outcome.node_seed,
            name = %outcome.name,
            side = outcome.side.label(),
            activated = outcome.activated,
            "SyncResourceState",
        );
        Ok(Response::new(Empty {}))
    }

    async fn add_channel_mapping(
        &self,
        request: Request<AddChannelMappingRequest>,
    ) -> Result<Response<AddChannelMappingResponse>, Status> {
        let req = request.into_inner();
        let outcome =
            self.add_channelmapping(&req.session_handle, &req.name, &req.inputs, &req.outputs)?;
        tracing::info!(
            session_handle = %req.session_handle,
            name = %req.name,
            channelmapping_handle = %outcome.channelmapping_handle,
            node_seed = %outcome.node_seed,
            "AddChannelMapping",
        );
        Ok(Response::new(AddChannelMappingResponse {
            channelmapping_handle: outcome.channelmapping_handle,
            input_ids: outcome.input_ids,
            output_ids: outcome.output_ids,
        }))
    }

    async fn remove_channel_mapping(
        &self,
        request: Request<RemoveChannelMappingRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        let (outcome, ffi) = {
            let mut state = self.lock_state();
            state.remove_channelmapping(&req.session_handle, &req.channelmapping_handle)?
        };
        // libnvnmos removal is best-effort and runs outside the lock.
        ffi.run();
        tracing::info!(
            session_handle = %req.session_handle,
            channelmapping_handle = %req.channelmapping_handle,
            node_seed = %outcome.node_seed,
            name = %outcome.name,
            "RemoveChannelMapping",
        );
        Ok(Response::new(Empty {}))
    }

    async fn sync_channel_mapping_state(
        &self,
        request: Request<SyncChannelMappingStateRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        let (outcome, ffi) = {
            let mut state = self.lock_state();
            state.sync_channelmapping_state(
                &req.session_handle,
                &req.channelmapping_handle,
                &req.output_id,
                &req.active_map,
            )?
        };
        // The libnvnmos IS-08 activate runs outside the lock.
        ffi.run()?;
        tracing::info!(
            session_handle = %req.session_handle,
            channelmapping_handle = %req.channelmapping_handle,
            node_seed = %outcome.node_seed,
            name = %outcome.name,
            "SyncChannelMappingState",
        );
        Ok(Response::new(Empty {}))
    }

    type SubscribeChannelMappingActivationsStream = ChannelMappingSubscriptionStream;

    async fn subscribe_channel_mapping_activations(
        &self,
        request: Request<SubscribeChannelMappingActivationsRequest>,
    ) -> Result<Response<Self::SubscribeChannelMappingActivationsStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio_mpsc::channel(SUBSCRIPTION_BUFFER);
        {
            let mut state = self.lock_state();
            state.subscribe_channelmapping_activations(&req.session_handle, tx)?;
            self.session_gc.cancel_timeout(&req.session_handle);
        }
        tracing::info!(
            session_handle = %req.session_handle,
            "SubscribeChannelMappingActivations",
        );
        Ok(Response::new(ChannelMappingSubscriptionStream {
            inner: ReceiverStream::new(rx),
            session_handle: req.session_handle,
            state: self.state.clone(),
            session_gc: self.session_gc.clone(),
        }))
    }

    async fn ack_channel_mapping_activation(
        &self,
        request: Request<AckChannelMappingActivationRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        {
            let mut state = self.lock_state();
            state.complete_channelmapping_activation(
                &req.session_handle,
                &req.activation_handle,
                AckOutcome {
                    success: req.success,
                    failure_reason: req.failure_reason.clone(),
                },
            )?;
        }
        tracing::info!(
            session_handle = %req.session_handle,
            activation_handle = %req.activation_handle,
            success = req.success,
            "AckChannelMappingActivation",
        );
        Ok(Response::new(Empty {}))
    }
}

/// Server-streaming wrapper that arms the resubscribe watchdog when the
/// client drops the `SubscribeActivations` stream.
struct SubscriptionStream {
    inner: ReceiverStream<Result<ActivationEvent, Status>>,
    session_handle: String,
    state: Arc<Mutex<State>>,
    session_gc: SessionGc,
}

impl Stream for SubscriptionStream {
    type Item = Result<ActivationEvent, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl Drop for SubscriptionStream {
    fn drop(&mut self) {
        let session_handle = self.session_handle.clone();
        let state = self.state.clone();
        let session_gc = self.session_gc.clone();
        tokio::spawn(async move {
            let arm = {
                let mut guard = state.lock().expect("daemon state mutex poisoned");
                guard.on_subscription_stream_ended(&session_handle)
            };
            if arm {
                session_gc.start_resubscribe_timeout(&session_handle);
            }
        });
    }
}

struct ChannelMappingSubscriptionStream {
    inner: ReceiverStream<Result<nvnmos_rpc::v1::ChannelMappingActivationEvent, Status>>,
    session_handle: String,
    state: Arc<Mutex<State>>,
    session_gc: SessionGc,
}

impl Stream for ChannelMappingSubscriptionStream {
    type Item = Result<nvnmos_rpc::v1::ChannelMappingActivationEvent, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl Drop for ChannelMappingSubscriptionStream {
    fn drop(&mut self) {
        let session_handle = self.session_handle.clone();
        let state = self.state.clone();
        let session_gc = self.session_gc.clone();
        tokio::spawn(async move {
            let arm = {
                let mut guard = state.lock().expect("daemon state mutex poisoned");
                guard.on_channelmapping_subscription_stream_ended(&session_handle)
            };
            if arm {
                session_gc.start_resubscribe_timeout(&session_handle);
            }
        });
    }
}

/// Decode a wire-format proto3 `transport` field into the proto's
/// generated [`ProtoTransport`] enum. Out-of-range values (a future client
/// using a transport this daemon doesn't know) become `INVALID_ARGUMENT`
/// rather than panicking inside `Transport::try_from`.
fn decode_proto_transport(raw: i32) -> Result<ProtoTransport, Status> {
    ProtoTransport::try_from(raw).map_err(|_| {
        Status::invalid_argument(format!("unknown Transport value on the wire: {raw}"))
    })
}

/// Bridge a single libnvnmos activation callback into the daemon's
/// pending-activation flow.
///
/// Runs on a libnvnmos worker thread (non-tokio), synchronously: the
/// IS-05 PATCH stays open until this returns. Translates each outcome
/// from [`State::dispatch_activation`] into a NACK string for libnvnmos
/// (and logs the reason); on a successful enqueue, blocks on the
/// per-activation sync channel until the client's `AckActivation`
/// arrives or [`state::ACTIVATION_ACK_TIMEOUT`] elapses.
fn route_activation(
    state: &Arc<Mutex<State>>,
    node_seed: &str,
    act: &Activation<'_>,
) -> std::result::Result<(), String> {
    let side = Side::from_wrapper(act.side);
    let dispatch = {
        let mut s = state.lock().expect("daemon state mutex poisoned");
        s.dispatch_activation(node_seed, side, act.name, act.transport_file)
    };
    let (activation_handle, ack_rx) = match dispatch {
        ActivationDispatch::Routed {
            activation_handle,
            ack_rx,
        } => (activation_handle, ack_rx),
        ActivationDispatch::NoResource => {
            tracing::warn!(
                node_seed,
                side = side.label(),
                name = act.name,
                activated = act.transport_file.is_some(),
                "activation for unknown resource (likely a stray from a \
                 prior name mismatch); NACKing",
            );
            return Err("resource not known to daemon".to_string());
        }
        ActivationDispatch::NoSubscriber => {
            tracing::warn!(
                node_seed,
                side = side.label(),
                name = act.name,
                activated = act.transport_file.is_some(),
                "activation for resource whose owning session has no \
                 SubscribeActivations stream; NACKing",
            );
            return Err("no SubscribeActivations stream on owning session".to_string());
        }
        ActivationDispatch::SubscriberBusy => {
            tracing::warn!(
                node_seed,
                side = side.label(),
                name = act.name,
                activated = act.transport_file.is_some(),
                "subscriber stream buffer full; NACKing",
            );
            return Err("subscriber stream buffer is full".to_string());
        }
    };

    let result = ack_rx.recv_timeout(state::ACTIVATION_ACK_TIMEOUT);

    // Idempotent: ack handler may have already removed it on the
    // happy path.
    state
        .lock()
        .expect("daemon state mutex poisoned")
        .cleanup_pending_activation(&activation_handle);

    match result {
        Ok(outcome) if outcome.success => Ok(()),
        Ok(outcome) => Err(outcome.failure_reason),
        Err(std_mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(activation_handle, "activation ack timed out; NACKing",);
            Err("activation ack timed out".to_string())
        }
        Err(std_mpsc::RecvTimeoutError::Disconnected) => {
            tracing::warn!(
                activation_handle,
                "activation ack channel disconnected (session closed or \
                 ack handler dropped sender); NACKing",
            );
            Err("session closed before ack".to_string())
        }
    }
}

fn route_channelmapping_activation(
    state: &Arc<Mutex<State>>,
    node_seed: &str,
    act: &ChannelMappingActivation<'_>,
) -> std::result::Result<(), String> {
    let proto_map = state::active_map_to_proto(act.active_map);

    let dispatch = {
        let mut s = state.lock().expect("daemon state mutex poisoned");
        s.dispatch_channelmapping_activation(node_seed, act.output_id, proto_map)
    };

    let (activation_handle, ack_rx) = match dispatch {
        ChannelMappingActivationDispatch::Routed {
            activation_handle,
            ack_rx,
        } => (activation_handle, ack_rx),
        ChannelMappingActivationDispatch::NoChannelMapping => {
            tracing::warn!(
                node_seed,
                name = act.name,
                output_id = act.output_id,
                "channelmapping activation for unknown channel mapping/output; NACKing",
            );
            return Err("channel mapping not known to daemon".to_string());
        }
        ChannelMappingActivationDispatch::NoSubscriber => {
            tracing::warn!(
                node_seed,
                name = act.name,
                output_id = act.output_id,
                "channelmapping activation with no SubscribeChannelMappingActivations stream; \
                 NACKing",
            );
            return Err(
                "no SubscribeChannelMappingActivations stream on owning session".to_string(),
            );
        }
        ChannelMappingActivationDispatch::SubscriberBusy => {
            tracing::warn!(
                node_seed,
                name = act.name,
                output_id = act.output_id,
                "channelmapping subscriber stream buffer full; NACKing",
            );
            return Err("subscriber stream buffer is full".to_string());
        }
    };

    let result = ack_rx.recv_timeout(state::ACTIVATION_ACK_TIMEOUT);
    state
        .lock()
        .expect("daemon state mutex poisoned")
        .cleanup_pending_channelmapping_activation(&activation_handle);

    match result {
        Ok(outcome) if outcome.success => Ok(()),
        Ok(outcome) => Err(outcome.failure_reason),
        Err(std_mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                activation_handle,
                "channelmapping activation ack timed out; NACKing",
            );
            Err("activation ack timed out".to_string())
        }
        Err(std_mpsc::RecvTimeoutError::Disconnected) => {
            tracing::warn!(
                activation_handle,
                "channelmapping activation ack channel disconnected; NACKing",
            );
            Err("session closed before ack".to_string())
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    nvnmosd::uds::prepare_listen_path(&args.uds)?;

    let listener = UnixListener::bind(&args.uds)
        .with_context(|| format!("binding UDS socket at {}", args.uds.display()))?;
    let incoming = UnixListenerStream::new(listener);

    let daemon = Daemon::new();

    tracing::info!(uds = %args.uds.display(), "nvnmosd listening");

    tonic::transport::Server::builder()
        .add_service(NvnmosDaemonServer::new(daemon))
        .serve_with_incoming_shutdown(incoming, shutdown_signal())
        .await
        .context("gRPC server terminated with error")?;

    tracing::info!("nvnmosd shutting down");
    let _ = std::fs::remove_file(&args.uds);
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        if let Ok(mut sigterm) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sigterm.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
