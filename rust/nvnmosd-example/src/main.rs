// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nvnmosd-example` — minimal regression client for `nvnmosd`.
//!
//! Modelled on the C `nvnmos-example` in `src/main.c`. This commit
//! exercises `OpenSession` + `CloseSession` end-to-end; subsequent
//! commits grow this binary into a regression harness that mirrors the
//! C example's interactive flow (register senders/receivers, observe
//! activations, deactivate, tear down).
//!
//! See `doc/designs/nvnmosd/README.md` for the full design and the
//! current rollout plan.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use hyper_util::rt::TokioIo;
use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{NodeConfig, OpenSessionRequest, SessionId};
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;

#[derive(Parser, Debug)]
#[command(version, about = "nvnmosd example/regression client")]
struct Args {
    /// Path to the UDS socket where nvnmosd is listening.
    #[arg(long, env = "NVNMOSD_UDS", default_value = "/tmp/nvnmosd.sock")]
    uds: PathBuf,

    /// NvNmos node seed to attach to. Created on demand if no other
    /// session is currently attached.
    #[arg(long, default_value = "nvnmosd-example")]
    node_seed: String,
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

    let channel = connect_uds(&args.uds)
        .await
        .with_context(|| format!("connecting to nvnmosd at {}", args.uds.display()))?;
    let mut client = NvnmosDaemonClient::new(channel);

    tracing::info!(node_seed = %args.node_seed, "OpenSession");
    let resp = client
        .open_session(OpenSessionRequest {
            node_seed: args.node_seed.clone(),
            node_config: Some(NodeConfig {
                seed: args.node_seed.clone(),
                ..Default::default()
            }),
            persistent: false,
        })
        .await
        .context("OpenSession failed")?
        .into_inner();

    tracing::info!(session_id = %resp.session_id, node_uuid = %resp.node_uuid, "session open");

    tracing::info!(session_id = %resp.session_id, "CloseSession");
    client
        .close_session(SessionId {
            id: resp.session_id.clone(),
        })
        .await
        .context("CloseSession failed")?;

    tracing::info!("done");
    Ok(())
}

async fn connect_uds(uds: &Path) -> anyhow::Result<tonic::transport::Channel> {
    let uds = uds.to_path_buf();
    // The HTTP URI is a placeholder — tonic insists on a Uri to set
    // authority/scheme on outgoing requests, but the custom connector
    // ignores it and dials the UDS path instead.
    let endpoint = Endpoint::try_from("http://[::1]:50051")?;
    let channel = endpoint
        .connect_with_connector(service_fn(move |_: Uri| {
            let uds = uds.clone();
            async move {
                let stream = UnixStream::connect(uds).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await?;
    Ok(channel)
}
