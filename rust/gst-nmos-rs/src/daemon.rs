// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! gRPC client glue for the `nvnmosd` daemon.
//!
//! [`Session`] wraps the per-element gRPC state: a `tonic` channel, the
//! daemon's session handle, an optional registered resource, and a
//! background task subscribed to [`SubscribeActivations`]. Constructed
//! at NULL→READY and torn down at READY→NULL by the element's
//! `change_state` override.
//!
//! When [`Session::open`] is called with a non-empty `transport_file`
//! it also drives `AddSender` / `AddReceiver` (selected by `side`) so
//! the resource is published in IS-04 immediately. With no
//! `transport_file` the session is opened but no resource is
//! registered — a future change will build the transport file from
//! upstream caps and the element's properties.
//!
//! The activation task acks every `ActivationEvent` with `success=true`
//! today. The inner data path isn't wired yet so the plugin can't
//! actually apply an activation; once that lands the ack will be
//! conditional on the local apply succeeding.
//!
//! [`SubscribeActivations`]: nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient::subscribe_activations

use std::path::PathBuf;
use std::time::Duration;

use hyper_util::rt::TokioIo;
use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AckActivationRequest, AddReceiverRequest, AddSenderRequest, CloseSessionRequest, NodeConfig,
    OpenSessionRequest, Side as ProtoSide, SubscribeActivationsRequest, Transport as ProtoTransport,
};
use thiserror::Error;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use gstreamer as gst;

use crate::CAT;
use crate::runtime::SHARED_RUNTIME;
use crate::session::Side;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A live session against `nvnmosd`.
///
/// Open with [`Session::open`]; tear down with [`Session::close`]. Drop
/// silently aborts the activation task but does **not** call
/// `CloseSession` — prefer the explicit close path. `CloseSession`
/// implicitly removes any resource the session added, so no explicit
/// `RemoveResource` is needed in the close path.
pub(crate) struct Session {
    pub(crate) session_handle: String,
    pub(crate) node_id: String,
    pub(crate) created_node: bool,
    /// `Some((resource_handle, resource_id))` when `Session::open` was
    /// called with a non-empty `transport_file` and the daemon
    /// accepted the `AddSender` / `AddReceiver`. `None` otherwise.
    resource: Option<RegisteredResource>,
    client: NvnmosDaemonClient<Channel>,
    activation_task: JoinHandle<()>,
}

struct RegisteredResource {
    handle: String,
    id: String,
}

