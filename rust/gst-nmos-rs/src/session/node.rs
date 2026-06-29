// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared Node identity / network-services settings for `OpenSession` and
//! `AddNode` (the `NodeConfig` protobuf message).

use gstreamer as gst;
use nvnmos_rpc::v1::{NetworkServicesConfig, NodeConfig};

use crate::network_services::parse_nmos_url;
use crate::CAT;

/// Snapshot of element properties that map to proto `NodeConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NodeSettings {
    pub node_seed: String,
    pub http_port: u16,
    pub host_name: String,
    pub domain: String,
    pub registration_url: String,
    pub system_url: String,
}

impl NodeSettings {
    /// Build the `NodeConfig` for `OpenSession` or `AddNode`.
    ///
    /// Invalid `registration-url` / `system-url` values are logged and omitted;
    /// other fields are still forwarded.
    pub(crate) fn to_node_config(&self) -> NodeConfig {
        NodeConfig {
            seed: self.node_seed.clone(),
            host_name: self.host_name.clone(),
            http_port: u32::from(self.http_port),
            network_services: self.build_network_services(),
            ..NodeConfig::default()
        }
    }

    fn build_network_services(&self) -> Option<NetworkServicesConfig> {
        let mut has_network_services = false;
        let mut network_services = NetworkServicesConfig::default();

        if !self.domain.is_empty() {
            network_services.domain = self.domain.clone();
            has_network_services = true;
        }

        if !self.registration_url.is_empty() {
            let reg_parts = parse_nmos_url(&self.registration_url, "registration");
            if reg_parts.valid {
                network_services.registration_address = reg_parts.address;
                network_services.registration_port = reg_parts.port;
                if !reg_parts.version.is_empty() {
                    network_services.registration_version = reg_parts.version;
                }
                has_network_services = true;
            } else {
                gst::warning!(
                    CAT,
                    "failed to parse registration-url `{}`; property ignored",
                    self.registration_url
                );
            }
        }

        if !self.system_url.is_empty() {
            let sys_parts = parse_nmos_url(&self.system_url, "system");
            if sys_parts.valid {
                network_services.system_address = sys_parts.address;
                network_services.system_port = sys_parts.port;
                if !sys_parts.version.is_empty() {
                    network_services.system_version = sys_parts.version;
                }
                has_network_services = true;
            } else {
                gst::warning!(
                    CAT,
                    "failed to parse system-url `{}`; property ignored",
                    self.system_url
                );
            }
        }

        if has_network_services {
            Some(network_services)
        } else {
            None
        }
    }
}
