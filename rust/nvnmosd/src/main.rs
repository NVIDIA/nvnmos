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
//! See `doc/designs/nvnmosd/README.md` for the full design.

// `tonic::Status` is intentionally large (it carries gRPC metadata) so every
// `Result<T, Status>` trips `result_large_err`. The alternative is to box
// `Status` everywhere, which penalises the happy path; tonic-using crates
// uniformly allow the lint at the crate root instead.
#![allow(clippy::result_large_err)]

mod log_bridge;
mod state;

use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::Parser;
use nvnmos::{Activation, NodeServer};
use nvnmos_rpc::v1::nvnmos_daemon_server::{NvnmosDaemon, NvnmosDaemonServer};
use nvnmos_rpc::v1::{
    AckActivationRequest, ActivationEvent, AddNodeRequest, AddNodeResponse, AddReceiverRequest,
    AddResourceResponse, AddSenderRequest, CloseSessionRequest, Empty, OpenSessionRequest,
    OpenSessionResponse, RemoveNodeRequest, RemoveResourceRequest, SubscribeActivationsRequest,
    SyncResourceStateRequest, Transport as ProtoTransport,
};
use tokio::net::UnixListener;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::{Request, Response, Status};

use crate::state::{AckOutcome, ActivationDispatch, State};

/// Bound on the per-session activations stream. Small because activations
/// are rare (one per IS-05 PATCH) and the consumer is expected to ack
/// each one promptly; a backed-up channel almost always means the client
/// stopped reading, in which case NACKing further activations is the
/// right behaviour.
const SUBSCRIPTION_BUFFER: usize = 16;

#[derive(Parser, Debug)]
#[command(version, about = "NMOS daemon (nvnmosd)")]
struct Args {
    /// Path to the UDS socket to listen on. A pre-existing file at this
    /// path is removed before binding.
    #[arg(long, env = "NVNMOSD_UDS", default_value = "/tmp/nvnmosd.sock")]
    uds: PathBuf,
}

struct Daemon {
    state: Arc<Mutex<State>>,
}

impl Daemon {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::new())),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        // Daemon state is held by a single Mutex; poisoning would mean a
        // panic on another RPC. Surface that as a panic here too — there's
        // no useful recovery and silently continuing risks compounding the
        // inconsistency that triggered the original panic.
        self.state.lock().expect("daemon state mutex poisoned")
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
        let state_for_callback = self.state.clone();
        let seed_for_callback = seed.clone();
        let outcome = {
            let mut state = self.lock_state();
            state.add_node(&seed, || {
                build_node_server(&config, state_for_callback, seed_for_callback)
            })?
        };
        tracing::info!(
            node_seed = %seed,
            node_id = %outcome.node_id,
            "AddNode",
        );
        Ok(Response::new(AddNodeResponse {
            node_id: outcome.node_id,
        }))
    }

    async fn remove_node(
        &self,
        request: Request<RemoveNodeRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        let outcome = {
            let mut state = self.lock_state();
            state.remove_node(&req.node_seed)?
        };
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

        // Hold the state lock only over the registry update (and the
        // libnvnmos create call inside it, which blocks on mDNS / bind /
        // worker spawn). Acceptable while the daemon is single-client;
        // revisit when multi-client throughput matters.
        let state_for_callback = self.state.clone();
        let seed_for_callback = seed.clone();
        let outcome = {
            let mut state = self.lock_state();
            state.open_session(&seed, || {
                build_node_server(&config, state_for_callback, seed_for_callback)
            })?
        };

        tracing::info!(
            node_seed = %seed,
            session_handle = %outcome.session_handle,
            node_id = %outcome.node_id,
            lifetime = outcome.lifetime.label(),
            created_node = outcome.created_node,
            "OpenSession",
        );
        Ok(Response::new(OpenSessionResponse {
            session_handle: outcome.session_handle,
            node_id: outcome.node_id,
            created_node: outcome.created_node,
        }))
    }

    async fn close_session(
        &self,
        request: Request<CloseSessionRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        let outcome = {
            let mut state = self.lock_state();
            state.close_session(&req.session_handle)?
        };
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
    ) -> Result<Response<AddResourceResponse>, Status> {
        let req = request.into_inner();
        let transport = state::translate_transport(decode_proto_transport(req.transport)?)?;
        let outcome = {
            let mut state = self.lock_state();
            state.add_sender(
                &req.session_handle,
                transport,
                &req.transport_file,
                &req.internal_id,
            )?
        };
        tracing::info!(
            session_handle = %req.session_handle,
            node_seed = %outcome.node_seed,
            resource_handle = %outcome.resource_handle,
            resource_id = %outcome.resource_id,
            internal_id = %req.internal_id,
            kind = outcome.kind.label(),
            "AddSender",
        );
        Ok(Response::new(AddResourceResponse {
            resource_handle: outcome.resource_handle,
            resource_id: outcome.resource_id,
        }))
    }

    async fn add_receiver(
        &self,
        request: Request<AddReceiverRequest>,
    ) -> Result<Response<AddResourceResponse>, Status> {
        let req = request.into_inner();
        let transport = state::translate_transport(decode_proto_transport(req.transport)?)?;
        let outcome = {
            let mut state = self.lock_state();
            state.add_receiver(
                &req.session_handle,
                transport,
                &req.transport_file,
                &req.internal_id,
            )?
        };
        tracing::info!(
            session_handle = %req.session_handle,
            node_seed = %outcome.node_seed,
            resource_handle = %outcome.resource_handle,
            resource_id = %outcome.resource_id,
            internal_id = %req.internal_id,
            kind = outcome.kind.label(),
            "AddReceiver",
        );
        Ok(Response::new(AddResourceResponse {
            resource_handle: outcome.resource_handle,
            resource_id: outcome.resource_id,
        }))
    }

    async fn remove_resource(
        &self,
        request: Request<RemoveResourceRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        let outcome = {
            let mut state = self.lock_state();
            state.remove_resource(&req.session_handle, &req.resource_handle)?
        };
        tracing::info!(
            session_handle = %req.session_handle,
            resource_handle = %req.resource_handle,
            node_seed = %outcome.node_seed,
            internal_id = %outcome.internal_id,
            kind = outcome.kind.label(),
            "RemoveResource",
        );
        Ok(Response::new(Empty {}))
    }

    type SubscribeActivationsStream = ReceiverStream<Result<ActivationEvent, Status>>;

    async fn subscribe_activations(
        &self,
        request: Request<SubscribeActivationsRequest>,
    ) -> Result<Response<Self::SubscribeActivationsStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio_mpsc::channel(SUBSCRIPTION_BUFFER);
        {
            let mut state = self.lock_state();
            state.subscribe_activations(&req.session_handle, tx)?;
        }
        tracing::info!(
            session_handle = %req.session_handle,
            "SubscribeActivations",
        );
        Ok(Response::new(ReceiverStream::new(rx)))
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
        let outcome = {
            let mut state = self.lock_state();
            state.sync_resource_state(
                &req.session_handle,
                &req.resource_handle,
                req.transport_file.as_deref(),
            )?
        };
        tracing::info!(
            session_handle = %req.session_handle,
            resource_handle = %req.resource_handle,
            node_seed = %outcome.node_seed,
            internal_id = %outcome.internal_id,
            kind = outcome.kind.label(),
            activated = outcome.activated,
            "SyncResourceState",
        );
        Ok(Response::new(Empty {}))
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

