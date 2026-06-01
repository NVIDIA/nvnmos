// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use gstreamer_sdp::SDPMessage;

use crate::sdp::SdpError;

/// Reject SDP shapes this stack does not support on the passthrough path.
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
