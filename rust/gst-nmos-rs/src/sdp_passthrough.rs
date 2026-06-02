// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! In-place SDP mutation for transport-file passthrough fidelity.
//!
//! When the user supplies a transport file, property overrides are applied
//! directly on the parsed `SDPMessage` tree rather than round-tripping
//! through [`crate::sdp::parse_sdp`] / [`crate::sdp::build_sdp`], so
//! vendor and spec-extension attributes the model does not represent survive
//! verbatim to `libnvnmos`.

use gstreamer_sdp::{SDPAttribute, SDPMediaRef, SDPConnection, SDPMessage};

use crate::iface;
use crate::sdp::{defaults, SdpError, SdpOverrides};
use crate::types::CapsMode;

/// Reject SDP shapes this stack does not support on the passthrough path.
///
/// Single-`m=` SDPs pass. Two `m=` blocks are reserved for future ST 2022-7
/// support and are rejected with [`SdpError::MultipleMedia`] until dual-leg
/// handling lands. Video+audio (or any mixed `m=` media type) in one SDP is
/// rejected with [`SdpError::MultiMediaMixedEssence`]. Three or more `m=`
/// blocks yield [`SdpError::TooManyMediaBlocks`].
pub(crate) fn reject_unsupported_multi_media(msg: &SDPMessage) -> Result<(), SdpError> {
    let num_medias = msg.medias_len() as usize;
    if num_medias == 0 {
        return Err(SdpError::NoMedia);
    }
    if num_medias > 2 {
        return Err(SdpError::TooManyMediaBlocks(num_medias));
    }
    if num_medias == 2 {
        let first = msg.media(0).and_then(|m| m.media()).unwrap_or("");
        let second = msg.media(1).and_then(|m| m.media()).unwrap_or("");
        if !first.eq_ignore_ascii_case(second) {
            return Err(SdpError::MultiMediaMixedEssence);
        }
        return Err(SdpError::MultipleMedia(2));
    }
    Ok(())
}

/// Apply session-level [`SdpOverrides`] in place (`s=`, `i=`, `a=x-nvnmos-name`).
pub(crate) fn apply_session_overrides_in_place(
    msg: &mut SDPMessage,
    overrides: &SdpOverrides<'_>,
) -> Result<(), SdpError> {
    if let Some(label) = overrides.label {
        msg.set_session_name(label);
    }
    if let Some(description) = overrides.description {
        msg.set_information(description);
    }
    if let Some(name) = overrides.name {
        upsert_session_attribute(msg, "x-nvnmos-name", Some(name));
    }
    Ok(())
}

/// Apply media-level [`SdpOverrides`] to every `m=` block in `msg`.
pub(crate) fn apply_media_overrides_in_place(
    msg: &mut SDPMessage,
    overrides: &SdpOverrides<'_>,
) -> Result<(), SdpError> {
    let num_medias = msg.medias_len();
    for idx in 0..num_medias {
        let Some(m) = msg.media_mut(idx) else {
            continue;
        };
        apply_media_overrides_on_leg(m, overrides)?;
    }
    Ok(())
}

/// Parse an SDP transport file, apply [`SdpOverrides`] in place, and
/// serialise the result without round-tripping through [`crate::sdp::UdpMedia`].
pub(crate) fn passthrough_with_overrides(
    text: &str,
    overrides: &SdpOverrides<'_>,
) -> Result<String, SdpError> {
    let mut msg = SDPMessage::parse_buffer(text.as_bytes())
        .map_err(|e| SdpError::Parse(e.to_string()))?;
    reject_unsupported_multi_media(&msg)?;
    apply_session_overrides_in_place(&mut msg, overrides)?;
    apply_media_overrides_in_place(&mut msg, overrides)?;
    msg.as_text()
        .map_err(|e| SdpError::Serialise(e.to_string()))
}

