// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nvnmosd` — the NMOS daemon.
//!
//! This binary listens on a UDS socket and serves the `NvnmosDaemon` gRPC
//! service. `OpenSession` and `CloseSession` are implemented end-to-end
//! so we can verify the gRPC plumbing round-trips; the remaining RPCs
//! return [`tonic::Code::Unimplemented`] until later commits land the
//! NvNmos integration and the activation pump.
//!
//! See `doc/designs/nvnmosd/README.md` for the full design.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use clap::Parser;
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

#[derive(Parser, Debug)]
#[command(version, about = "NMOS daemon (nvnmosd)")]
struct Args {
    /// Path to the UDS socket to listen on. A pre-existing file at this
    /// path is removed before binding.
    #[arg(long, env = "NVNMOSD_UDS", default_value = "/tmp/nvnmosd.sock")]
    uds: PathBuf,
}

#[derive(Default)]
struct Daemon {
    /// Monotonically-increasing session handle source. Replaced with
    /// proper session state (NvNmos node-server backing, refcounting,
    /// per-session activation queues) in the next commit.
    next_session_handle: AtomicU64,
}

impl Daemon {
    fn allocate_session_handle(&self) -> String {
        let n = self.next_session_handle.fetch_add(1, Ordering::Relaxed);
        format!("sess-{n}")
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
        let session_handle = self.allocate_session_handle();
        tracing::info!(
            node_seed = %req.node_seed,
            session_handle = %session_handle,
            "OpenSession",
        );
        Ok(Response::new(OpenSessionResponse {
            session_handle,
            node_id: String::new(),
        }))
    }

    async fn close_session(
        &self,
        request: Request<CloseSessionRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        tracing::info!(session_handle = %req.session_handle, "CloseSession");
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

    let daemon = Daemon::default();

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
