// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! IP-address to network-interface-name resolution for `udpsrc` /
//! `udpsink`'s `multicast-iface` property.
//!
//! NMOS carries the receiver/sender's preferred interface as an IP
//! address (the IS-04 `interface_bindings`-resolved address that ends
//! up on `transport_params[].interface_ip`). GStreamer's `udpsrc` and
//! `udpsink` expose `multicast-iface` as an **interface name**
//! (`eth0`, `enp1s0`, …) because that's what the kernel ultimately
//! wants for `IP_MULTICAST_IF` / `MCAST_JOIN_GROUP` — the address-keyed
//! variants of those socket options don't exist on Linux.
//!
//! Wraps POSIX `getifaddrs(3)` via the [`nix`] crate, walks the linked
//! list of `struct ifaddrs`, downcasts each entry's `address` to either
//! `sockaddr_in` or `sockaddr_in6`, and returns the first matching
//! interface name. `None` if the address isn't bound on any local
//! interface (operator misconfiguration, or the SDP advertises an IP
//! that lives on a different host) — callers treat that as "leave
//! `multicast-iface` unset and let the kernel's default-route pick the
//! NIC", which is the right fallback on single-NIC hosts and the only
//! safe answer when we can't prove which NIC the user intended.
//!
//! This helper is called from [`crate::inner::build_udpsink`] and
//! [`crate::inner::build_udpsrc`] only when the **destination** is a
//! multicast address — for unicast there's nothing to pin and
//! `bind-address` already handles source-IP selection.

#[cfg(unix)]
use std::net::IpAddr;

#[cfg(unix)]
use nix::{ifaddrs::getifaddrs, sys::socket::SockaddrStorage};

/// Resolve an IPv4 or IPv6 host address to the local interface name it
/// is bound on, suitable for setting on `udpsrc.multicast-iface` /
/// `udpsink.multicast-iface`.
///
/// Returns `None` when the address isn't bound on any local interface
/// (or when `getifaddrs` itself fails — the only documented failure
/// mode is `ENOMEM`, which is essentially "the host is on fire" and
/// not something this layer should try to recover from). On
/// non-Unix targets this is a stub that always returns `None`; the
/// caller silently falls back to "don't set `multicast-iface`", which
/// matches today's behaviour and lets the kernel's default-route
/// picker decide.
#[cfg(unix)]
pub(crate) fn iface_name_for_ip(target: IpAddr) -> Option<String> {
    let addrs = getifaddrs().ok()?;
    for ifa in addrs {
        if matches_address(ifa.address.as_ref(), target) {
            return Some(ifa.interface_name);
        }
    }
    None
}

#[cfg(not(unix))]
pub(crate) fn iface_name_for_ip(_target: std::net::IpAddr) -> Option<String> {
    None
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
            // CI containers / minimal hosts sometimes have IPv6 disabled
            // entirely; treat that as a soft skip rather than a failure.
            // The helper is only invoked for multicast destinations
            // anyway, and v6 multicast isn't on the near-term roadmap.
            return;
        };
        assert!(!name.is_empty(), "`::1` resolution must yield a non-empty name");
    }

    #[test]
    fn unknown_address_returns_none() {
        // TEST-NET-1 (RFC 5737) — never assignable to a real interface,
        // safe to assume not bound locally.
        let unknown = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 254));
        assert_eq!(
            iface_name_for_ip(unknown),
            None,
            "addresses outside the host's interface list must resolve to None",
        );
    }
}