fn apply_media_overrides_on_leg(
    m: &mut SDPMediaRef,
    overrides: &SdpOverrides<'_>,
) -> Result<(), SdpError> {
    let is_audio = m.media().is_some_and(|kind| kind.eq_ignore_ascii_case("audio"));

    if let Some(port) = overrides.destination_port {
        let num_ports = m.num_ports();
        m.set_port_info(u32::from(port), num_ports);
    }

    let current_dest = connection_address(m);
    if let Some(dest) = overrides.destination_ip {
        if current_dest.as_deref() != Some(dest) {
            replace_connection_address(m, dest)?;
            update_source_filter_destination(m, dest);
        }
    }

    if let Some(ip) = overrides.interface_ip {
        upsert_media_attribute(m, "x-nvnmos-iface-ip", Some(ip), false);
        sync_x_nvnmos_iface_for_local_ip(m, ip);
    }
    if let Some(port) = overrides.source_port {
        upsert_media_attribute(m, "x-nvnmos-src-port", Some(&port.to_string()), false);
    }
    if let Some(src) = overrides.source_ip {
        let dest = overrides
            .destination_ip
            .map(str::to_owned)
            .or_else(|| connection_address(m))
            .unwrap_or_default();
        let value = format!(" incl IN IP4 {dest} {src}");
        upsert_media_attribute(m, "source-filter", Some(&value), false);
    }

    if let Some(pt) = overrides.payload_type {
        if !(96..=127).contains(&pt) {
            return Err(SdpError::InvalidPayloadType(u32::from(pt)));
        }
        rewrite_payload_type(m, pt)?;
    }

    if is_audio {
        if let Some(rate) = overrides.audio_clock_rate {
            rewrite_audio_clock_rate(m, rate)?;
        }
        if let Some(ptime) = overrides.a_ptime {
            upsert_media_attribute(m, "ptime", Some(ptime), false);
        }
        if let Some(maxptime) = overrides.a_maxptime {
            upsert_media_attribute(m, "maxptime", Some(maxptime), false);
        }
    }

    apply_caps_mode_override(m, overrides.caps_mode);

    Ok(())
}

fn apply_caps_mode_override(m: &mut SDPMediaRef, caps_mode: CapsMode) {
    match caps_mode {
        CapsMode::Auto => {}
        CapsMode::Narrow => {
            remove_media_attributes_by_key(m, "x-nvnmos-caps");
        }
        CapsMode::Wide => {
            if m.attribute_val("x-nvnmos-caps").is_none() {
                let pt = m.format(0).unwrap_or("96").to_owned();
                upsert_media_attribute(m, "x-nvnmos-caps", Some(&pt), false);
            }
        }
    }
}

fn rewrite_payload_type(m: &mut SDPMediaRef, new_pt: u8) -> Result<(), SdpError> {
    let old_pt = m.format(0).ok_or(SdpError::MissingPt)?.to_owned();
    let new_pt_str = new_pt.to_string();
    if old_pt == new_pt_str {
        return Ok(());
    }
    m.replace_format(0, &new_pt_str)
        .map_err(|e| SdpError::Parse(e.to_string()))?;

    let old_prefix = format!("{old_pt} ");
    let mut rewrites: Vec<(u32, String, String)> = Vec::new();
    for idx in 0..m.attributes_len() {
        let Some(attr) = m.attribute(idx) else { continue };
        let key = attr.key().to_owned();
        let Some(value) = attr.value() else { continue };
        if let Some(new_value) = match key.as_str() {
            "rtpmap" | "fmtp" if value.starts_with(&old_prefix) => Some(format!(
                "{new_pt_str} {}",
                &value[old_prefix.len()..]
            )),
            "x-nvnmos-caps" if value == old_pt => Some(new_pt_str.clone()),
            _ => None,
        } {
            rewrites.push((idx, key, new_value));
        }
    }
    for (idx, key, new_value) in rewrites {
        upsert_media_attribute_at(m, idx, &key, Some(&new_value), true);
    }
    Ok(())
}

