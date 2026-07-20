// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Session lifecycle for `nmosaudiochannelmap`.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, bail};
use gstreamer as gst;

use crate::channel_mapping_session::{ChannelMappingActivationHandler, ChannelMappingSession};
use crate::runtime::SHARED_RUNTIME;
use crate::types::DEFAULT_DAEMON_URI;

use super::NodeSettings;

const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) const CHANNELMAPPING_NAME_BLURB: &str = "\
    Caller-chosen name for this channel mapping. It must be unique within the \
    Node. It is not an IS-08 Input or Output ID.";

pub(crate) const RESTRICT_ROUTABLE_INPUTS_BLURB: &str = "\
    Limit each Output's IS-08 `/caps/routable_inputs` to this element's Inputs. \
    When false, routable Inputs are unrestricted. Default: false.";

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
    session: &Mutex<Option<ChannelMappingSession>>,
    activation_handler: ChannelMappingActivationHandler,
) -> Result<(), anyhow::Error> {
    if settings.node.node_seed.is_empty() {
        bail!("{element}: `node-seed` is required");
    }
    if settings.channelmapping_name.is_empty() {
        bail!("{element}: `channelmapping-name` is required");
    }

    let settings = settings.clone();
    let new_session = SHARED_RUNTIME
        .block_on(async {
            tokio::time::timeout(
                OPEN_TIMEOUT,
                ChannelMappingSession::open(&settings, activation_handler),
            )
            .await
        })
        .with_context(|| {
            format!(
                "{element}: OpenSession against {} timed out",
                settings.daemon_uri
            )
        })?
        .with_context(|| format!("{element}: OpenSession against {}", settings.daemon_uri))?;

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

    *session.lock().unwrap() = Some(new_session);
    Ok(())
}

pub(crate) fn close(
    cat: &gst::DebugCategory,
    element: &str,
    session: &Mutex<Option<ChannelMappingSession>>,
) {
    let to_close = session.lock().unwrap().take();
    if let Some(s) = to_close {
        let handle = s.session_handle.clone();
        let result = SHARED_RUNTIME.block_on(s.close());
        match result {
            Ok(()) => gst::info!(cat, "channel mapping session closed: handle={handle}"),
            Err(e) => gst::warning!(cat, "{element}: CloseSession (handle={handle}): {e}"),
        }
    }
}
