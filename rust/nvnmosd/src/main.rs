// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nvnmosd` — the NMOS daemon.
//!
//! This binary listens on a UDS socket and serves the `NvnmosDaemon` gRPC
//! service. `OpenSession` / `CloseSession` now drive real
//! [`nvnmos::NodeServer`]s with session refcounting; the remaining RPCs
//! still return [`tonic::Code::Unimplemented`] until later commits land
//! the resource and activation plumbing.
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
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::Parser;
use nvnmos::NodeServer;
use nvnmos_rpc::v1::nvnmos_daemon_server::{NvnmosDaemon, NvnmosDaemonServer};
use nvnmos_rpc::v1::{
    AckActivationRequest, ActivationEvent, AddNodeRequest, AddNodeResponse, AddReceiverRequest,
    AddResourceResponse, AddSenderRequest, CloseSessionRequest, Empty, OpenSessionRequest,
    OpenSessionResponse, RemoveNodeRequest, RemoveResourceRequest, SubscribeActivationsRequest,
    SyncResourceStateRequest,
};
use tokio::net::UnixListener;
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::{Request, Response, Status};

use crate::state::State;

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
        _request: Request<AddNodeRequest>,
    ) -> Result<Response<AddNodeResponse>, Status> {
        Err(unimplemented_rpc("AddNode"))
    }

    async fn remove_node(
        &self,
        _request: Request<RemoveNodeRequest>,
    ) -> Result<Response<Empty>, Status> {
        Err(unimplemented_rpc("RemoveNode"))
    }

    async fn open_session(
        &self,
        request: Request<OpenSessionRequest>,
    ) -> Result<Response<OpenSessionResponse>, Status> {
        let req = request.into_inner();

        // Translate the proto config outside the state lock — it can fail
        // (bad port), and there's no reason to hold the lock for it.
        let config = state::translate_config(req.node_config.as_ref(), &req.node_seed)?;

        // Hold the state lock only over the registry update (and the
        // libnvnmos create call inside it, which blocks on mDNS / bind /
        // worker spawn). Acceptable while the daemon is single-client;
        // revisit when multi-client throughput matters.
        let outcome = {
            let mut state = self.lock_state();
            state.open_session(&req.node_seed, || {
                NodeServer::builder(&config)
                    .on_log(log_bridge::forward)
                    .build()
                    .map_err(|e| {
                        Status::internal(format!("create_nmos_node_server failed: {e}"))
                    })
            })?
        };

        tracing::info!(
            node_seed = %req.node_seed,
            session_handle = %outcome.session_handle,
            node_id = %outcome.node_id,
            created_node = outcome.created_node,
            "OpenSession",
        );
        Ok(Response::new(OpenSessionResponse {
            session_handle: outcome.session_handle,
            node_id: outcome.node_id,
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
            remaining_refcount = outcome.remaining_refcount,
            node_destroyed = outcome.remaining_refcount == 0,
            "CloseSession",
        );
        Ok(Response::new(Empty {}))
    }

    async fn add_sender(
        &self,
        _request: Request<AddSenderRequest>,
    ) -> Result<Response<AddResourceResponse>, Status> {
        Err(unimplemented_rpc("AddSender"))
    }

    async fn add_receiver(
        &self,
        _request: Request<AddReceiverRequest>,
    ) -> Result<Response<AddResourceResponse>, Status> {
        Err(unimplemented_rpc("AddReceiver"))
    }

    async fn remove_resource(
        &self,
        _request: Request<RemoveResourceRequest>,
    ) -> Result<Response<Empty>, Status> {
        Err(unimplemented_rpc("RemoveResource"))
    }

    type SubscribeActivationsStream = ReceiverStream<Result<ActivationEvent, Status>>;

    async fn subscribe_activations(
        &self,
        _request: Request<SubscribeActivationsRequest>,
    ) -> Result<Response<Self::SubscribeActivationsStream>, Status> {
        Err(unimplemented_rpc("SubscribeActivations"))
    }

    async fn ack_activation(
        &self,
        _request: Request<AckActivationRequest>,
    ) -> Result<Response<Empty>, Status> {
        Err(unimplemented_rpc("AckActivation"))
    }

    async fn sync_resource_state(
        &self,
        _request: Request<SyncResourceStateRequest>,
    ) -> Result<Response<Empty>, Status> {
        Err(unimplemented_rpc("SyncResourceState"))
    }
}

fn unimplemented_rpc(rpc: &str) -> Status {
    Status::unimplemented(format!("{rpc}: not implemented yet"))
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