fn rewrite_audio_clock_rate(m: &mut SDPMediaRef, new_rate: u32) -> Result<(), SdpError> {
    for idx in 0..m.attributes_len() {
        let Some(attr) = m.attribute(idx) else { continue };
        if attr.key() != "rtpmap" {
            continue;
        }
        let Some(value) = attr.value() else { continue };
        let Some(new_value) = rewrite_rtpmap_clock_rate(value, new_rate) else {
            continue;
        };
        upsert_media_attribute_at(m, idx, "rtpmap", Some(&new_value), true);
        return Ok(());
    }
    Ok(())
}

fn rewrite_rtpmap_clock_rate(value: &str, new_rate: u32) -> Option<String> {
    let (pt, rest) = value.split_once(' ')?;
    let (encoding, after_slash) = rest.split_once('/')?;
    let (old_rate, channels) = after_slash.split_once('/')?;
    if old_rate.parse::<u32>().ok()? == new_rate {
        return None;
    }
    Some(format!("{pt} {encoding}/{new_rate}/{channels}"))
}

fn connection_address(m: &SDPMediaRef) -> Option<String> {
    m.connection(0)
        .and_then(|c| c.address())
        .map(strip_address_ttl_suffix)
}

fn strip_address_ttl_suffix(address: &str) -> String {
    address.split('/').next().unwrap_or(address).to_owned()
}

fn replace_connection_address(m: &mut SDPMediaRef, address: &str) -> Result<(), SdpError> {
    let conn = m.connection(0).ok_or(SdpError::MissingConnection)?;
    let nettype = conn.nettype().unwrap_or("IN");
    let addrtype = conn.addrtype().unwrap_or("IP4");
    let ttl = if is_multicast_address(address) {
        defaults::MULTICAST_TTL
    } else {
        conn.ttl()
    };
    let new_conn = SDPConnection::new(
        nettype,
        addrtype,
        address,
        ttl,
        conn.addr_number(),
    );
    m.replace_connection(0, new_conn)
        .map_err(|e| SdpError::Parse(e.to_string()))
}

fn is_multicast_address(address: &str) -> bool {
    address
        .split('.')
        .next()
        .and_then(|s| s.parse::<u8>().ok())
        .is_some_and(|o| (224..=239).contains(&o))
}

fn update_source_filter_destination(m: &mut SDPMediaRef, dest: &str) {
    for idx in 0..m.attributes_len() {
        let Some(attr) = m.attribute(idx) else { continue };
        if attr.key() != "source-filter" {
            continue;
        }
        let Some(value) = attr.value() else { continue };
        let Some(new_value) = rewrite_source_filter_destination(value, dest) else {
            continue;
        };
        upsert_media_attribute_at(m, idx, "source-filter", Some(&new_value), false);
        return;
    }
}

fn rewrite_source_filter_destination(value: &str, dest: &str) -> Option<String> {
    let mut parts = value.split_whitespace();
    let mode = parts.next()?;
    let nettype = parts.next()?;
    let addrtype = parts.next()?;
    let _old_dest = parts.next()?;
    let source = parts.collect::<Vec<_>>().join(" ");
    if source.is_empty() {
        return None;
    }
    Some(format!(" {mode} {nettype} {addrtype} {dest} {source}"))
}

fn upsert_session_attribute(msg: &mut SDPMessage, key: &str, value: Option<&str>) {
    if let Some(idx) = find_session_attribute_index(msg, key) {
        let _ = msg.replace_attribute(idx, SDPAttribute::new(key, value));
    } else {
        msg.add_attribute(key, value);
    }
}

fn find_session_attribute_index(msg: &SDPMessage, key: &str) -> Option<u32> {
    (0..msg.attributes_len()).find(|&idx| {
        msg.attribute(idx)
            .is_some_and(|attr| attr.key() == key)
    })
}

