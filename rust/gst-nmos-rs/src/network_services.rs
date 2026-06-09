// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! NMOS network-services URL parsing and `NodeConfig` assembly for
//! `OpenSession`. Mirrors the `parse_nmos_url` / `NvNmosNetworkServicesConfig`
//! path in `nvds_nmos_bin`.

use gstreamer as gst;
use nvnmos_rpc::v1::{NetworkServicesConfig, NodeConfig};
use regex::Regex;
use url::{Host, Url};

use crate::session::CommonSettings;
use crate::CAT;

/// Parsed pieces of an NMOS Registration or System API URL:
/// `http://host[:port]/x-nmos/{registration,system}/v<X.Y>[/]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct NmosUrlParts {
    pub(crate) address: String,
    /// TCP port from the URL (explicit, or the `http` scheme default).
    pub(crate) port: u32,
    pub(crate) version: String,
    pub(crate) valid: bool,
}

/// Parse an NMOS Registration or System API URL. Invalid URLs return
/// `valid = false` (callers should log and skip, matching nvds).
pub(crate) fn parse_nmos_url(url_str: &str, service_type: &str) -> NmosUrlParts {
    let mut parts = NmosUrlParts::default();
    if url_str.is_empty() {
        return parts;
    }

    let parsed = match Url::parse(url_str) {
        Ok(url) => url,
        Err(_) => {
            parts.valid = false;
            return parts;
        }
    };

    if parsed.scheme() != "http" {
        parts.valid = false;
        return parts;
    }

    parts.address = match parsed.host() {
        Some(Host::Domain(domain)) if !domain.is_empty() => domain.to_owned(),
        Some(Host::Ipv4(addr)) => addr.to_string(),
        _ => {
            parts.valid = false;
            return parts;
        }
    };

    // `port_or_known_default()` supplies 80 when the authority omits `:port`.
    parts.port = u32::from(parsed.port_or_known_default().unwrap_or(80));

    parse_version_from_path(parsed.path(), service_type, &mut parts.version);

    parts.valid = true;
    parts
}

fn parse_version_from_path(path: &str, service_type: &str, version: &mut String) {
    // Same path/version pattern as nvds_nmos_bin::parse_nmos_url.
    let pattern = format!(r"^/x-nmos/{service_type}/(v[0-9]+\.[0-9]+)/?$");
    let re = Regex::new(&pattern).expect("NMOS version path pattern is valid");
    if let Some(caps) = re.captures(path) {
        *version = caps[1].to_owned();
    }
}

/// Build the `NodeConfig` handed to `OpenSession` from a settings snapshot.
/// Invalid `registration-url` / `system-url` values are logged and omitted;
/// other fields are still forwarded.
pub(crate) fn node_config_from_settings(settings: &CommonSettings) -> NodeConfig {
    let network_services = build_network_services(settings);
    NodeConfig {
        seed: settings.node_seed.clone(),
        host_name: settings.host_name.clone(),
        http_port: u32::from(settings.http_port),
        network_services,
        ..NodeConfig::default()
    }
}

fn build_network_services(settings: &CommonSettings) -> Option<NetworkServicesConfig> {
    let mut has_network_services = false;
    let mut network_services = NetworkServicesConfig::default();

    if !settings.domain.is_empty() {
        network_services.domain = settings.domain.clone();
        has_network_services = true;
    }

    if !settings.registration_url.is_empty() {
        let reg_parts = parse_nmos_url(&settings.registration_url, "registration");
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
                settings.registration_url
            );
        }
    }

    if !settings.system_url.is_empty() {
        let sys_parts = parse_nmos_url(&settings.system_url, "system");
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
                settings.system_url
            );
        }
    }

    if has_network_services {
        Some(network_services)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_registration_url_with_port_and_version() {
        let parts = parse_nmos_url(
            "http://reg.example:8080/x-nmos/registration/v1.3/",
            "registration",
        );
        assert!(parts.valid);
        assert_eq!(parts.address, "reg.example");
        assert_eq!(parts.port, 8080);
        assert_eq!(parts.version, "v1.3");
    }

    #[test]
    fn parse_registration_url_without_port_or_version() {
        let parts = parse_nmos_url("http://reg.example/x-nmos/registration/", "registration");
        assert!(parts.valid);
        assert_eq!(parts.address, "reg.example");
        assert_eq!(parts.port, 80);
        assert_eq!(parts.version, "");
    }

    #[test]
    fn parse_system_url() {
        let parts = parse_nmos_url("http://sys.local/x-nmos/system/v1.0", "system");
        assert!(parts.valid);
        assert_eq!(parts.address, "sys.local");
        assert_eq!(parts.port, 80);
        assert_eq!(parts.version, "v1.0");
    }

    #[test]
    fn parse_rejects_ipv6_literal_host() {
        let parts = parse_nmos_url(
            "http://[2001:db8::1]:3210/x-nmos/registration/v1.3",
            "registration",
        );
        assert!(!parts.valid);
    }

    #[test]
    fn parse_rejects_https() {
        let parts = parse_nmos_url("https://reg.example/x-nmos/registration/v1.3", "registration");
        assert!(!parts.valid);
    }

    #[test]
    fn parse_rejects_invalid_port() {
        let parts = parse_nmos_url("http://reg.example:99999/x-nmos/registration/v1.3", "registration");
        assert!(!parts.valid);
    }

    #[test]
    fn node_config_forwards_host_name_and_domain() {
        let settings = CommonSettings {
            node_seed: "seed-a".to_owned(),
            host_name: "studio-a".to_owned(),
            domain: "local".to_owned(),
            ..CommonSettings::default()
        };
        let config = node_config_from_settings(&settings);
        assert_eq!(config.seed, "seed-a");
        assert_eq!(config.host_name, "studio-a");
        let ns = config.network_services.expect("domain should populate network_services");
        assert_eq!(ns.domain, "local");
    }

    #[test]
    fn node_config_parses_registration_and_system_urls() {
        let settings = CommonSettings {
            node_seed: "seed-b".to_owned(),
            registration_url: "http://reg:3210/x-nmos/registration/v1.3".to_owned(),
            system_url: "http://sys:10641/x-nmos/system/v1.0".to_owned(),
            ..CommonSettings::default()
        };
        let config = node_config_from_settings(&settings);
        let ns = config.network_services.expect("urls should populate network_services");
        assert_eq!(ns.registration_address, "reg");
        assert_eq!(ns.registration_port, 3210);
        assert_eq!(ns.registration_version, "v1.3");
        assert_eq!(ns.system_address, "sys");
        assert_eq!(ns.system_port, 10641);
        assert_eq!(ns.system_version, "v1.0");
    }
}