/// Construct the daemon's standard [`NodeServer`]: wraps the wrapper's
/// builder with the daemon's log bridge and the activation router so
/// that libnvnmos's IS-05 callbacks are bridged into the right session's
/// `SubscribeActivations` stream.
///
/// Takes the `Arc<Mutex<State>>` and `node_seed` by value because both
/// have to be captured by the `'static` activation closure. The caller
/// is expected to `clone()` from the daemon's own state and to forward
/// the request's `node_seed`.
fn build_node_server(
    config: &nvnmos::NodeConfig,
    state: Arc<Mutex<State>>,
    node_seed: String,
) -> Result<NodeServer, Status> {
    NodeServer::builder(config)
        .on_log(log_bridge::forward)
        .on_activation(move |act| route_activation(&state, &node_seed, act))
        .build()
        .map_err(|e| Status::internal(format!("create_nmos_node_server failed: {e}")))
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
    let dispatch = {
        let mut s = state.lock().expect("daemon state mutex poisoned");
        s.dispatch_activation(node_seed, act.internal_id, act.transport_file)
    };
    let (activation_handle, ack_rx) = match dispatch {
        ActivationDispatch::Routed {
            activation_handle,
            ack_rx,
        } => (activation_handle, ack_rx),
        ActivationDispatch::NoResource => {
            tracing::warn!(
                node_seed,
                internal_id = act.internal_id,
                activated = act.transport_file.is_some(),
                "activation for unknown resource (likely a stray from a \
                 prior internal_id mismatch); NACKing",
            );
            return Err("resource not registered with daemon".to_string());
        }
        ActivationDispatch::NoSubscriber => {
            tracing::warn!(
                node_seed,
                internal_id = act.internal_id,
                activated = act.transport_file.is_some(),
                "activation for resource whose owning session has no \
                 SubscribeActivations stream; NACKing",
            );
            return Err("no SubscribeActivations stream on owning session".to_string());
        }
        ActivationDispatch::SubscriberBusy => {
            tracing::warn!(
                node_seed,
                internal_id = act.internal_id,
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
            tracing::warn!(
                activation_handle,
                "activation ack timed out; NACKing",
            );
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    if args.uds.exists() {
        std::fs::remove_file(&args.uds)
            .with_context(|| format!("removing stale UDS socket at {}", args.uds.display()))?;
    }
    if let Some(parent) = args.uds.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory for {}", args.uds.display()))?;
    }

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
