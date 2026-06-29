// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! gRPC session glue for IS-08 channel mapping (`nmosaudiochannelmap`).
//!
//! [`ChannelMappingSession`] mirrors [`crate::daemon::Session`] but uses
//! `SubscribeChannelMappingActivations` and the channel-mapping RPCs.

use std::sync::Arc;

use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AckChannelMappingActivationRequest, AddChannelMappingRequest, AddChannelMappingResponse,
    ChannelMappingActivationEvent, CloseSessionRequest, OpenSessionRequest,
    RemoveChannelMappingRequest, SubscribeChannelMappingActivationsRequest,
    SyncChannelMappingStateRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;

use gstreamer as gst;

use crate::daemon::{connect_uds, parse_unix_uri, DaemonError};
use crate::runtime::SHARED_RUNTIME;
use crate::session::channel_mapping::ChannelMappingSettings;

/// Per-output activation delivered on the IS-08 subscription.
#[derive(Debug, Clone)]
pub(crate) struct ChannelMappingActivationRequest {
    pub(crate) channelmapping_handle: String,
    pub(crate) activation_handle: String,
    pub(crate) output_id: String,
    /// Dense active map: index `i` is output channel `i`; empty `input_id` = unrouted.
    pub(crate) active_map: Vec<nvnmos_rpc::v1::ActiveMapEntry>,
}

#[derive(Debug)]
pub(crate) enum ChannelMappingActivationOutcome {
    Applied,
    Failed { reason: String },
}

pub(crate) type ChannelMappingActivationHandler = Arc<
    dyn Fn(ChannelMappingActivationRequest, oneshot::Sender<ChannelMappingActivationOutcome>)
        + Send
        + Sync,
>;

/// Live IS-08 channel-mapping session against `nvnmosd`.
pub(crate) struct ChannelMappingSession {
    pub(crate) session_handle: String,
    pub(crate) node_id: String,
    pub(crate) created_node: bool,
    pub(crate) http_port: u16,
    channelmapping: Option<AddedChannelMapping>,
    client: NvnmosDaemonClient<Channel>,
    activation_task: JoinHandle<()>,
}

struct AddedChannelMapping {
    handle: String,
}

impl ChannelMappingSession {
    /// Open a session and subscribe to channel-mapping activations.
    /// Does not call [`Self::add_channel_mapping`] — the element does
    /// that once pad geometry is known (READY→PAUSED).
    pub(crate) async fn open(
        settings: &ChannelMappingSettings,
        activation_handler: ChannelMappingActivationHandler,
    ) -> Result<Self, DaemonError> {
        let uds_path = parse_unix_uri(&settings.daemon_uri)?;
        let channel = connect_uds(uds_path).await?;
        let mut client = NvnmosDaemonClient::new(channel.clone());

        let resp = client
            .open_session(OpenSessionRequest {
                node_config: Some(settings.node.to_node_config()),
            })
            .await?
            .into_inner();

        let session_handle = resp.session_handle.clone();
        let node_id = resp.node_id.clone();
        let created_node = resp.created_node;
        let http_port = u16::try_from(resp.http_port).unwrap_or(0);

        let mut subscribe_client = client.clone();
        let stream = match subscribe_client
            .subscribe_channel_mapping_activations(SubscribeChannelMappingActivationsRequest {
                session_handle: session_handle.clone(),
            })
            .await
        {
            Ok(resp) => resp.into_inner(),
            Err(e) => {
                let _ = client
                    .close_session(CloseSessionRequest {
                        session_handle: session_handle.clone(),
                    })
                    .await;
                return Err(e.into());
            }
        };

        let activation_task = spawn_channel_mapping_activation_task(
            client.clone(),
            session_handle.clone(),
            stream,
            activation_handler,
        );

        Ok(Self {
            session_handle,
            node_id,
            created_node,
            http_port,
            channelmapping: None,
            client,
            activation_task,
        })
    }

    pub(crate) fn channelmapping_handle(&self) -> Option<&str> {
        self.channelmapping
            .as_ref()
            .map(|cm| cm.handle.as_str())
    }

    pub(crate) async fn add_channel_mapping(
        &mut self,
        request: AddChannelMappingRequest,
    ) -> Result<AddChannelMappingResponse, DaemonError> {
        if self.channelmapping.is_some() {
            return Err(DaemonError::AlreadyAdded);
        }
        let resp = self
            .client
            .add_channel_mapping(request)
            .await?
            .into_inner();
        self.channelmapping = Some(AddedChannelMapping {
            handle: resp.channelmapping_handle.clone(),
        });
        Ok(resp)
    }

