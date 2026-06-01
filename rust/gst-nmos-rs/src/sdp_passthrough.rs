// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! In-place SDP mutation for transport-file passthrough fidelity.
//!
//! When the user supplies a transport file, property overrides are applied
//! directly on the parsed `SDPMessage` tree rather than round-tripping
//! through [`crate::sdp::parse_sdp`] / [`crate::sdp::build_sdp`], so
//! vendor and spec-extension attributes the model does not represent survive
//! verbatim to `libnvnmos`.

use gstreamer_sdp::{SDPAttribute, SDPMediaRef, SDPConnection, SDPMessage};

use crate::sdp::{defaults, SdpError, SdpOverrides};
use crate::types::CapsMode;

/// Reject SDP shapes this stack does not support on the passthrough path.
///
/// Single-`m=` SDPs pass. Two `m=` blocks are reserved for future ST 2022-7
/// support and are rejected with [`SdpError::MultipleMedia`] until dual-leg
/// handling lands. Video+audio (or any mixed `m=` media type) in one SDP is
/// rejected with [`SdpError::MultiMediaMixedEssence`]. Three or more `m=`
/// blocks yield [`SdpError::TooManyMediaBlocks`].
pub(crate) fn reject_unsupported_multi_media(msg: &SDPMessage) -> Result<(), SdpError> {
    let num_medias = msg.medias_len() as usize;
    if num_medias == 0 {
        return Err(SdpError::NoMedia);
    }
    if num_medias > 2 {
        return Err(SdpError::TooManyMediaBlocks(num_medias));
    }
    if num_medias == 2 {
        let first = msg.media(0).and_then(|m| m.media()).unwrap_or("");
        let second = msg.media(1).and_then(|m| m.media()).unwrap_or("");
        if !first.eq_ignore_ascii_case(second) {
            return Err(SdpError::MultiMediaMixedEssence);
        }
        return Err(SdpError::MultipleMedia(2));
    }
    Ok(())
}

/// Apply session-level [`SdpOverrides`] in place (`s=`, `i=`, `a=x-nvnmos-name`).
pub(crate) fn apply_session_overrides_in_place(
    msg: &mut SDPMessage,
    overrides: &SdpOverrides<'_>,
) -> Result<(), SdpError> {
    if let Some(label) = overrides.label {
        msg.set_session_name(label);
    }
    if let Some(description) = overrides.description {
        msg.set_information(description);
    }
    if let Some(name) = overrides.name {
        upsert_session_attribute(msg, "x-nvnmos-name", Some(name));
    }
    Ok(())
}

/// Apply media-level [`SdpOverrides`] to every `m=` block in `msg`.
pub(crate) fn apply_media_overrides_in_place(
    msg: &mut SDPMessage,
    overrides: &SdpOverrides<'_>,
) -> Result<(), SdpError> {
    let num_medias = msg.medias_len();
    for idx in 0..num_medias {
        let Some(m) = msg.media_mut(idx) else {
            continue;
        };
        apply_media_overrides_on_leg(m, overrides)?;
    }
    Ok(())
}