#[derive(Debug, Error)]
pub(crate) enum DaemonError {
    #[error("unsupported daemon-uri scheme: {0}; only `unix:` URIs are supported")]
    UnsupportedScheme(String),
    #[error("transport error connecting to nvnmosd: {0}")]
    Transport(#[from] Box<tonic::transport::Error>),
    #[error("RPC error: {0}")]
    Rpc(#[from] Box<tonic::Status>),
}

impl From<tonic::transport::Error> for DaemonError {
    fn from(e: tonic::transport::Error) -> Self {
        DaemonError::Transport(Box::new(e))
    }
}

impl From<tonic::Status> for DaemonError {
    fn from(s: tonic::Status) -> Self {
        DaemonError::Rpc(Box::new(s))
    }
}

impl Session {
    /// Open a session against the daemon at `daemon_uri` for Node
    /// `node_seed`, subscribe to activations, and (when
    /// `transport_file` is `Some`) register `name` as a Sender or
    /// Receiver via `AddSender` / `AddReceiver`.
    ///
    /// Only `unix:/path/to/sock` URIs are supported; the `node_seed`
    /// is the only field set on `NodeConfig` (label, description,
    /// asset_tags, network_services are left at their proto-default
    /// and ignored by the daemon when attaching to an existing Node).
    ///
    /// If the resource registration fails the partially-open session
    /// is rolled back via `CloseSession` so the daemon doesn't leak
    /// state.
    pub(crate) async fn open(
        daemon_uri: &str,
        node_seed: &str,
        side: Side,
        name: &str,
        transport: ProtoTransport,
        transport_file: Option<&str>,
    ) -> Result<Self, DaemonError> {
        let uds_path = parse_unix_uri(daemon_uri)?;
        let channel = connect_uds(uds_path).await?;
        let mut client = NvnmosDaemonClient::new(channel.clone());

        let resp = client
            .open_session(OpenSessionRequest {
                node_config: Some(NodeConfig {
                    seed: node_seed.to_owned(),
                    ..NodeConfig::default()
                }),
            })
            .await?
            .into_inner();

        let session_handle = resp.session_handle.clone();
        let node_id = resp.node_id.clone();
        let created_node = resp.created_node;

        let activation_task = spawn_activation_task(client.clone(), session_handle.clone());

        let resource = match transport_file {
            Some(file) => match add_resource(
                &mut client,
                &session_handle,
                side,
                name,
                transport,
                file,
            )
            .await
            {
                Ok(r) => Some(r),
                Err(e) => {
                    activation_task.abort();
                    let _ = activation_task.await;
                    let _ = client
                        .close_session(CloseSessionRequest {
                            session_handle: session_handle.clone(),
                        })
                        .await;
                    return Err(e);
                }
            },
            None => None,
        };

        Ok(Self {
            session_handle,
            node_id,
            created_node,
            resource,
            client,
            activation_task,
        })
    }

    pub(crate) fn resource_id(&self) -> Option<(&str, &str)> {
        self.resource
            .as_ref()
            .map(|r| (r.handle.as_str(), r.id.as_str()))
    }

    /// Cancel the background activation task and tell the daemon to
    /// close this session. The daemon removes any resource the
    /// session contributed as part of `CloseSession`, so no explicit
    /// `RemoveResource` is sent here. Errors are returned so callers
    /// can log them; the session is consumed either way.
    pub(crate) async fn close(self) -> Result<(), DaemonError> {
        let Session {
            session_handle,
            mut client,
            activation_task,
            ..
        } = self;

        activation_task.abort();
        let _ = activation_task.await;

        client
            .close_session(CloseSessionRequest { session_handle })
            .await?;

        Ok(())
    }
}

async fn add_resource(
    client: &mut NvnmosDaemonClient<Channel>,
    session_handle: &str,
    side: Side,
    name: &str,
    transport: ProtoTransport,
    transport_file: &str,
) -> Result<RegisteredResource, DaemonError> {
    let resp = match side {
        Side::Sender => client
            .add_sender(AddSenderRequest {
                session_handle: session_handle.to_owned(),
                name: name.to_owned(),
                transport: transport as i32,
                transport_file: transport_file.to_owned(),
            })
            .await?
            .into_inner(),
        Side::Receiver => client
            .add_receiver(AddReceiverRequest {
                session_handle: session_handle.to_owned(),
                name: name.to_owned(),
                transport: transport as i32,
                transport_file: transport_file.to_owned(),
            })
            .await?
            .into_inner(),
    };
    Ok(RegisteredResource {
        handle: resp.resource_handle,
        id: resp.resource_id,
    })
}

fn parse_unix_uri(daemon_uri: &str) -> Result<PathBuf, DaemonError> {
    if let Some(path) = daemon_uri.strip_prefix("unix:") {
        Ok(PathBuf::from(path))
    } else {
        let scheme = daemon_uri
            .split(':')
            .next()
            .unwrap_or(daemon_uri)
            .to_owned();
        Err(DaemonError::UnsupportedScheme(scheme))
    }
}

async fn connect_uds(uds_path: PathBuf) -> Result<Channel, tonic::transport::Error> {
    // tonic requires a Uri to drive HTTP/2 authority/scheme; the UDS
    // connector ignores it.
    let endpoint = Endpoint::try_from("http://[::1]:50051")?.connect_timeout(CONNECT_TIMEOUT);
    endpoint
        .connect_with_connector(service_fn(move |_: Uri| {
            let uds_path = uds_path.clone();
            async move {
                let stream = UnixStream::connect(uds_path).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await
}

fn spawn_activation_task(
    mut client: NvnmosDaemonClient<Channel>,
    session_handle: String,
) -> JoinHandle<()> {
    SHARED_RUNTIME.spawn(async move {
        let sub = client
            .subscribe_activations(SubscribeActivationsRequest {
                session_handle: session_handle.clone(),
            })
            .await;

        let mut stream = match sub {
            Ok(s) => s.into_inner(),
            Err(status) => {
                gst::warning!(
                    CAT,
                    "SubscribeActivations failed for session {session_handle}: {status}"
                );
                return;
            }
        };

        loop {
            match stream.message().await {
                Ok(Some(ev)) => {
                    let deactivating = ev.transport_file.is_none();
                    let side = ProtoSide::try_from(ev.side)
                        .map(|s| s.as_str_name())
                        .unwrap_or("UNKNOWN");
                    gst::info!(
                        CAT,
                        "ActivationEvent received (session={session_handle}, \
                         resource_handle={}, activation_handle={}, \
                         side={}, deactivating={}); auto-acking",
                        ev.resource_handle,
                        ev.activation_handle,
                        side,
                        deactivating,
                    );

                    let ack = client
                        .ack_activation(AckActivationRequest {
                            session_handle: session_handle.clone(),
                            activation_handle: ev.activation_handle.clone(),
                            success: true,
                            failure_reason: String::new(),
                        })
                        .await;
                    if let Err(status) = ack {
                        gst::warning!(
                            CAT,
                            "AckActivation failed for session {session_handle} \
                             (activation_handle={}): {status}",
                            ev.activation_handle,
                        );
                    }
                }
                Ok(None) => {
                    gst::debug!(
                        CAT,
                        "activation stream closed by daemon for session {session_handle}",
                    );
                    break;
                }
                Err(status) => {
                    // tonic emits Cancelled when the channel is dropped
                    // by abort(); treat that as a clean exit.
                    if status.code() == tonic::Code::Cancelled {
                        gst::debug!(
                            CAT,
                            "activation stream cancelled for session {session_handle}",
                        );
                    } else {
                        gst::warning!(
                            CAT,
                            "activation stream error for session {session_handle}: {status}"
                        );
                    }
                    break;
                }
            }
        }
    })
}
