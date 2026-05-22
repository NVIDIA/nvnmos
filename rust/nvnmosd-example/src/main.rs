// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `nvnmosd-example` — minimal regression client for `nvnmosd`.
//!
//! Modelled on the C `nvnmos-example` in `src/main.c`. This commit
//! exercises the session-refcounted Node lifetime end-to-end:
//!
//! 1. `OpenSession` on `--node-seed` — creates the Node, refcount 0→1.
//! 2. `OpenSession` on the same seed — attaches to the existing Node,
//!    refcount 1→2; the returned `node_id` must match (1).
//! 3. `CloseSession` of the first handle — refcount 2→1, Node alive.
//! 4. `CloseSession` of the second handle — refcount 1→0, Node destroyed.
//!
//! Subsequent commits will grow this binary into a full regression
//! harness mirroring the C example's interactive flow (register
//! senders/receivers, observe activations, deactivate, tear down).
//!
//! See `doc/designs/nvnmosd/README.md` for the full design and the
//! current rollout plan.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use hyper_util::rt::TokioIo;
use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{CloseSessionRequest, NodeConfig, OpenSessionRequest, OpenSessionResponse};
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
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

    // (1) First OpenSession: creates the Node.
    let a = open(&mut client, &args.node_seed, "first session (creates Node)").await?;

    // (2) Second OpenSession on the same seed: attaches to the existing
    // Node. `node_config` is intentionally omitted because the daemon
    // ignores it when the Node already exists.
    let b = open(&mut client, &args.node_seed, "second session (refcount bump)").await?;

    anyhow::ensure!(
        a.node_id == b.node_id,
        "OpenSession reused the Node but returned a different node_id: {} vs {}",
        a.node_id,
        b.node_id,
    );
    anyhow::ensure!(
        a.session_handle != b.session_handle,
        "two sessions ended up with the same session_handle: {}",
        a.session_handle,
    );

    // (3) Close the first session: refcount 2→1, Node remains.
    close(&mut client, &a.session_handle, "first close (refcount to 1)").await?;

    // (4) Close the second session: refcount 1→0, Node destroyed.
    close(&mut client, &b.session_handle, "second close (Node destroyed)").await?;

    tracing::info!("done");
    Ok(())
}

async fn open(
    client: &mut NvnmosDaemonClient<Channel>,
    node_seed: &str,
    label: &str,
) -> anyhow::Result<OpenSessionResponse> {
    tracing::info!(node_seed = %node_seed, "OpenSession ({label})");
    let resp = client
        .open_session(OpenSessionRequest {
            node_seed: node_seed.to_string(),
            node_config: Some(NodeConfig {
                seed: node_seed.to_string(),
                ..Default::default()
            }),
        })
        .await
        .with_context(|| format!("OpenSession ({label}) failed"))?
        .into_inner();
    tracing::info!(
        session_handle = %resp.session_handle,
        node_id = %resp.node_id,
        "session open ({label})",
    );
    Ok(resp)
}

async fn close(
    client: &mut NvnmosDaemonClient<Channel>,
    session_handle: &str,
    label: &str,
) -> anyhow::Result<()> {
    tracing::info!(session_handle = %session_handle, "CloseSession ({label})");
    client
        .close_session(CloseSessionRequest {
            session_handle: session_handle.to_string(),
        })
        .await
        .with_context(|| format!("CloseSession ({label}) failed"))?;
    Ok(())
}

async fn connect_uds(uds: &Path) -> anyhow::Result<Channel> {
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
