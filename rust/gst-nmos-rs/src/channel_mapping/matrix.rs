// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Mix-matrix builders for `audiomixer` and `audiomixmatrix`.

use std::collections::HashSet;

use gstreamer as gst;
use thiserror::Error;

use super::active_map::ActiveMapRoute;
use super::types::FrozenTopology;
use crate::channel_mapping_session::ChannelMappingActivationRequest;

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ActiveMapValidationError {
    #[error("unknown input id `{0}`")]
    UnknownInputId(String),
    #[error(
        "input channel {input_channel} out of range for input `{input_id}` (max {max_channel})"
    )]
    InputChannelOutOfRange {
        input_id: String,
        input_channel: u32,
        max_channel: u32,
    },
    #[error("duplicate routing to output channel {0}")]
    DuplicateOutputChannel(u32),
    #[error("output channel {0} out of range (max {1})")]
    OutputChannelOutOfRange(u32, u32),
}

/// Build `input_bus_channels × input_channels` mix matrix for one mixer sink pad.
pub(crate) fn build_input_bus_mix_matrix(
    input_bus_channels: u32,
    input_channels: u32,
    bus_offset: u32,
) -> Vec<Vec<f32>> {
    let t = input_bus_channels as usize;
    let n = input_channels as usize;
    let mut matrix = vec![vec![0.0f32; n]; t];
    for ch in 0..n {
        matrix[bus_offset as usize + ch][ch] = 1.0;
    }
    matrix
}

/// Build `output_channels × input_bus_channels` mix matrix for one `audiomixmatrix` src pad.
pub(crate) fn build_output_mix_matrix(
    topology: &FrozenTopology,
    output_channels: u32,
    routes: &[ActiveMapRoute],
) -> Result<Vec<Vec<f32>>, ActiveMapValidationError> {
    validate_active_map_for_output(topology, output_channels, routes)?;
    let t = topology.input_bus_channels as usize;
    let m = output_channels as usize;
    let mut matrix = vec![vec![0.0f32; t]; m];
    for route in routes {
        let bus_index = topology
            .input_bus_index(&route.input_id, route.input_channel)
            .ok_or_else(|| ActiveMapValidationError::UnknownInputId(route.input_id.clone()))?;
        matrix[route.output_channel as usize][bus_index as usize] = 1.0;
    }
    Ok(matrix)
}

pub(crate) fn validate_active_map_for_output(
    topology: &FrozenTopology,
    output_channels: u32,
    routes: &[ActiveMapRoute],
) -> Result<(), ActiveMapValidationError> {
    let mut seen_outputs = HashSet::new();
    for route in routes {
        if route.output_channel >= output_channels {
            return Err(ActiveMapValidationError::OutputChannelOutOfRange(
                route.output_channel,
                output_channels.saturating_sub(1),
            ));
        }
        if !seen_outputs.insert(route.output_channel) {
            return Err(ActiveMapValidationError::DuplicateOutputChannel(
                route.output_channel,
            ));
        }
        let pad_idx = topology
            .input_ids
            .iter()
            .position(|id| id == &route.input_id)
            .ok_or_else(|| ActiveMapValidationError::UnknownInputId(route.input_id.clone()))?;
        let max_ch = topology.input_channels.get(pad_idx).copied().unwrap_or(0);
        if route.input_channel >= max_ch {
            return Err(ActiveMapValidationError::InputChannelOutOfRange {
                input_id: route.input_id.clone(),
                input_channel: route.input_channel,
                max_channel: max_ch.saturating_sub(1),
            });
        }
    }
    Ok(())
}

