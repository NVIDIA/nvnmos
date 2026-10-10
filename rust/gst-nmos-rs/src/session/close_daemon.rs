// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `CloseSession`, `AddChannelMapping`, and `SyncChannelMappingState`
//! against a daemon that answers, or does not.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nvnmos_rpc::v1::nvnmos_daemon_server::{NvnmosDaemon, NvnmosDaemonServer};
use nvnmos_rpc::v1::{self as rpc, Empty};
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use crate::channel_mapping_session::{
    ChannelMappingActivationHandler, ChannelMappingActivationOutcome, ChannelMappingSession,
};
use crate::daemon::{ActivationHandler, ActivationOutcome, DaemonError, Session};
use crate::runtime::SHARED_RUNTIME;

use super::channel_mapping::{self, ChannelMappingSettings};
use super::support::{cat, init_gst, settings};
use super::{NodeSettings, OPEN_TIMEOUT, Side, close, transport_to_proto};

/// How long the stub holds the outstanding RPC.
/// Twice [`OPEN_TIMEOUT`], so a call bounded by that timeout returns
/// while this hold is still running.
fn hold_for() -> Duration {
    OPEN_TIMEOUT + OPEN_TIMEOUT
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HeldCall {
    CloseSession,
    AddChannelMapping,
    SyncChannelMappingState,
}

enum Mode {
    /// Complete `CloseSession` and notify the test.
    Answer(std::sync::mpsc::Sender<()>),
    /// Complete `call` only after [`hold_for`], then set the flag.
    /// Every other RPC returns immediately.
    Hold {
        call: HeldCall,
        replied: Arc<AtomicBool>,
        hold: Duration,
    },
}

struct Stub {
    mode: Mode,
}

type EventStream<T> = ReceiverStream<Result<T, Status>>;

fn idle_stream<T>() -> EventStream<T> {
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    ReceiverStream::new(rx)
}

#[allow(clippy::result_large_err)]
fn unused<T>() -> Result<Response<T>, Status> {
    Err(Status::unimplemented("unused"))
}

#[tonic::async_trait]
impl NvnmosDaemon for Stub {
    async fn add_node(
        &self,
        _request: Request<rpc::AddNodeRequest>,
    ) -> Result<Response<rpc::AddNodeResponse>, Status> {
        unused()
    }

    async fn remove_node(
        &self,
        _request: Request<rpc::RemoveNodeRequest>,
    ) -> Result<Response<Empty>, Status> {
        unused()
    }

    async fn open_session(
        &self,
        _request: Request<rpc::OpenSessionRequest>,
    ) -> Result<Response<rpc::OpenSessionResponse>, Status> {
        Ok(Response::new(rpc::OpenSessionResponse {
            session_handle: "session-1".to_owned(),
            node_id: "node-1".to_owned(),
            created_node: true,
            http_port: 0,
        }))
    }

    async fn close_session(
        &self,
        _request: Request<rpc::CloseSessionRequest>,
    ) -> Result<Response<Empty>, Status> {
        self.hold_if(HeldCall::CloseSession).await;
        if let Mode::Answer(closed) = &self.mode {
            let _ = closed.send(());
        }
        Ok(Response::new(Empty {}))
    }

    async fn add_sender(
        &self,
        _request: Request<rpc::AddSenderRequest>,
    ) -> Result<Response<rpc::AddSenderResponse>, Status> {
        unused()
    }

    async fn add_receiver(
        &self,
        _request: Request<rpc::AddReceiverRequest>,
    ) -> Result<Response<rpc::AddReceiverResponse>, Status> {
        unused()
    }

    async fn remove_resource(
        &self,
        _request: Request<rpc::RemoveResourceRequest>,
    ) -> Result<Response<Empty>, Status> {
        unused()
    }

    type SubscribeActivationsStream = EventStream<rpc::ActivationEvent>;

    async fn subscribe_activations(
        &self,
        _request: Request<rpc::SubscribeActivationsRequest>,
    ) -> Result<Response<Self::SubscribeActivationsStream>, Status> {
        Ok(Response::new(idle_stream()))
    }

    async fn ack_activation(
        &self,
        _request: Request<rpc::AckActivationRequest>,
    ) -> Result<Response<Empty>, Status> {
        unused()
    }

    async fn sync_resource_state(
        &self,
        _request: Request<rpc::SyncResourceStateRequest>,
    ) -> Result<Response<Empty>, Status> {
        unused()
    }

    async fn add_channel_mapping(
        &self,
        _request: Request<rpc::AddChannelMappingRequest>,
    ) -> Result<Response<rpc::AddChannelMappingResponse>, Status> {
        self.hold_if(HeldCall::AddChannelMapping).await;
        Ok(Response::new(rpc::AddChannelMappingResponse {
            channelmapping_handle: "cm-1".to_owned(),
            input_ids: vec!["in-1".to_owned()],
            output_ids: vec!["out-1".to_owned()],
        }))
    }

    async fn remove_channel_mapping(
        &self,
        _request: Request<rpc::RemoveChannelMappingRequest>,
    ) -> Result<Response<Empty>, Status> {
        unused()
    }

    async fn sync_channel_mapping_state(
        &self,
        _request: Request<rpc::SyncChannelMappingStateRequest>,
    ) -> Result<Response<Empty>, Status> {
        self.hold_if(HeldCall::SyncChannelMappingState).await;
        Ok(Response::new(Empty {}))
    }

    type SubscribeChannelMappingActivationsStream = EventStream<rpc::ChannelMappingActivationEvent>;

    async fn subscribe_channel_mapping_activations(
        &self,
        _request: Request<rpc::SubscribeChannelMappingActivationsRequest>,
    ) -> Result<Response<Self::SubscribeChannelMappingActivationsStream>, Status> {
        Ok(Response::new(idle_stream()))
    }

    async fn ack_channel_mapping_activation(
        &self,
        _request: Request<rpc::AckChannelMappingActivationRequest>,
    ) -> Result<Response<Empty>, Status> {
        unused()
    }
}

impl Stub {
    async fn hold_if(&self, call: HeldCall) {
        let Mode::Hold {
            call: held,
            replied,
            hold,
        } = &self.mode
        else {
            return;
        };
        if *held != call {
            return;
        }
        tokio::time::sleep(*hold).await;
        replied.store(true, Ordering::SeqCst);
    }
}

struct TestDaemon {
    uri: String,
    _dir: tempfile::TempDir,
    server: tokio::task::AbortHandle,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl TestDaemon {
    fn start(mode: Mode) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nvnmosd.sock");
        let uri = format!("unix:{}", path.display());
        let listener = SHARED_RUNTIME
            .block_on(async move { tokio::net::UnixListener::bind(&path) })
            .expect("bind stub daemon");
        let server = SHARED_RUNTIME.spawn(async move {
            let incoming = UnixListenerStream::new(listener);
            let _ = Server::builder()
                .add_service(NvnmosDaemonServer::new(Stub { mode }))
                .serve_with_incoming_shutdown(incoming, std::future::pending::<()>())
                .await;
        });
        Self {
            uri,
            _dir: dir,
            server: server.abort_handle(),
        }
    }
}

fn open_session(uri: &str) -> Session {
    init_gst();
    let mut snapshot = settings(Side::Sender);
    snapshot.daemon_uri = uri.to_owned();
    let handler: ActivationHandler = Arc::new(|_req, tx| {
        let _ = tx.send(ActivationOutcome::Applied);
    });
    SHARED_RUNTIME
        .block_on(Session::open(
            &snapshot.daemon_uri,
            &snapshot,
            snapshot.side,
            &snapshot.name,
            transport_to_proto(snapshot.transport),
            None,
            handler,
        ))
        .expect("OpenSession")
}

fn open_channel_mapping(uri: &str) -> ChannelMappingSession {
    init_gst();
    let snapshot = ChannelMappingSettings {
        daemon_uri: uri.to_owned(),
        node: NodeSettings {
            node_seed: "test-seed".to_owned(),
            ..NodeSettings::default()
        },
        channelmapping_name: "test-map".to_owned(),
    };
    let handler: ChannelMappingActivationHandler = Arc::new(|_req, tx| {
        let _ = tx.send(ChannelMappingActivationOutcome::Applied);
    });
    SHARED_RUNTIME
        .block_on(ChannelMappingSession::open(&snapshot, handler))
        .expect("OpenSession")
}

/// READY→NULL close returns while `CloseSession` is still unanswered.
/// The daemon holds that RPC for twice [`OPEN_TIMEOUT`].
#[test]
fn close_returns_while_close_session_is_outstanding() {
    let replied = Arc::new(AtomicBool::new(false));
    let hold = hold_for();
    let daemon = TestDaemon::start(Mode::Hold {
        call: HeldCall::CloseSession,
        replied: Arc::clone(&replied),
        hold,
    });
    let session = Mutex::new(Some(open_session(&daemon.uri)));
    let started = Instant::now();
    close(&cat(), "nmossink", &session);
    assert!(
        started.elapsed() < hold,
        "close waited for CloseSession to finish"
    );
    assert!(
        !replied.load(Ordering::SeqCst),
        "close returned only after the daemon answered CloseSession"
    );
}

/// Dropping a session without `close` still sends `CloseSession`.
#[test]
fn dropped_session_sends_close_session() {
    let (tx, rx) = std::sync::mpsc::channel();
    let daemon = TestDaemon::start(Mode::Answer(tx));
    drop(open_session(&daemon.uri));
    rx.recv_timeout(OPEN_TIMEOUT)
        .expect("CloseSession after drop");
}

/// An explicit close still reaches a daemon that answers.
#[test]
fn close_sends_close_session_when_the_daemon_answers() {
    let (tx, rx) = std::sync::mpsc::channel();
    let daemon = TestDaemon::start(Mode::Answer(tx));
    let session = Mutex::new(Some(open_session(&daemon.uri)));
    close(&cat(), "nmossink", &session);
    rx.recv_timeout(OPEN_TIMEOUT).expect("CloseSession");
}

/// Channel-map READY→NULL close returns while `CloseSession` is still unanswered.
#[test]
fn channel_mapping_close_returns_while_close_session_is_outstanding() {
    let replied = Arc::new(AtomicBool::new(false));
    let hold = hold_for();
    let daemon = TestDaemon::start(Mode::Hold {
        call: HeldCall::CloseSession,
        replied: Arc::clone(&replied),
        hold,
    });
    let session = Mutex::new(Some(open_channel_mapping(&daemon.uri)));
    let started = Instant::now();
    channel_mapping::close(&cat(), "nmosaudiochannelmap", &session);
    assert!(
        started.elapsed() < hold,
        "close waited for CloseSession to finish"
    );
    assert!(
        !replied.load(Ordering::SeqCst),
        "close returned only after the daemon answered CloseSession"
    );
}

/// Dropping a channel-map session without `close` still sends `CloseSession`.
#[test]
fn dropped_channel_mapping_session_sends_close_session() {
    let (tx, rx) = std::sync::mpsc::channel();
    let daemon = TestDaemon::start(Mode::Answer(tx));
    drop(open_channel_mapping(&daemon.uri));
    rx.recv_timeout(OPEN_TIMEOUT)
        .expect("CloseSession after drop");
}

fn add_request() -> rpc::AddChannelMappingRequest {
    rpc::AddChannelMappingRequest {
        session_handle: "session-1".to_owned(),
        name: "test-map".to_owned(),
        ..Default::default()
    }
}

/// READY→PAUSED `AddChannelMapping` returns while that RPC is still unanswered.
/// The daemon holds it for twice [`OPEN_TIMEOUT`].
#[test]
fn add_channel_mapping_returns_while_the_rpc_is_outstanding() {
    let replied = Arc::new(AtomicBool::new(false));
    let hold = hold_for();
    let daemon = TestDaemon::start(Mode::Hold {
        call: HeldCall::AddChannelMapping,
        replied: Arc::clone(&replied),
        hold,
    });
    let mut session = open_channel_mapping(&daemon.uri);
    let started = Instant::now();
    let result = SHARED_RUNTIME.block_on(session.add_channel_mapping(add_request()));
    assert!(
        started.elapsed() < hold,
        "add waited for AddChannelMapping to finish"
    );
    assert!(
        !replied.load(Ordering::SeqCst),
        "add returned only after the daemon answered AddChannelMapping"
    );
    assert!(
        matches!(result, Err(DaemonError::TimedOut)),
        "AddChannelMapping returned {result:?}"
    );
}

/// `SyncChannelMappingState` returns while that RPC is still unanswered.
/// The daemon holds it for twice [`OPEN_TIMEOUT`].
#[test]
fn sync_channel_mapping_state_returns_while_the_rpc_is_outstanding() {
    let replied = Arc::new(AtomicBool::new(false));
    let hold = hold_for();
    let daemon = TestDaemon::start(Mode::Hold {
        call: HeldCall::SyncChannelMappingState,
        replied: Arc::clone(&replied),
        hold,
    });
    let mut session = open_channel_mapping(&daemon.uri);
    SHARED_RUNTIME
        .block_on(session.add_channel_mapping(add_request()))
        .expect("AddChannelMapping");
    let started = Instant::now();
    let result = SHARED_RUNTIME.block_on(session.sync_channel_mapping_state("out-1", Vec::new()));
    assert!(
        started.elapsed() < hold,
        "sync waited for SyncChannelMappingState to finish"
    );
    assert!(
        !replied.load(Ordering::SeqCst),
        "sync returned only after the daemon answered SyncChannelMappingState"
    );
    assert!(
        matches!(result, Err(DaemonError::TimedOut)),
        "SyncChannelMappingState returned {result:?}"
    );
}
