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
//! Each `ActivationEvent` arriving on the subscription is routed to
//! the element-supplied [`ActivationHandler`] (see [`Session::open`]).
//! The handler returns an [`ActivationOutcome`] via a `oneshot` —
//! `Applied` becomes `AckActivation { success=true }`, `Failed` becomes
//! `AckActivation { success=false, failure_reason }`. The next event
//! is read only after the current one's ack lands, so the IS-05
//! controller and the local data path stay in lock-step.
//!
//! [`SubscribeActivations`]: nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient::subscribe_activations

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hyper_util::rt::TokioIo;
use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AckActivationRequest, AddReceiverRequest, AddSenderRequest, CloseSessionRequest, NodeConfig,
    OpenSessionRequest, Side as ProtoSide, SubscribeActivationsRequest, SyncResourceStateRequest,
    Transport as ProtoTransport,
};
use thiserror::Error;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use gstreamer as gst;

use crate::CAT;
use crate::runtime::SHARED_RUNTIME;
use crate::session::Side;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-event input to an [`ActivationHandler`]. Mirrors
/// `nvnmos.daemon.v1.ActivationEvent` with the `side` decoded into
/// the crate-local enum and the proto `optional string transport_file`
/// already flattened.
#[derive(Debug, Clone)]
pub(crate) struct ActivationRequest {
    pub(crate) activation_handle: String,
    pub(crate) resource_handle: String,
    pub(crate) side: Side,
    /// `Some(text)` for an activation, `None` for a deactivation.
    /// For MXL receivers the daemon synthesises this by splicing the
    /// PATCHed `mxl_domain_id` / `mxl_flow_id` into the resource's
    /// internal `flow_def`; the element just consumes the result.
    pub(crate) transport_file: Option<String>,
}

/// What the handler tells the activation task to ack back to the
/// daemon. `Applied` translates to `AckActivation { success=true }`,
/// `Failed { reason }` to `AckActivation { success=false, failure_reason=reason }`.
#[derive(Debug)]
pub(crate) enum ActivationOutcome {
    Applied,
    Failed { reason: String },
}

