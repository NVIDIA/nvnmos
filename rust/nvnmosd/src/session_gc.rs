// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Implicit [`State::close_session`] when clients abandon a session.
//!
//! One watchdog per session; two configured durations (`T_subscribe` after
//! `OpenSession`, `T_resubscribe` after the activation stream ends).
//! [`SessionGc::cancel_timeout`] runs synchronously on successful
//! `SubscribeActivations`; re-check for a live subscription before closing
//! when the watchdog fires.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::AbortHandle;

use crate::env_config::{self, EnvDefault};
use crate::malloc_trim;
use crate::state::State;

const DEFAULT_SUBSCRIBE_TIMEOUT_SEC: u64 = 60;
const DEFAULT_RESUBSCRIBE_TIMEOUT_SEC: u64 = 5;

/// Session GC configuration read once at daemon startup.
#[derive(Debug, Clone)]
pub struct SessionGcConfig {
    pub enabled: bool,
    pub subscribe_timeout: Duration,
    pub resubscribe_timeout: Duration,
}

impl SessionGcConfig {
    pub fn from_env() -> Self {
        let enabled = env_config::read_env_bool("NVNMOSD_SESSION_GC", EnvDefault::OptOut);
        let subscribe_timeout = env_config::read_timeout_secs(
            "NVNMOSD_SESSION_SUBSCRIBE_TIMEOUT_SEC",
            DEFAULT_SUBSCRIBE_TIMEOUT_SEC,
        );
        let resubscribe_timeout = env_config::read_timeout_secs(
            "NVNMOSD_SESSION_RESUBSCRIBE_TIMEOUT_SEC",
            DEFAULT_RESUBSCRIBE_TIMEOUT_SEC,
        );
        Self {
            enabled,
            subscribe_timeout,
            resubscribe_timeout,
        }
    }

    pub fn log_effective(&self) {
        tracing::info!(
            enabled = self.enabled,
            subscribe_timeout_sec = self.subscribe_timeout.as_secs(),
            resubscribe_timeout_sec = self.resubscribe_timeout.as_secs(),
            "session GC configuration"
        );
    }
}

/// Per-session watchdog scheduler.
#[derive(Clone)]
pub struct SessionGc {
    config: SessionGcConfig,
    state: Arc<Mutex<State>>,
    watchdogs: Arc<Mutex<HashMap<String, AbortHandle>>>,
}

impl SessionGc {
    pub fn new(state: Arc<Mutex<State>>) -> Self {
        let config = SessionGcConfig::from_env();
        config.log_effective();
        Self {
            config,
            state,
            watchdogs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn cancel_timeout(&self, session_handle: &str) {
        if let Some(handle) = self
            .watchdogs
            .lock()
            .expect("watchdog mutex poisoned")
            .remove(session_handle)
        {
            handle.abort();
        }
    }

    pub fn start_subscribe_timeout(&self, session_handle: &str) {
        self.start_timeout(session_handle, self.config.subscribe_timeout);
    }

    pub fn start_resubscribe_timeout(&self, session_handle: &str) {
        self.start_timeout(session_handle, self.config.resubscribe_timeout);
    }

    fn start_timeout(&self, session_handle: &str, timeout: Duration) {
        if !self.config.enabled {
            return;
        }
        self.cancel_timeout(session_handle);
        let state = self.state.clone();
        let handle = session_handle.to_string();
        let watchdogs = self.watchdogs.clone();
        let join = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let outcome = {
                let mut guard = state.lock().expect("daemon state mutex poisoned");
                if !guard.sessions_contains(&handle) {
                    return;
                }
                if guard.has_any_activation_subscription(&handle) {
                    return;
                }
                let outcome = match guard.close_session(&handle) {
                    Ok(outcome) => outcome,
                    Err(_) => return,
                };
                malloc_trim::maybe_after_close_session(&guard, &outcome);
                outcome
            };
            tracing::info!(
                session_handle = %handle,
                node_seed = %outcome.node_seed,
                node_id = %outcome.node_id,
                lifetime = outcome.lifetime.label(),
                remaining_sessions = outcome.remaining_sessions,
                node_destroyed = outcome.node_destroyed,
                implicit = true,
                "CloseSession",
            );
            let _ = watchdogs
                .lock()
                .expect("watchdog mutex poisoned")
                .remove(&handle);
        });
        self.watchdogs
            .lock()
            .expect("watchdog mutex poisoned")
            .insert(session_handle.to_string(), join.abort_handle());
    }
}