/// Convert a controller activation event into routes for one output.
pub(crate) fn routes_from_activation(
    topology: &FrozenTopology,
    output_id: &str,
    active_map: &[nvnmos_rpc::v1::ActiveMapEntry],
) -> Result<Vec<ActiveMapRoute>, ActiveMapValidationError> {
    let output_index = topology
        .output_ids
        .iter()
        .position(|id| id == output_id)
        .ok_or(ActiveMapValidationError::OutputChannelOutOfRange(0, 0))?;
    let output_channels = topology
        .output_channels
        .get(output_index)
        .copied()
        .unwrap_or(0);
    let routes: Vec<ActiveMapRoute> = active_map
        .iter()
        .enumerate()
        .filter_map(|(out_ch, entry)| {
            let input_id = entry.input_id.as_ref()?;
            if input_id.is_empty() {
                return None;
            }
            Some(ActiveMapRoute {
                output_channel: out_ch as u32,
                input_id: input_id.clone(),
                input_channel: entry.input_channel?,
            })
        })
        .collect();
    validate_active_map_for_output(topology, output_channels, &routes)?;
    Ok(routes)
}

pub(crate) fn routes_from_activation_request(
    topology: &FrozenTopology,
    req: &ChannelMappingActivationRequest,
) -> Result<Vec<ActiveMapRoute>, ActiveMapValidationError> {
    routes_from_activation(topology, &req.output_id, &req.active_map)
}

use glib::prelude::*;

/// Pack `out-channels × in-channels` matrix for `audiomixmatrix.matrix` (coefficients are `double`).
pub(crate) fn matrix_to_audiomixmatrix_gvalue(rows: &[Vec<f32>]) -> gst::Array {
    gst::Array::from_values(rows.iter().map(|row| {
        let inner =
            gst::Array::from_values(row.iter().copied().map(|v| (f64::from(v)).to_send_value()));
        glib::SendValue::from_owned(inner)
    }))
}

