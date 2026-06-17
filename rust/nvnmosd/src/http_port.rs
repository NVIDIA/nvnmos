// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! HTTP port allocation helpers for Node creation.
//!
//! When `NodeConfig.http_port` is `0`, the daemon scans
//! [`PortRange`] and picks the first port that is neither registered to
//! another Node nor bound on the host (bind-only probe without
//! `SO_REUSEADDR`, matching wildcard listen semantics). Explicit non-zero
//! ports are validated the same way before create.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddrV4};

use socket2::{Domain, Socket, Type};

use crate::env_config;

/// Default minimum port. Aligns with `nvnmosd-bench --base-http-port` /
/// `NVNMOSD_BENCH_BASE_PORT`.
pub(crate) const DEFAULT_HTTP_PORT_MIN: u16 = 18_080;
pub(crate) const DEFAULT_HTTP_PORT_MAX: u16 = 18_099;

const ENV_HTTP_PORT_MIN: &str = "NVNMOSD_HTTP_PORT_MIN";
const ENV_HTTP_PORT_MAX: &str = "NVNMOSD_HTTP_PORT_MAX";

/// Inclusive TCP port range used when clients request auto-allocation
/// (`http_port = 0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PortRange {
    min: u16,
    max: u16,
}

impl PortRange {
    pub(crate) fn new(min: u16, max: u16) -> Result<Self, String> {
        if min == 0 || max == 0 {
            return Err("port range endpoints must be non-zero".to_owned());
        }
        if min > max {
            return Err(format!("http_port min {min} is greater than max {max}"));
        }
        Ok(Self { min, max })
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = u16> {
        self.min..=self.max
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.min, self.max)
    }
}

/// Read `NVNMOSD_HTTP_PORT_MIN` / `NVNMOSD_HTTP_PORT_MAX` (inclusive).
/// Invalid individual values fall back to their defaults; if the pair is
/// still invalid (`min > max`), both defaults are used and a warning is
/// logged.
pub(crate) fn read_http_port_range() -> PortRange {
    let min = env_config::read_tcp_port(ENV_HTTP_PORT_MIN, DEFAULT_HTTP_PORT_MIN);
    let max = env_config::read_tcp_port(ENV_HTTP_PORT_MAX, DEFAULT_HTTP_PORT_MAX);
    match PortRange::new(min, max) {
        Ok(range) => range,
        Err(reason) => {
            tracing::warn!(
                min,
                max,
                reason,
                default_min = DEFAULT_HTTP_PORT_MIN,
                default_max = DEFAULT_HTTP_PORT_MAX,
                "invalid HTTP port range; using defaults"
            );
            PortRange::new(DEFAULT_HTTP_PORT_MIN, DEFAULT_HTTP_PORT_MAX)
                .expect("default port range is valid")
        }
    }
}

/// Returns `true` when a TCP bind to `0.0.0.0:port` would succeed right
/// now. Does not set `SO_REUSEADDR` and does not call `listen()`.
pub(crate) fn is_tcp_port_bindable(port: u16) -> bool {
    let Ok(socket) = Socket::new(Domain::IPV4, Type::STREAM, None) else {
        return false;
    };
    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    socket.bind(&addr.into()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn port_range_iterates_inclusive_span() {
        let range = PortRange::new(18_080, 18_099).expect("range");
        assert_eq!(range.iter().count(), 20);
    }

    #[test]
    fn port_range_rejects_min_greater_than_max() {
        assert!(PortRange::new(18_100, 18_080).is_err());
    }

    #[test]
    fn bind_probe_detects_bound_port() {
        let listener = TcpListener::bind("0.0.0.0:0").expect("bind ephemeral");
        let port = listener.local_addr().expect("local_addr").port();
        assert!(
            !is_tcp_port_bindable(port),
            "port should be busy while listener is open"
        );
    }

    #[test]
    fn bind_probe_succeeds_after_listener_dropped() {
        let port = {
            let listener = TcpListener::bind("0.0.0.0:0").expect("bind ephemeral");
            listener.local_addr().expect("local_addr").port()
        };
        assert!(is_tcp_port_bindable(port));
    }
}
