// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Session lifecycle for `nmosaudiochannelmap`.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::bail;
use gstreamer as gst;

use crate::channel_mapping_session::{ChannelMappingActivationHandler, ChannelMappingSession};
use crate::daemon::{OpenJoin, RPC_TIMEOUT, SessionPhase, finish_opening, take_for_close};
use crate::runtime::{SHARED_RUNTIME, wait_for_task};
use crate::types::DEFAULT_DAEMON_URI;

use super::NodeSettings;

/// [`RPC_TIMEOUT`] for the open call.
const OPEN_TIMEOUT: Duration = RPC_TIMEOUT;

pub(crate) const CHANNELMAPPING_NAME_BLURB: &str = "\
    Caller-chosen name for this channel mapping. It must be unique within the \
    Node. It is not an IS-08 Input or Output ID.";

pub(crate) const RESTRICT_ROUTABLE_INPUTS_BLURB: &str = "\
    Limit each Output's IS-08 `/caps/routable_inputs` to this element's Inputs \
    and unrouted. When false, routable Inputs are unrestricted. Default: false.";

/// Settings snapshot for `nmosaudiochannelmap` NULL→READY.
#[derive(Debug, Clone)]
pub(crate) struct ChannelMappingSettings {
    pub(crate) daemon_uri: String,
    pub(crate) node: NodeSettings,
    pub(crate) channelmapping_name: String,
}

impl Default for ChannelMappingSettings {
    fn default() -> Self {
        Self {
            daemon_uri: DEFAULT_DAEMON_URI.to_owned(),
            node: NodeSettings::default(),
            channelmapping_name: String::new(),
        }
    }
}

pub(crate) fn validate_and_open(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &ChannelMappingSettings,
    session: &Mutex<Option<SessionPhase<ChannelMappingSession>>>,
    activation_handler: ChannelMappingActivationHandler,
) -> Result<(), anyhow::Error> {
    if settings.node.node_seed.is_empty() {
        bail!("{element}: `node-seed` is required");
    }
    if settings.channelmapping_name.is_empty() {
        bail!("{element}: `channelmapping-name` is required");
    }

    let slot = session.lock().unwrap().take();
    match slot {
        Some(SessionPhase::Pending(handle)) => match finish_opening(handle) {
            OpenJoin::Running(handle) => {
                *session.lock().unwrap() = Some(SessionPhase::Pending(handle));
                bail!(
                    "{element}: OpenSession against {} is still running",
                    settings.daemon_uri
                );
            }
            OpenJoin::Ready(opened) => {
                let _ = SHARED_RUNTIME.block_on(opened.close());
            }
            OpenJoin::Finished => {}
        },
        Some(SessionPhase::Open(existing)) => {
            *session.lock().unwrap() = Some(SessionPhase::Open(existing));
            bail!("{element}: session is already open");
        }
        None => {}
    }
    let settings_for_open = settings.clone();
    let handler = activation_handler.clone();
    let mut opening = SHARED_RUNTIME
        .spawn(async move { ChannelMappingSession::open(&settings_for_open, handler).await });
    let new_session = match SHARED_RUNTIME.block_on(wait_for_task(&mut opening, OPEN_TIMEOUT)) {
        None => {
            *session.lock().unwrap() = Some(SessionPhase::Pending(opening));
            bail!(
                "{element}: OpenSession against {} timed out",
                settings.daemon_uri
            );
        }
        Some(Ok(Ok(opened))) => opened,
        Some(Ok(Err(error))) => {
            return Err(anyhow::Error::new(error).context(format!(
                "{element}: OpenSession against {}",
                settings.daemon_uri
            )));
        }
        Some(Err(join)) => bail!("{element}: OpenSession ended: {join}"),
    };

    gst::info!(
        cat,
        "channel mapping session opened: handle={} node_id={} created_node={} http_port={} \
         (node_seed={}, channelmapping_name={})",
        new_session.session_handle,
        new_session.node_id,
        new_session.created_node,
        new_session.http_port,
        settings.node.node_seed,
        settings.channelmapping_name,
    );

    *session.lock().unwrap() = Some(SessionPhase::Open(new_session));
    Ok(())
}

pub(crate) fn close(
    cat: &gst::DebugCategory,
    element: &str,
    session: &Mutex<Option<SessionPhase<ChannelMappingSession>>>,
) {
    let Some(s) = take_for_close(session) else {
        return;
    };
    {
        let handle = s.session_handle.clone();
        let result = SHARED_RUNTIME.block_on(s.close());
        match result {
            Ok(()) => gst::info!(cat, "channel mapping session closed: handle={handle}"),
            Err(e) => gst::warning!(cat, "{element}: CloseSession (handle={handle}): {e}"),
        }
    }
}