/// Build `converter-config` for an audiomixer sink pad (`input_bus_channels × input_channels` mix matrix).
pub(crate) fn input_bus_converter_config(
    matrix: &[Vec<f32>],
) -> gstreamer_audio::AudioConverterConfig {
    let mut config = gstreamer_audio::AudioConverterConfig::new();
    config.set_mix_matrix(matrix);
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel_mapping::types::{SinkPadSnapshot, SrcPadSnapshot};
    use crate::test_support::init_gst;

    fn stereo_topology() -> FrozenTopology {
        let sinks = vec![
            SinkPadSnapshot {
                receiver_name: String::new(),
                input_id: "input0".into(),
                label: String::new(),
                description: String::new(),
                negotiated_channels: 2,
            },
            SinkPadSnapshot {
                receiver_name: String::new(),
                input_id: "input1".into(),
                label: String::new(),
                description: String::new(),
                negotiated_channels: 2,
            },
        ];
        let src = SrcPadSnapshot {
            sender_name: String::new(),
            output_id: "output0".into(),
            label: String::new(),
            description: String::new(),
            negotiated_channels: 4,
            active_map: None,
        };
        FrozenTopology::from_snapshots(
            &sinks,
            &[src],
            &["input0".into(), "input1".into()],
            &["output0".into()],
        )
    }

    #[test]
    fn input_bus_converter_config_sets_gst_audio_converter_mix_matrix() {
        init_gst();
        let matrix = build_input_bus_mix_matrix(4, 2, 0);
        let config = input_bus_converter_config(&matrix);
        assert_eq!(config.mix_matrix(), matrix);
    }

    #[test]
    fn input_bus_matrix_places_channels_disjointly() {
        let m0 = build_input_bus_mix_matrix(4, 2, 0);
        assert_eq!(m0[0][0], 1.0);
        assert_eq!(m0[1][1], 1.0);
        assert_eq!(m0[2][0], 0.0);
        let m1 = build_input_bus_mix_matrix(4, 2, 2);
        assert_eq!(m1[2][0], 1.0);
        assert_eq!(m1[3][1], 1.0);
    }

    #[test]
    fn output_matrix_routes_input_bus_channels() {
        let topology = stereo_topology();
        let routes = vec![
            ActiveMapRoute {
                output_channel: 0,
                input_id: "input0".into(),
                input_channel: 0,
            },
            ActiveMapRoute {
                output_channel: 3,
                input_id: "input1".into(),
                input_channel: 1,
            },
        ];
        let matrix = build_output_mix_matrix(&topology, 4, &routes).unwrap();
        assert_eq!(matrix[0][0], 1.0);
        assert_eq!(matrix[3][3], 1.0);
        assert_eq!(matrix[0][1], 0.0);
    }

    #[test]
    fn rejects_unknown_input_id() {
        let topology = stereo_topology();
        let routes = vec![ActiveMapRoute {
            output_channel: 0,
            input_id: "nope".into(),
            input_channel: 0,
        }];
        assert!(matches!(
            build_output_mix_matrix(&topology, 4, &routes),
            Err(ActiveMapValidationError::UnknownInputId(_))
        ));
    }

    #[test]
    fn audiomixmatrix_accepts_output_matrix_gvalue() {
        init_gst();
        use gstreamer::prelude::*;
        let el = gstreamer::ElementFactory::make("audiomixmatrix")
            .property("in-channels", 4u32)
            .property("out-channels", 2u32)
            .build()
            .expect("audiomixmatrix");
        let topology = stereo_topology();
        let routes = vec![
            ActiveMapRoute {
                output_channel: 0,
                input_id: "input0".into(),
                input_channel: 0,
            },
            ActiveMapRoute {
                output_channel: 1,
                input_id: "input0".into(),
                input_channel: 1,
            },
        ];
        let mix = build_output_mix_matrix(&topology, 2, &routes).unwrap();
        assert_eq!(mix.len(), 2);
        assert_eq!(mix[0].len(), 4);
        el.set_property("matrix", matrix_to_audiomixmatrix_gvalue(&mix));
    }

    fn dual_src_topology() -> (FrozenTopology, Vec<SinkPadSnapshot>, Vec<SrcPadSnapshot>) {
        let sinks = vec![
            SinkPadSnapshot {
                receiver_name: String::new(),
                input_id: "input0".into(),
                label: String::new(),
                description: String::new(),
                negotiated_channels: 2,
            },
            SinkPadSnapshot {
                receiver_name: String::new(),
                input_id: "input1".into(),
                label: String::new(),
                description: String::new(),
                negotiated_channels: 2,
            },
        ];
        let srcs = vec![
            SrcPadSnapshot {
                sender_name: String::new(),
                output_id: "output0".into(),
                label: String::new(),
                description: String::new(),
                negotiated_channels: 2,
                active_map: None,
            },
            SrcPadSnapshot {
                sender_name: String::new(),
                output_id: "output1".into(),
                label: String::new(),
                description: String::new(),
                negotiated_channels: 2,
                active_map: None,
            },
        ];
        let topology = FrozenTopology::from_snapshots(
            &sinks,
            &srcs,
            &["input0".into(), "input1".into()],
            &["output0".into(), "output1".into()],
        );
        (topology, sinks, srcs)
    }

    #[test]
    fn audiomixmatrix_accepts_dual_src_identity_matrices() {
        use crate::channel_mapping::active_map::default_identity_routes;
        init_gst();
        use gstreamer::prelude::*;
        let (topology, sinks, srcs) = dual_src_topology();
        let src_count = srcs.len();
        for src_idx in 0..src_count {
            let out_ch = topology.output_channels[src_idx];
            let routes = default_identity_routes(&topology, src_idx, out_ch, &sinks, src_count);
            let mix = build_output_mix_matrix(&topology, out_ch, &routes).unwrap();
            let el = gstreamer::ElementFactory::make("audiomixmatrix")
                .property("in-channels", topology.input_bus_channels)
                .property("out-channels", out_ch)
                .build()
                .expect("audiomixmatrix");
            el.set_property("matrix", matrix_to_audiomixmatrix_gvalue(&mix));
        }
    }
}
