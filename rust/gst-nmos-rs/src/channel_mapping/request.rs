// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Build `AddChannelMappingRequest` from pad snapshots.

use nvnmos_rpc::v1::{
    AddChannelMappingRequest, ChannelMappingInput, ChannelMappingOutput, ChannelMappingParentType,
};

use super::types::{SinkPadSnapshot, SrcPadSnapshot};

/// libnvnmos requires `channel_labels.len() == negotiated channel count` (the
/// vector must not be empty when `channels > 0`). Individual labels may be
/// empty strings, but we default to `"0"`, `"1"`, … at the GStreamer layer
/// where pad geometry is known.
fn default_channel_labels(channels: u32) -> Vec<String> {
    (0..channels).map(|i| i.to_string()).collect()
}

pub(crate) fn build_add_channel_mapping_request(
    session_handle: &str,
    channelmapping_name: &str,
    sinks: &[SinkPadSnapshot],
    srcs: &[SrcPadSnapshot],
    restrict_routable_inputs: bool,
) -> AddChannelMappingRequest {
    let input_ids_for_routable: Vec<String> = sinks.iter().map(|s| s.input_id.clone()).collect();

    let inputs = sinks
        .iter()
        .map(|s| ChannelMappingInput {
            id: s.input_id.clone(),
            name: s.label.clone(),
            description: s.description.clone(),
            channel_labels: default_channel_labels(s.negotiated_channels),
            parent_name: s.receiver_name.clone(),
            parent_type: ChannelMappingParentType::Receiver as i32,
            reordering: true,
            block_size: 1,
        })
        .collect();

    let outputs = srcs
        .iter()
        .map(|s| {
            let routable_inputs = if restrict_routable_inputs {
                input_ids_for_routable.clone()
            } else {
                Vec::new()
            };
            ChannelMappingOutput {
                id: s.output_id.clone(),
                name: s.label.clone(),
                description: s.description.clone(),
                channel_labels: default_channel_labels(s.negotiated_channels),
                sender_name: s.sender_name.clone(),
                routable_inputs,
            }
        })
        .collect();

    AddChannelMappingRequest {
        session_handle: session_handle.to_owned(),
        name: channelmapping_name.to_owned(),
        inputs,
        outputs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restrict_routable_inputs_populates_outputs() {
        let sinks = vec![SinkPadSnapshot {
            receiver_name: "rx1".into(),
            input_id: "input0".into(),
            label: String::new(),
            description: String::new(),
            negotiated_channels: 2,
        }];
        let srcs = vec![SrcPadSnapshot {
            sender_name: "tx1".into(),
            output_id: "output0".into(),
            label: String::new(),
            description: String::new(),
            negotiated_channels: 2,
            active_map: None,
        }];
        let req = build_add_channel_mapping_request("sess", "studio", &sinks, &srcs, true);
        assert_eq!(req.name, "studio");
        assert_eq!(req.inputs.len(), 1);
        assert_eq!(req.inputs[0].parent_name, "rx1");
        assert_eq!(req.inputs[0].channel_labels, vec!["0", "1"]);
        assert_eq!(req.outputs[0].routable_inputs, vec!["input0"]);
    }

    #[test]
    fn unrestricted_leaves_routable_inputs_empty() {
        let sinks = vec![SinkPadSnapshot {
            receiver_name: String::new(),
            input_id: "input0".into(),
            label: String::new(),
            description: String::new(),
            negotiated_channels: 0,
        }];
        let srcs = vec![SrcPadSnapshot {
            sender_name: String::new(),
            output_id: "output0".into(),
            label: String::new(),
            description: String::new(),
            negotiated_channels: 0,
            active_map: None,
        }];
        let req = build_add_channel_mapping_request("sess", "map", &sinks, &srcs, false);
        assert!(req.outputs[0].routable_inputs.is_empty());
    }
}
