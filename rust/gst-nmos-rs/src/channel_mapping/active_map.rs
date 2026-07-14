// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Parse per-src `active-map` structures and build default identity maps.

use std::collections::HashSet;

use gstreamer as gst;
use nvnmos_rpc::v1::ActiveMapEntry;
use thiserror::Error;

use super::types::{FrozenTopology, SinkPadSnapshot, SrcPadSnapshot};

/// One routed output channel in an active map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveMapRoute {
    pub(crate) output_channel: u32,
    pub(crate) input_id: String,
    pub(crate) input_channel: u32,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ActiveMapError {
    #[error("active-map field key `{0}` is not a valid output channel index")]
    InvalidOutputChannelKey(String),
    #[error("active-map output channel {0} is duplicated")]
    DuplicateOutputChannel(u32),
    #[error("active-map value for output channel {output_channel}: {reason}")]
    InvalidValue { output_channel: u32, reason: String },
}

/// Parse `inputId:channel_index` (e.g. `input0:2`).
pub(crate) fn parse_input_channel_ref(value: &str) -> Result<(&str, u32), String> {
    let (input_id, ch) = value
        .split_once(':')
        .ok_or_else(|| format!("expected `inputId:channel`, got `{value}`"))?;
    if input_id.is_empty() {
        return Err("input id is empty".to_owned());
    }
    let input_channel = ch
        .parse::<u32>()
        .map_err(|_| format!("channel index `{ch}` is not a non-negative integer"))?;
    Ok((input_id, input_channel))
}

/// Parse a per-src `active-map` `GstStructure` into routes.
pub(crate) fn parse_active_map_structure(
    structure: &gst::Structure,
) -> Result<Vec<ActiveMapRoute>, ActiveMapError> {
    // The structure name is conventional (`map`) but not interpreted,
    // matching the `transport-`/`pay-`/`depay-properties` convention.
    let mut routes = Vec::new();
    let mut seen_outputs = HashSet::new();
    for (key, _value) in structure.iter() {
        let key = key.as_str();
        let output_channel = key
            .parse::<u32>()
            .map_err(|_| ActiveMapError::InvalidOutputChannelKey(key.to_string()))?;
        if !seen_outputs.insert(output_channel) {
            return Err(ActiveMapError::DuplicateOutputChannel(output_channel));
        }
        let value =
            structure
                .get::<Option<String>>(key)
                .map_err(|e| ActiveMapError::InvalidValue {
                    output_channel,
                    reason: format!("field value is not a string: {e}"),
                })?;
        // `active-map` is intended to be sparse, but allow explicit unrouted channels.
        // Support both 'correct', `(string)NULL` GValue (None), and 'incorrect', `NULL` or `null` string.
        let Some(value) = value else { continue };
        if value.eq_ignore_ascii_case("null") {
            continue;
        }
        let (input_id, input_channel) =
            parse_input_channel_ref(&value).map_err(|reason| ActiveMapError::InvalidValue {
                output_channel,
                reason,
            })?;
        routes.push(ActiveMapRoute {
            output_channel,
            input_id: input_id.to_owned(),
            input_channel,
        });
    }
    routes.sort_by_key(|r| r.output_channel);
    Ok(routes)
}

/// Convert routes to dense proto entries (index `i` = output channel `i`).
pub(crate) fn active_map_entries_from_routes(
    output_channels: u32,
    routes: &[ActiveMapRoute],
) -> Vec<ActiveMapEntry> {
    let mut entries = vec![
        ActiveMapEntry {
            input_id: None,
            input_channel: None,
        };
        output_channels as usize
    ];
    for route in routes {
        if route.output_channel >= output_channels {
            continue;
        }
        let slot = &mut entries[route.output_channel as usize];
        slot.input_id = Some(route.input_id.clone());
        slot.input_channel = Some(route.input_channel);
    }
    entries
}

/// Parse `active-map` or return default identity for one src pad.
pub(crate) fn active_map_entries_for_src(
    topology: &FrozenTopology,
    src: &SrcPadSnapshot,
    src_index: usize,
    sinks: &[SinkPadSnapshot],
    src_count: usize,
) -> Result<Vec<ActiveMapEntry>, ActiveMapError> {
    let output_channels = src.negotiated_channels;
    let routes = if let Some(structure) = src.active_map.as_ref() {
        parse_active_map_structure(structure)?
    } else {
        default_identity_routes(topology, src_index, output_channels, sinks, src_count)
    };
    Ok(active_map_entries_from_routes(output_channels, &routes))
}

