// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use gstreamer as gst;

/// Snapshot of one sink pad at fixation time.
#[derive(Debug, Clone)]
pub(crate) struct SinkPadSnapshot {
    pub(crate) receiver_name: String,
    pub(crate) input_id: String,
    pub(crate) label: String,
    pub(crate) description: String,
    pub(crate) negotiated_channels: u32,
}

/// Snapshot of one src pad at fixation time.
#[derive(Debug, Clone)]
pub(crate) struct SrcPadSnapshot {
    pub(crate) sender_name: String,
    pub(crate) output_id: String,
    pub(crate) label: String,
    pub(crate) description: String,
    pub(crate) negotiated_channels: u32,
    pub(crate) active_map: Option<gst::Structure>,
}

/// Topology frozen after the first successful internal build.
#[derive(Debug, Clone)]
pub(crate) struct FrozenTopology {
    pub(crate) input_ids: Vec<String>,
    pub(crate) output_ids: Vec<String>,
    /// Negotiated channel count per IS-08 Input (same order as `input_ids`).
    pub(crate) input_channels: Vec<u32>,
    /// Negotiated channel count per IS-08 Output (same order as `output_ids`).
    pub(crate) output_channels: Vec<u32>,
    /// Input-bus slot for each Input's channel 0 (disjoint placement).
    pub(crate) input_bus_offsets: Vec<u32>,
    /// Width of the input bus (`sum(input_channels)`).
    pub(crate) input_bus_channels: u32,
}

impl FrozenTopology {
    pub(crate) fn from_snapshots(
        sinks: &[SinkPadSnapshot],
        srcs: &[SrcPadSnapshot],
        effective_input_ids: &[String],
        effective_output_ids: &[String],
    ) -> Self {
        let input_channels: Vec<u32> = sinks.iter().map(|s| s.negotiated_channels).collect();
        let output_channels: Vec<u32> = srcs.iter().map(|s| s.negotiated_channels).collect();
        let mut offset = 0u32;
        let input_bus_offsets: Vec<u32> = input_channels
            .iter()
            .map(|&n| {
                let o = offset;
                offset += n;
                o
            })
            .collect();
        let input_bus_channels = offset;
        Self {
            input_ids: effective_input_ids.to_vec(),
            output_ids: effective_output_ids.to_vec(),
            input_channels,
            output_channels,
            input_bus_offsets,
            input_bus_channels,
        }
    }

    /// Map `(input_id, input_channel)` to an input-bus channel index.
    pub(crate) fn input_bus_index(&self, input_id: &str, input_channel: u32) -> Option<u32> {
        let input_idx = self.input_ids.iter().position(|id| id == input_id)?;
        let offset = self.input_bus_offsets.get(input_idx).copied()?;
        let channels = self.input_channels.get(input_idx).copied()?;
        if input_channel >= channels {
            return None;
        }
        Some(offset + input_channel)
    }
}
