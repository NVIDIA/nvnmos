// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! MXL `flow_def` JSON helpers.
//!
//! The IS-05 transport_file for transport=mxl is a JSON document
//! whose top-level `id` is the MXL flow id and whose `format` is the
//! NMOS format URN (`urn:x-nmos:format:video|audio|data`). The
//! element needs both to configure the inner `mxlsink` / `mxlsrc`
//! (`mxlsink.flow-id=` and `mxlsrc.{video,audio,data}-flow-id=`).
//!
//! [`read_flow_def_meta`] parses the JSON into [`FlowDefMeta`].
//! [`resolve_mxl_flow_meta`] combines that with the `mxl-flow-id` /
//! `mxl-flow-format` property overrides, mirroring
//! [`crate::domain::resolve_mxl_domain_id`]:
//!
//! * Property only: use it.
//! * File only: use it.
//! * Both agreeing: use the value (DEBUG log at the caller).
//! * Both disagreeing: hard error.
//! * Neither: empty id / `Unspecified` format — the element will
//!   fall back to its placeholder data path.

use serde::Deserialize;
use thiserror::Error;

use crate::types::FlowFormat;

#[derive(Debug, Deserialize)]
struct RawFlowDef {
    /// MXL flow id (UUID). Required by BCP-007-03 but here we just
    /// surface a typed error if a JSON document omits it.
    id: Option<String>,
    /// NMOS format URN: `urn:x-nmos:format:video|audio|data`.
    /// Optional in the file; the property may supply it instead.
    format: Option<String>,
}

#[derive(Debug, Error)]
pub(crate) enum FlowDefError {
    #[error("failed to parse transport_file as JSON: {source}")]
    Parse {
        #[source]
        source: serde_json::Error,
    },
    #[error("transport_file `format` `{0}` is not a recognised NMOS format URN")]
    UnknownFormat(String),
    #[error(
        "mxl-flow-id mismatch: property `{property}` != transport_file top-level `id` `{file}`"
    )]
    IdMismatch { property: String, file: String },
    #[error(
        "mxl-flow-format mismatch: property `{property:?}` != transport_file `format` `{file:?}`"
    )]
    FormatMismatch {
        property: FlowFormat,
        file: FlowFormat,
    },
}