/// Default identity routes when `active-map` is unset or whole-property NULL.
pub(crate) fn default_identity_routes(
    topology: &FrozenTopology,
    src_index: usize,
    output_channels: u32,
    sinks: &[SinkPadSnapshot],
    src_count: usize,
) -> Vec<ActiveMapRoute> {
    if src_count == 1 {
        let mut routes = Vec::new();
        let mut logical = 0u32;
        for (pad_idx, sink) in sinks.iter().enumerate() {
            let input_id = topology
                .input_ids
                .get(pad_idx)
                .cloned()
                .unwrap_or_else(|| sink.input_id.clone());
            for ch in 0..sink.negotiated_channels {
                if logical >= output_channels {
                    return routes;
                }
                routes.push(ActiveMapRoute {
                    output_channel: logical,
                    input_id: input_id.clone(),
                    input_channel: ch,
                });
                logical += 1;
            }
        }
        return routes;
    }

    let mut routes = Vec::new();
    let input_id = topology
        .input_ids
        .get(src_index)
        .cloned()
        .or_else(|| sinks.get(src_index).map(|s| s.input_id.clone()))
        .unwrap_or_default();
    if input_id.is_empty() {
        return routes;
    }
    let sink_channels = sinks
        .get(src_index)
        .map(|s| s.negotiated_channels)
        .unwrap_or(0);
    let routed = output_channels.min(sink_channels);
    for ch in 0..routed {
        routes.push(ActiveMapRoute {
            output_channel: ch,
            input_id: input_id.clone(),
            input_channel: ch,
        });
    }
    routes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use crate::test_support::init_gst;

    fn map_structure(launch: &str) -> gst::Structure {
        init_gst();
        gst::Structure::from_str(launch).expect("structure parse")
    }

    #[test]
    fn parse_input_channel_ref_valid() {
        assert_eq!(parse_input_channel_ref("input0:2").unwrap(), ("input0", 2));
    }

    #[test]
    fn parse_input_channel_ref_rejects_missing_colon() {
        assert!(parse_input_channel_ref("input0").is_err());
    }

    #[test]
    fn parse_active_map_stereo_identity() {
        let s = map_structure("map,0=input0:0,1=input0:1");
        let routes = parse_active_map_structure(&s).unwrap();
        assert_eq!(
            routes,
            vec![
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
            ]
        );
    }

    #[test]
    fn parse_active_map_ignores_structure_name() {
        // The structure name is conventional (`map`) but not enforced.
        let s = map_structure("not-map,0=input0:0");
        let routes = parse_active_map_structure(&s).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].input_id, "input0");
        assert_eq!(routes[0].input_channel, 0);
    }

    #[test]
    fn parse_active_map_null_gvalue_means_unrouted() {
        let s = map_structure("map,0=(string)NULL,1=input0:1");
        let routes = parse_active_map_structure(&s).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].output_channel, 1);
        assert_eq!(routes[0].input_id, "input0");
    }

    #[test]
    fn parse_active_map_bare_null_string_means_unrouted() {
        let s = map_structure("map,0=null,1=input0:1");
        let routes = parse_active_map_structure(&s).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].output_channel, 1);
        assert_eq!(routes[0].input_id, "input0");
    }

    #[test]
    fn parse_active_map_null_input_id_with_channel_is_routed() {
        let s = map_structure("map,0=null:0");
        let routes = parse_active_map_structure(&s).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].input_id, "null");
        assert_eq!(routes[0].input_channel, 0);
    }

    #[test]
    fn parse_active_map_duplicate_field_keeps_last_value() {
        // GstStructure field names are unique: a launch string with the same key
        // twice is deduplicated by GStreamer before we parse (last assignment wins).
        let s = map_structure("map,0=input0:0,0=input1:0");
        assert_eq!(s.n_fields(), 1);
        assert_eq!(s.get::<String>("0").unwrap(), "input1:0");
        let routes = parse_active_map_structure(&s).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].input_id, "input1");
    }

    #[test]
    fn entries_sparse_unrouted_channels() {
        let s = map_structure("map,2=input1:0");
        let routes = parse_active_map_structure(&s).unwrap();
        let entries = active_map_entries_from_routes(4, &routes);
        assert_eq!(entries.len(), 4);
        assert!(entries[0].input_id.as_ref().is_none_or(|s| s.is_empty()));
        assert!(entries[1].input_id.as_ref().is_none_or(|s| s.is_empty()));
        assert_eq!(entries[2].input_id.as_deref(), Some("input1"));
        assert_eq!(entries[2].input_channel, Some(0));
        assert!(entries[3].input_id.as_ref().is_none_or(|s| s.is_empty()));
    }

    #[test]
    fn default_identity_matched_index_two_by_two() {
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
            output_id: "output1".into(),
            label: String::new(),
            description: String::new(),
            negotiated_channels: 2,
            active_map: None,
        };
        let topology = FrozenTopology::from_snapshots(
            &sinks,
            std::slice::from_ref(&src),
            &["input0".into(), "input1".into()],
            &["output1".into()],
        );
        let routes = default_identity_routes(&topology, 1, 2, &sinks, 2);
        assert_eq!(
            routes,
            vec![
                ActiveMapRoute {
                    output_channel: 0,
                    input_id: "input1".into(),
                    input_channel: 0,
                },
                ActiveMapRoute {
                    output_channel: 1,
                    input_id: "input1".into(),
                    input_channel: 1,
                },
            ]
        );
    }

    #[test]
    fn default_identity_single_src_all_inputs() {
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
        let topology = FrozenTopology::from_snapshots(
            &sinks,
            &[src],
            &["input0".into(), "input1".into()],
            &["output0".into()],
        );
        let routes = default_identity_routes(&topology, 0, 4, &sinks, 1);
        assert_eq!(routes.len(), 4);
        assert_eq!(routes[0].input_id, "input0");
        assert_eq!(routes[0].input_channel, 0);
        assert_eq!(routes[2].input_id, "input1");
        assert_eq!(routes[2].input_channel, 0);
    }
}
