// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! IP-address to local network-interface resolution for `udpsrc` /
//! `udpsink`'s `multicast-iface` property and SDP `a=x-nvnmos-iface:`.
//!
//! NMOS carries the receiver/sender's preferred interface as an IP
//! address (the IS-04 `interface_bindings`-resolved address that ends
//! up on `transport_params[].interface_ip`). GStreamer's `udpsrc` and
//! `udpsink` expose `multicast-iface` as an **interface name**
//! (`eth0`, `enp1s0`, …) because that's what the kernel ultimately
//! wants for `IP_MULTICAST_IF` / `MCAST_JOIN_GROUP` — the address-keyed
//! variants of those socket options don't exist on Linux.
//!
//! libnvnmos additionally accepts media-level `a=x-nvnmos-iface:
//! <name> <port-id>` (IS-04 interface identity) when the daemon cannot
//! map `a=x-nvnmos-iface-ip:` through its host interface list.
//!
//! Wraps POSIX `getifaddrs(3)` via the [`nix`] crate. `None` when the
//! address isn't bound locally — callers treat that as "leave
//! `multicast-iface` unset" / omit or clear `x-nvnmos-iface` in SDP.
//!
//! [`iface_name_for_ip`] and [`iface_identity_for_ip`] are used from
//! [`crate::inner::build_udpsink`], [`crate::inner::build_udpsrc`],
//! [`crate::sdp::build_sdp`], and [`crate::sdp_passthrough`].

#[cfg(unix)]
use std::net::IpAddr;

#[cfg(unix)]
use nix::{ifaddrs::getifaddrs, sys::socket::SockaddrStorage};

/// Local interface identity for SDP `a=x-nvnmos-iface:` (two-token form).
#[cfg(unix)]
pub(crate) struct IfaceIdentity {
    pub name: String,
    /// IS-04 `port_id`: lowercase hyphenated MAC (`aa-bb-cc-dd-ee-ff`).
    pub port_id: String,
}

#[cfg(unix)]
impl IfaceIdentity {
    /// SDP attribute value: `<name> <port-id>`.
    pub(crate) fn to_sdp_value(&self) -> String {
        format!("{} {}", self.name, self.port_id)
    }
}

/// Resolve an IPv4 or IPv6 host address to the local interface name it
/// is bound on, suitable for setting on `udpsrc.multicast-iface` /
/// `udpsink.multicast-iface`.
#[cfg(unix)]
pub(crate) fn iface_name_for_ip(target: IpAddr) -> Option<String> {
    let addrs: Vec<_> = getifaddrs().ok()?.collect();
    addrs
        .iter()
        .find(|ifa| matches_address(ifa.address.as_ref(), target))
        .map(|ifa| ifa.interface_name.clone())
}

/// Resolve a host address to local interface name and IS-04 MAC
/// `port_id` for `a=x-nvnmos-iface:`.
#[cfg(unix)]
pub(crate) fn iface_identity_for_ip(target: IpAddr) -> Option<IfaceIdentity> {
    let addrs: Vec<_> = getifaddrs().ok()?.collect();
    let ifname = addrs
        .iter()
        .find(|ifa| matches_address(ifa.address.as_ref(), target))
        .map(|ifa| ifa.interface_name.as_str())?;
    let port_id = port_id_for_interface(&addrs, ifname)?;
    Some(IfaceIdentity {
        name: ifname.to_owned(),
        port_id,
    })
}

/// SDP `a=x-nvnmos-iface:` value for a local NIC IP, when resolvable.
pub(crate) fn x_nvnmos_iface_value_for_ip(ip: &str) -> Option<String> {
    #[cfg(unix)]
    {
        let addr = ip.parse().ok()?;
        iface_identity_for_ip(addr).map(|id| id.to_sdp_value())
    }
    #[cfg(not(unix))]
    {
        let _ = ip;
        None
    }
}

#[cfg(not(unix))]
pub(crate) fn iface_name_for_ip(_target: std::net::IpAddr) -> Option<String> {
    None
}

#[cfg(unix)]
fn port_id_for_interface(
    addrs: &[nix::ifaddrs::InterfaceAddress],
    ifname: &str,
) -> Option<String> {
    for ifa in addrs {
        if ifa.interface_name != ifname {
            continue;
        }
        let Some(sa) = ifa.address.as_ref() else {
            continue;
        };
        let Some(mac) = sa.as_link_addr().and_then(|dl| dl.addr()) else {
            continue;
        };
        if mac.iter().all(|&b| b == 0) {
            continue;
        }
        return Some(format_is04_mac(&mac));
    }
    None
}

#[cfg(unix)]
fn format_is04_mac(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}-{:02x}-{:02x}-{:02x}-{:02x}-{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

#[cfg(unix)]
fn matches_address(sa: Option<&SockaddrStorage>, target: IpAddr) -> bool {
    let Some(sa) = sa else {
        return false;
    };
    match target {
        IpAddr::V4(v4) => sa.as_sockaddr_in().is_some_and(|s| s.ip() == v4),
        IpAddr::V6(v6) => sa.as_sockaddr_in6().is_some_and(|s| s.ip() == v6),
    }
}

/// Host IPv4 for tests that need a resolvable `x-nvnmos-iface:` (skipped when none).
#[cfg(all(test, unix))]
pub(crate) fn test_first_non_loopback_ipv4() -> Option<IpAddr> {
    let addrs: Vec<_> = getifaddrs().ok()?.collect();
    for ifa in addrs {
        if ifa.interface_name == "lo" {
            continue;
        }
        let IpAddr::V4(v4) = ifa
            .address
            .as_ref()
            .and_then(|sa| sa.as_sockaddr_in().map(|s| IpAddr::V4(s.ip())))?
        else {
            continue;
        };
        if !v4.is_loopback() {
            return Some(IpAddr::V4(v4));
        }
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn loopback_v4_resolves_to_a_real_interface_name() {
        let name = iface_name_for_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .expect("127.0.0.1 must be bound on a local interface in any sane host");
        assert!(
            !name.is_empty(),
            "interface name for loopback must be a non-empty string (`lo` on Linux, `lo0` on macOS)",
        );
    }

    #[test]
    fn loopback_v6_resolves_to_a_real_interface_name() {
        let Some(name) = iface_name_for_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)) else {
            return;
        };
        assert!(!name.is_empty(), "`::1` resolution must yield a non-empty name");
    }

    #[test]
    fn unknown_address_returns_none() {
        let unknown = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 254));
        assert_eq!(
            iface_name_for_ip(unknown),
            None,
            "addresses outside the host's interface list must resolve to None",
        );
    }

    #[test]
    fn first_non_loopback_ipv4_yields_iface_identity_when_mac_available() {
        let Some(ip) = test_first_non_loopback_ipv4() else {
            return;
        };
        let identity = iface_identity_for_ip(ip)
            .expect("first non-loopback IPv4 should resolve name + port_id on typical hosts");
        assert!(!identity.name.is_empty());
        assert!(
            identity.port_id.contains('-') && !identity.port_id.contains(':'),
            "port_id must be hyphenated IS-04 form: {}",
            identity.port_id
        );
        let sdp = x_nvnmos_iface_value_for_ip(&ip.to_string()).expect("value");
        assert_eq!(sdp, identity.to_sdp_value());
    }
}