/// Element-side callback invoked once per [`ActivationEvent`]. The
/// activation task creates a fresh oneshot per event, hands the
/// sender to the handler, and awaits the receiver before acking.
///
/// Implementations must not block the calling task: dispatch the
/// real work onto the GStreamer thread via
/// `gst::glib::object::Cast::call_async` (see `nmossink::imp` /
/// `nmossrc::imp`) and arrange for the eventual outcome to land on
/// the supplied sender.
pub(crate) type ActivationHandler =
    Arc<dyn Fn(ActivationRequest, oneshot::Sender<ActivationOutcome>) + Send + Sync>;

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
    #[error(
        "session already has a resource registered; deferred registration is a one-shot operation"
    )]
    AlreadyRegistered,
    #[error(
        "session has no resource registered yet; auto-activate sync cannot run before AddSender / AddReceiver"
    )]
    NoResource,
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
    /// `activation_handler` is invoked for every `ActivationEvent`
    /// the daemon delivers on this session. See
    /// [`ActivationHandler`].
    ///
    /// If the resource registration fails the partially-open session
    /// is rolled back via `CloseSession` so the daemon doesn't leak
    /// state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open(
        daemon_uri: &str,
        node_seed: &str,
        http_port: u16,
        side: Side,
        name: &str,
        transport: ProtoTransport,
        transport_file: Option<&str>,
        activation_handler: ActivationHandler,
    ) -> Result<Self, DaemonError> {
        let uds_path = parse_unix_uri(daemon_uri)?;
        let channel = connect_uds(uds_path).await?;
        let mut client = NvnmosDaemonClient::new(channel.clone());

        let resp = client
            .open_session(OpenSessionRequest {
                node_config: Some(NodeConfig {
                    seed: node_seed.to_owned(),
                    http_port: u32::from(http_port),
                    ..NodeConfig::default()
                }),
            })
            .await?
            .into_inner();

        let session_handle = resp.session_handle.clone();
        let node_id = resp.node_id.clone();
        let created_node = resp.created_node;

        let activation_task =
            spawn_activation_task(client.clone(), session_handle.clone(), activation_handler);

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

    /// Register a Sender or Receiver on this open session. Used by
    /// the deferred-mode path: at NULL→READY the session is opened
    /// with no resource (because neither `transport-file*` nor `caps`
    /// was supplied), and the actual `AddSender` / `AddReceiver` is
    /// driven later from inside `change_state(ReadyToPaused)` once
    /// upstream peer caps have negotiated and a flow_def can be
    /// synthesised.
    ///
    /// Errors with [`DaemonError::AlreadyRegistered`] if called on a
    /// session that already has a resource (caller bug —
    /// deferred-mode registration is one-shot).
    pub(crate) async fn add_resource(
        &mut self,
        side: Side,
        name: &str,
        transport: ProtoTransport,
        transport_file: &str,
    ) -> Result<(), DaemonError> {
        if self.resource.is_some() {
            return Err(DaemonError::AlreadyRegistered);
        }
        let resource = add_resource(
            &mut self.client,
            &self.session_handle,
            side,
            name,
            transport,
            transport_file,
        )
        .await?;
        self.resource = Some(resource);
        Ok(())
    }

    /// Tell the daemon to update its IS-04/IS-05 view of this
    /// session's resource without going through the IS-05 activation
    /// stream. Used by the `auto-activate=true` path: the element
    /// has already brought its inner `mxlsink` / `mxlsrc` up directly
    /// from the configured / resolved transport file, so the daemon's
    /// `/single/{senders,receivers}/{id}/active` endpoint needs to be
    /// brought into sync (`master_enable: true`) without first having
    /// to be PATCHed by an external IS-05 controller.
    ///
    /// `transport_file: Some(_)` means "(re)activate with this
    /// transport file"; `transport_file: None` means "deactivate".
    /// This is the same wire shape as `SubscribeActivations`'s
    /// `ActivationEvent` carries, but the daemon does *not* fire a
    /// callback back to subscribers (the element initiating the sync
    /// already knows; other subscribers learn via IS-04 / IS-05
    /// state).
    ///
    /// Errors when called on a session with no registered resource
    /// (caller bug — `auto-activate` paths only call this after
    /// `add_resource` succeeded).
    pub(crate) async fn sync_resource_state(
        &mut self,
        transport_file: Option<&str>,
    ) -> Result<(), DaemonError> {
        let resource_handle = self
            .resource
            .as_ref()
            .map(|r| r.handle.clone())
            .ok_or(DaemonError::NoResource)?;
        self.client
            .sync_resource_state(SyncResourceStateRequest {
                session_handle: self.session_handle.clone(),
                resource_handle,
                transport_file: transport_file.map(str::to_owned),
            })
            .await?;
        Ok(())
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
    handler: ActivationHandler,
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
                    let side_name = ProtoSide::try_from(ev.side)
                        .map(|s| s.as_str_name())
                        .unwrap_or("UNKNOWN");
                    gst::info!(
                        CAT,
                        "ActivationEvent received (session={session_handle}, \
                         resource_handle={}, activation_handle={}, \
                         side={}, deactivating={}); dispatching to element",
                        ev.resource_handle,
                        ev.activation_handle,
                        side_name,
                        deactivating,
                    );

                    let outcome = match Side::try_from_proto(ev.side) {
                        None => ActivationOutcome::Failed {
                            reason: format!(
                                "daemon delivered ActivationEvent with unrecognised `side` enum value {}",
                                ev.side,
                            ),
                        },
                        Some(side) => {
                            let req = ActivationRequest {
                                activation_handle: ev.activation_handle.clone(),
                                resource_handle: ev.resource_handle.clone(),
                                side,
                                transport_file: ev.transport_file.clone(),
                            };
                            let (tx, rx) = oneshot::channel();
                            handler(req, tx);
                            match rx.await {
                                Ok(o) => o,
                                Err(_) => ActivationOutcome::Failed {
                                    reason: "element dropped its activation oneshot before \
                                             completing the apply"
                                        .to_owned(),
                                },
                            }
                        }
                    };

                    let (success, failure_reason) = match outcome {
                        ActivationOutcome::Applied => (true, String::new()),
                        ActivationOutcome::Failed { reason } => {
                            gst::warning!(
                                CAT,
                                "activation apply failed (session={session_handle}, \
                                 activation_handle={}): {reason}",
                                ev.activation_handle,
                            );
                            (false, reason)
                        }
                    };

                    let ack = client
                        .ack_activation(AckActivationRequest {
                            session_handle: session_handle.clone(),
                            activation_handle: ev.activation_handle.clone(),
                            success,
                            failure_reason,
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