fn upsert_media_attribute(
    m: &mut SDPMediaRef,
    key: &str,
    value: Option<&str>,
    canonicalise: bool,
) {
    if let Some(idx) = find_media_attribute_index(m, key) {
        upsert_media_attribute_at(m, idx, key, value, canonicalise);
    } else {
        m.add_attribute(key, value);
        if canonicalise {
            let idx = m.attributes_len().saturating_sub(1);
            crate::sdp::canonicalise_media_attribute_at(m, idx);
        }
    }
}

fn upsert_media_attribute_at(
    m: &mut SDPMediaRef,
    idx: u32,
    key: &str,
    value: Option<&str>,
    canonicalise: bool,
) {
    let _ = m.replace_attribute(idx, SDPAttribute::new(key, value));
    if canonicalise {
        crate::sdp::canonicalise_media_attribute_at(m, idx);
    }
}

fn find_media_attribute_index(m: &SDPMediaRef, key: &str) -> Option<u32> {
    (0..m.attributes_len()).find(|&idx| {
        m.attribute(idx).is_some_and(|attr| attr.key() == key)
    })
}

fn remove_media_attributes_by_key(m: &mut SDPMediaRef, key: &str) {
    let mut idx = 0;
    while idx < m.attributes_len() {
        if m.attribute(idx).is_some_and(|attr| attr.key() == key) {
            let _ = m.remove_attribute(idx);
        } else {
            idx += 1;
        }
    }
}

/// Keep `a=x-nvnmos-iface:` aligned with an overridden local NIC IP.
fn sync_x_nvnmos_iface_for_local_ip(m: &mut SDPMediaRef, ip: &str) {
    if let Some(value) = iface::x_nvnmos_iface_value_for_ip(ip) {
        upsert_media_attribute(m, "x-nvnmos-iface", Some(&value), false);
    } else {
        remove_media_attributes_by_key(m, "x-nvnmos-iface");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIDEO_WITH_STALE_IFACE: &str = concat!(
        "v=0\r\n",
        "o=- 1 0 IN IP4 192.0.2.10\r\n",
        "s=Example\r\n",
        "t=0 0\r\n",
        "m=video 5008 RTP/AVP 96\r\n",
        "c=IN IP4 239.1.1.1/64\r\n",
        "a=x-nvnmos-iface:example-net1 00-00-5e-00-53-00\r\n",
        "a=x-nvnmos-iface-ip:192.0.2.11\r\n",
        "a=rtpmap:96 raw/90000\r\n",
    );

    #[test]
    #[cfg(unix)]
    fn interface_ip_override_replaces_stale_x_nvnmos_iface_when_resolvable() {
        let Some(ip) = iface::test_first_non_loopback_ipv4() else {
            return;
        };
        let ip_str = ip.to_string();
        let overrides = SdpOverrides {
            interface_ip: Some(&ip_str),
            ..Default::default()
        };
        let out = passthrough_with_overrides(VIDEO_WITH_STALE_IFACE, &overrides).expect("splice");
        assert!(
            out.contains(&format!("a=x-nvnmos-iface-ip:{ip}")),
            "iface-ip must be overridden: {out}",
        );
        assert!(
            !out.contains("example-net1"),
            "stale iface name must be removed or replaced: {out}",
        );
        let expected_iface = iface::x_nvnmos_iface_value_for_ip(&ip_str)
            .expect("test IP must resolve to x-nvnmos-iface on this host");
        assert!(
            out.contains(&format!("a=x-nvnmos-iface:{expected_iface}")),
            "iface must match resolved identity: {out}",
        );
    }

    #[test]
    fn interface_ip_override_to_unbound_ip_clears_x_nvnmos_iface() {
        let overrides = SdpOverrides {
            interface_ip: Some("192.0.2.254"),
            ..Default::default()
        };
        let out =
            passthrough_with_overrides(VIDEO_WITH_STALE_IFACE, &overrides).expect("splice");
        assert!(
            out.contains("a=x-nvnmos-iface-ip:192.0.2.254"),
            "iface-ip override: {out}",
        );
        assert!(
            !out.contains("x-nvnmos-iface:"),
            "unresolvable override must drop stale iface: {out}",
        );
    }

}