#[derive(Debug)]
pub(crate) struct FlowDefMeta {
    pub(crate) id: String,
    pub(crate) format: FlowFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueOrigin {
    /// User supplied the property; no transport_file consulted (or
    /// the file did not carry the field).
    Property,
    /// Read from the transport_file; user did not supply a property
    /// override.
    File,
    /// User supplied the property and the transport_file agreed.
    Both,
    /// Neither source supplied a value.
    None,
}

#[derive(Debug)]
pub(crate) struct FlowResolution {
    pub(crate) id: String,
    pub(crate) id_origin: ValueOrigin,
    pub(crate) format: FlowFormat,
    pub(crate) format_origin: ValueOrigin,
}

/// Parse `id` and `format` out of a `flow_def` JSON document.
/// `Ok(None)` is **not** returned — a transport_file we can't parse
/// at all is an error; missing individual fields surface as empty
/// `id` / [`FlowFormat::Unspecified`] in the returned struct.
pub(crate) fn read_flow_def_meta(text: &str) -> Result<FlowDefMeta, FlowDefError> {
    let raw: RawFlowDef =
        serde_json::from_str(text).map_err(|source| FlowDefError::Parse { source })?;
    let id = raw.id.unwrap_or_default();
    let format = match raw.format {
        None => FlowFormat::Unspecified,
        Some(urn) => match FlowFormat::from_format_urn(&urn) {
            FlowFormat::Unspecified => return Err(FlowDefError::UnknownFormat(urn)),
            other => other,
        },
    };
    Ok(FlowDefMeta { id, format })
}

/// Combine the user's `mxl-flow-id` / `mxl-flow-format` properties
/// with `id` / `format` read from the transport_file. See the module
/// docs for the truth table.
pub(crate) fn resolve_mxl_flow_meta(
    property_id: &str,
    property_format: FlowFormat,
    transport_file_text: Option<&str>,
) -> Result<FlowResolution, FlowDefError> {
    let file_meta = match transport_file_text {
        Some(t) if !t.is_empty() => Some(read_flow_def_meta(t)?),
        _ => None,
    };

    let (id, id_origin) = resolve_id(property_id, file_meta.as_ref().map(|m| m.id.as_str()))?;
    let (format, format_origin) = resolve_format(
        property_format,
        file_meta.as_ref().map(|m| m.format).unwrap_or(FlowFormat::Unspecified),
    )?;

    Ok(FlowResolution { id, id_origin, format, format_origin })
}

fn resolve_id(
    property: &str,
    file: Option<&str>,
) -> Result<(String, ValueOrigin), FlowDefError> {
    let file_present = file.filter(|s| !s.is_empty());
    match (property.is_empty(), file_present) {
        (true, Some(f)) => Ok((f.to_owned(), ValueOrigin::File)),
        (false, Some(f)) if f == property => Ok((f.to_owned(), ValueOrigin::Both)),
        (false, Some(f)) => Err(FlowDefError::IdMismatch {
            property: property.to_owned(),
            file: f.to_owned(),
        }),
        (false, None) => Ok((property.to_owned(), ValueOrigin::Property)),
        (true, None) => Ok((String::new(), ValueOrigin::None)),
    }
}

fn resolve_format(
    property: FlowFormat,
    file: FlowFormat,
) -> Result<(FlowFormat, ValueOrigin), FlowDefError> {
    use FlowFormat::Unspecified;
    match (property, file) {
        (Unspecified, Unspecified) => Ok((Unspecified, ValueOrigin::None)),
        (p, Unspecified) => Ok((p, ValueOrigin::Property)),
        (Unspecified, f) => Ok((f, ValueOrigin::File)),
        (p, f) if p == f => Ok((p, ValueOrigin::Both)),
        (p, f) => Err(FlowDefError::FormatMismatch { property: p, file: f }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID_A: &str = "00000000-0000-0000-0000-000000000001";
    const UUID_B: &str = "00000000-0000-0000-0000-000000000002";

    fn video_flow_def(id: &str) -> String {
        format!(r#"{{"id":"{id}","format":"urn:x-nmos:format:video"}}"#)
    }

    #[test]
    fn parse_id_and_format() {
        let m = read_flow_def_meta(&video_flow_def(UUID_A)).unwrap();
        assert_eq!(m.id, UUID_A);
        assert_eq!(m.format, FlowFormat::Video);
    }

    #[test]
    fn missing_format_is_unspecified_not_error() {
        let m = read_flow_def_meta(&format!(r#"{{"id":"{UUID_A}"}}"#)).unwrap();
        assert_eq!(m.format, FlowFormat::Unspecified);
    }

    #[test]
    fn unknown_format_urn_is_hard_error() {
        let err =
            read_flow_def_meta(&format!(r#"{{"id":"{UUID_A}","format":"urn:bogus"}}"#)).unwrap_err();
        assert!(matches!(err, FlowDefError::UnknownFormat(_)), "got: {err:?}");
    }

    #[test]
    fn malformed_json_is_parse_error() {
        let err = read_flow_def_meta("not json").unwrap_err();
        assert!(matches!(err, FlowDefError::Parse { .. }), "got: {err:?}");
    }

    #[test]
    fn property_only_no_file() {
        let r = resolve_mxl_flow_meta(UUID_A, FlowFormat::Video, None).unwrap();
        assert_eq!(r.id, UUID_A);
        assert_eq!(r.id_origin, ValueOrigin::Property);
        assert_eq!(r.format, FlowFormat::Video);
        assert_eq!(r.format_origin, ValueOrigin::Property);
    }

    #[test]
    fn file_only() {
        let r =
            resolve_mxl_flow_meta("", FlowFormat::Unspecified, Some(&video_flow_def(UUID_A))).unwrap();
        assert_eq!(r.id, UUID_A);
        assert_eq!(r.id_origin, ValueOrigin::File);
        assert_eq!(r.format, FlowFormat::Video);
        assert_eq!(r.format_origin, ValueOrigin::File);
    }

    #[test]
    fn both_agree() {
        let r =
            resolve_mxl_flow_meta(UUID_A, FlowFormat::Video, Some(&video_flow_def(UUID_A))).unwrap();
        assert_eq!(r.id, UUID_A);
        assert_eq!(r.id_origin, ValueOrigin::Both);
        assert_eq!(r.format, FlowFormat::Video);
        assert_eq!(r.format_origin, ValueOrigin::Both);
    }

    #[test]
    fn id_mismatch_is_hard_error() {
        let err =
            resolve_mxl_flow_meta(UUID_B, FlowFormat::Unspecified, Some(&video_flow_def(UUID_A)))
                .unwrap_err();
        assert!(matches!(err, FlowDefError::IdMismatch { .. }), "got: {err:?}");
    }

    #[test]
    fn format_mismatch_is_hard_error() {
        let err =
            resolve_mxl_flow_meta("", FlowFormat::Audio, Some(&video_flow_def(UUID_A))).unwrap_err();
        assert!(matches!(err, FlowDefError::FormatMismatch { .. }), "got: {err:?}");
    }

    #[test]
    fn neither_supplied() {
        let r = resolve_mxl_flow_meta("", FlowFormat::Unspecified, None).unwrap();
        assert!(r.id.is_empty());
        assert_eq!(r.id_origin, ValueOrigin::None);
        assert_eq!(r.format, FlowFormat::Unspecified);
        assert_eq!(r.format_origin, ValueOrigin::None);
    }

    #[test]
    fn empty_string_transport_file_is_treated_as_absent() {
        let r = resolve_mxl_flow_meta(UUID_A, FlowFormat::Video, Some("")).unwrap();
        assert_eq!(r.id, UUID_A);
        assert_eq!(r.id_origin, ValueOrigin::Property);
        assert_eq!(r.format, FlowFormat::Video);
        assert_eq!(r.format_origin, ValueOrigin::Property);
    }
}
