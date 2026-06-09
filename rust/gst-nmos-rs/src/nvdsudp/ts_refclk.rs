// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Map SDP `a=ts-refclk:` attributes to `nvdsudpsink` / `nvdsudpsrc` `ptp-src`.
//!
//! Intentionally scans raw SDP text here: [`crate::sdp::parse_sdp`] / [`UdpMedia`]
//! do not model `ts-refclk` yet (passthrough preserves the attribute verbatim).
//! A shared `sdp::` attribute collector can replace this when clock metadata
//! lands in the general SDP path.

/// Whether to enable Rivermax hardware PTP on the local NIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PtpSrcResolution {
    /// Leave `ptp-src` unset (system clock).
    Unset,
    /// SDP declares PTP — set `ptp-src` to `local-iface-ip` / `interface_ip`.
    ///
    /// RFC 7273 / ST 2110-10 `a=ts-refclk:ptp=…` carries clock identity
    /// (traceable, GMID, domain), not a bind address.
    UseInterfaceIp,
}

/// Resolve `ptp-src` from SDP text (activation or configuring transport file).
pub(crate) fn ptp_src_from_sdp(sdp_text: &str) -> PtpSrcResolution {
    let mut saw_ptp = false;

    for value in collect_ts_refclk_values(sdp_text) {
        if value == "local" || value.starts_with("localmac=") {
            return PtpSrcResolution::Unset;
        }
        if value.starts_with("ptp=") {
            saw_ptp = true;
        }
    }

    if saw_ptp {
        PtpSrcResolution::UseInterfaceIp
    } else {
        PtpSrcResolution::Unset
    }
}

fn collect_ts_refclk_values(sdp_text: &str) -> Vec<String> {
    sdp_text
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("a=ts-refclk:")
                .map(str::trim)
                .map(str::to_owned)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ptp_traceable_uses_interface_ip() {
        let sdp = "a=ts-refclk:ptp=IEEE1588-2008:traceable\r\n";
        assert_eq!(ptp_src_from_sdp(sdp), PtpSrcResolution::UseInterfaceIp);
    }

    #[test]
    fn ptp_gmid_uses_interface_ip() {
        let sdp = "a=ts-refclk:ptp=IEEE1588-2008:AC-DE-48-23-45-67-01-9F:42\r\n";
        assert_eq!(ptp_src_from_sdp(sdp), PtpSrcResolution::UseInterfaceIp);
    }

    #[test]
    fn ntp_refclk_does_not_enable_ptp_src() {
        let sdp = "a=ts-refclk:ntp=/traceable/\r\n";
        assert_eq!(ptp_src_from_sdp(sdp), PtpSrcResolution::Unset);
    }

    #[test]
    fn local_refclk_does_not_enable_ptp_src() {
        let sdp = "a=ts-refclk:local\r\n";
        assert_eq!(ptp_src_from_sdp(sdp), PtpSrcResolution::Unset);
    }

    #[test]
    fn localmac_suppresses_ptp_src() {
        let sdp = "a=ts-refclk:localmac=CA-FE-01-CA-FE-02\r\n\
                   a=ts-refclk:ptp=IEEE1588-2008:traceable\r\n";
        assert_eq!(ptp_src_from_sdp(sdp), PtpSrcResolution::Unset);
    }

    #[test]
    fn ptp_before_localmac_still_suppresses_ptp_src() {
        let sdp = "a=ts-refclk:ptp=IEEE1588-2008:traceable\r\n\
                   a=ts-refclk:localmac=CA-FE-01-CA-FE-02\r\n";
        assert_eq!(ptp_src_from_sdp(sdp), PtpSrcResolution::Unset);
    }

    #[test]
    fn absent_ts_refclk_is_unset() {
        assert_eq!(ptp_src_from_sdp("a=mediaclk:direct=0\r\n"), PtpSrcResolution::Unset);
    }

    #[test]
    fn dual_ptp_lines_still_use_interface_ip() {
        let sdp = "a=ts-refclk:ptp=IEEE1588-2008:traceable\r\n\
                   a=ts-refclk:ptp=IEEE1588-2008:AC-DE-48-23-45-67-01-9F:42\r\n";
        assert_eq!(ptp_src_from_sdp(sdp), PtpSrcResolution::UseInterfaceIp);
    }
}