    #[allow(dead_code)]
    pub(crate) async fn remove_channel_mapping(&mut self) -> Result<(), DaemonError> {
        let Some(cm) = self.channelmapping.take() else {
            return Ok(());
        };
        self.client
            .remove_channel_mapping(RemoveChannelMappingRequest {
                session_handle: self.session_handle.clone(),
                channelmapping_handle: cm.handle,
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn sync_channel_mapping_state(
        &mut self,
        output_id: &str,
        active_map: Vec<nvnmos_rpc::v1::ActiveMapEntry>,
    ) -> Result<(), DaemonError> {
        let handle = self
            .channelmapping
            .as_ref()
            .map(|cm| cm.handle.clone())
            .ok_or(DaemonError::NoResource)?;
        self.client
            .sync_channel_mapping_state(SyncChannelMappingStateRequest {
                session_handle: self.session_handle.clone(),
                channelmapping_handle: handle,
                output_id: output_id.to_owned(),
                active_map,
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn close(mut self) -> Result<(), DaemonError> {
        let ChannelMappingSession {
            session_handle,
            mut client,
            activation_task,
            ..
        } = self;

        activation_task.abort();
        let _ = activation_task.await;

        if let Some(cm) = self.channelmapping.take() {
            let _ = client
                .remove_channel_mapping(RemoveChannelMappingRequest {
                    session_handle: session_handle.clone(),
                    channelmapping_handle: cm.handle,
                })
                .await;
        }

        client
            .close_session(CloseSessionRequest { session_handle })
            .await?;

        Ok(())
    }
}

fn spawn_channel_mapping_activation_task(
    mut client: NvnmosDaemonClient<Channel>,
    session_handle: String,
    mut stream: tonic::Streaming<ChannelMappingActivationEvent>,
    handler: ChannelMappingActivationHandler,
) -> JoinHandle<()> {
    SHARED_RUNTIME.spawn(async move {
        loop {
            match stream.message().await {
                Ok(Some(ev)) => {
                    gst::info!(
                        crate::CAT,
                        "ChannelMappingActivationEvent (session={session_handle}, \
                         output_id={}, activation_handle={})",
                        ev.output_id,
                        ev.activation_handle,
                    );
                    let req = ChannelMappingActivationRequest {
                        channelmapping_handle: ev.channelmapping_handle.clone(),
                        activation_handle: ev.activation_handle.clone(),
                        output_id: ev.output_id.clone(),
                        active_map: ev.active_map.clone(),
                    };
                    let (tx, rx) = oneshot::channel();
                    handler(req, tx);
                    let outcome = match rx.await {
                        Ok(o) => o,
                        Err(_) => ChannelMappingActivationOutcome::Failed {
                            reason: "element dropped activation oneshot before completing apply"
                                .to_owned(),
                        },
                    };
                    let (success, failure_reason) = match outcome {
                        ChannelMappingActivationOutcome::Applied => (true, String::new()),
                        ChannelMappingActivationOutcome::Failed { reason } => (false, reason),
                    };
                    if let Err(status) = client
                        .ack_channel_mapping_activation(AckChannelMappingActivationRequest {
                            session_handle: session_handle.clone(),
                            activation_handle: ev.activation_handle.clone(),
                            success,
                            failure_reason,
                        })
                        .await
                    {
                        gst::warning!(
                            crate::CAT,
                            "AckChannelMappingActivation failed (session={session_handle}): {status}",
                        );
                    }
                }
                Ok(None) => break,
                Err(status) if status.code() == tonic::Code::Cancelled => break,
                Err(status) => {
                    gst::warning!(
                        crate::CAT,
                        "channel mapping activation stream error (session={session_handle}): {status}",
                    );
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod session_integration {
    use super::*;
    use crate::channel_mapping::request::build_add_channel_mapping_request;
    use crate::channel_mapping::types::{SinkPadSnapshot, SrcPadSnapshot};
    use crate::session::channel_mapping::ChannelMappingSettings;
    use crate::session::NodeSettings;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};
    use test_skip::skip;

    fn nvnmosd_bin() -> PathBuf {
        if let Ok(p) = std::env::var("NVNMOSD_BIN") {
            return PathBuf::from(p);
        }
        let target_dir = std::env::var("CARGO_TARGET_DIR").ok().unwrap_or_else(|| {
            let manifest =
                std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
            PathBuf::from(manifest)
                .parent()
                .expect("manifest parent")
                .join("target")
                .to_string_lossy()
                .into_owned()
        });
        let debug = PathBuf::from(&target_dir).join("debug").join("nvnmosd");
        if debug.exists() {
            return debug;
        }
        PathBuf::from(target_dir).join("release").join("nvnmosd")
    }

    fn libnvnmos_dir() -> Option<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Ok(paths) = std::env::var("LD_LIBRARY_PATH") {
            dirs.extend(paths.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
        }
        if let Ok(dir) = std::env::var("NVNMOS_LIB_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        dirs.into_iter().find(|d| d.join("libnvnmos.so").exists())
    }

    /// Mirror of `tests/common::nvnmosd_skip_reason` for this in-crate integration
    /// test, which needs `pub(crate)` access and so cannot live under `tests/`.
    /// `nvnmosd` is built by `cargo test`, but it links `libnvnmos.so` from the C
    /// build, which CI exposes on `LD_LIBRARY_PATH`; a Rust-only checkout skips.
    fn skip_reason() -> Option<String> {
        let bin = nvnmosd_bin();
        if !bin.exists() {
            return Some(format!("nvnmosd not built at `{}`", bin.display()));
        }
        if libnvnmos_dir().is_none() {
            return Some("libnvnmos.so not found via LD_LIBRARY_PATH or NVNMOS_LIB_DIR".into());
        }
        None
    }

    struct DaemonGuard {
        child: Child,
        socket: PathBuf,
    }

    impl DaemonGuard {
        fn new(socket: PathBuf) -> Self {
            let bin = nvnmosd_bin();
            assert!(
                bin.exists(),
                "nvnmosd binary not found at `{}`; build with `cargo build -p nvnmosd` \
                 or set NVNMOSD_BIN",
                bin.display(),
            );
            let _ = std::fs::remove_file(&socket);
            let mut command = Command::new(&bin);
            command
                .arg("--uds")
                .arg(&socket)
                .env("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
                .stdout(Stdio::null())
                .stderr(Stdio::inherit());
            // nvnmosd links libnvnmos.so; surface NVNMOS_LIB_DIR to the loader even
            // when the caller set only it (not LD_LIBRARY_PATH).
            if let Ok(lib_dir) = std::env::var("NVNMOS_LIB_DIR") {
                let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
                let value = if existing.is_empty() {
                    lib_dir
                } else {
                    format!("{lib_dir}:{existing}")
                };
                command.env("LD_LIBRARY_PATH", value);
            }
            let mut child = command
                .spawn()
                .unwrap_or_else(|e| panic!("spawn `{}`: {e}", bin.display()));
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if socket.exists() {
                    return Self { child, socket };
                }
                thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "nvnmosd UDS `{}` did not appear within 5s; check LD_LIBRARY_PATH includes libnvnmos",
                socket.display(),
            );
        }

        fn uri(&self) -> String {
            format!("unix:{}", self.socket.display())
        }
    }

    impl Drop for DaemonGuard {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    #[test]
    fn channel_mapping_session_add_and_remove() {
        if let Some(why) = skip_reason() {
            skip!(why);
        }
        let socket = tempfile::Builder::new()
            .prefix("nvnmos_is08_sess_")
            .suffix(".sock")
            .tempfile_in(std::env::temp_dir())
            .expect("temp socket")
            .into_temp_path();
        let daemon = DaemonGuard::new(socket.to_path_buf());

        let handler: ChannelMappingActivationHandler = Arc::new(|_req, tx| {
            let _ = tx.send(ChannelMappingActivationOutcome::Applied);
        });
        let settings = ChannelMappingSettings {
            daemon_uri: daemon.uri(),
            node: NodeSettings {
                node_seed: format!("gst-nmos-rs-is08-{}", std::process::id()),
                ..NodeSettings::default()
            },
            channelmapping_name: "test-map".into(),
        };

        SHARED_RUNTIME.block_on(async {
            let mut session = ChannelMappingSession::open(&settings, handler)
                .await
                .expect("OpenSession");

            let sinks = vec![SinkPadSnapshot {
                receiver_name: String::new(),
                input_id: String::new(),
                label: String::new(),
                description: String::new(),
                negotiated_channels: 2,
            }];
            let srcs = vec![SrcPadSnapshot {
                sender_name: String::new(),
                output_id: String::new(),
                label: String::new(),
                description: String::new(),
                negotiated_channels: 2,
                active_map: None,
            }];
            let req = build_add_channel_mapping_request(
                &session.session_handle,
                &settings.channelmapping_name,
                &sinks,
                &srcs,
                false,
            );
            let resp = session
                .add_channel_mapping(req)
                .await
                .expect("AddChannelMapping");
            assert_eq!(resp.input_ids.len(), 1);
            assert_eq!(resp.output_ids.len(), 1);
            assert!(!resp.input_ids[0].is_empty());
            assert!(!resp.output_ids[0].is_empty());

            session
                .remove_channel_mapping()
                .await
                .expect("RemoveChannelMapping");
            session.close().await.expect("CloseSession");
        });
    }
}
