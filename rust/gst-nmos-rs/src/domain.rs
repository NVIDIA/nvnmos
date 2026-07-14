// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! MXL Domain identity helpers.
//!
//! An MXL Domain has two facets: a stable UUID identity advertised in
//! NMOS (`urn:x-nvnmos:tag:mxl-domain-id` in the flow_def), and a
//! local filesystem path under which the MXL library reads and
//! writes shared-memory flow files. The `mxl-domain-id` and
//! `mxl-domain-path` GObject properties surface the two facets
//! independently.
//!
//! AMWA BCP-007-03 (work-in-progress) further says every MXL Domain
//! holds a `domain_def.json` file in its host directory whose `id`
//! field is the Domain's authoritative UUID. This module reads that
//! file when present and cross-checks it against the user-supplied
//! `mxl-domain-id`:
//!
//! * Both supplied and matching: fine.
//! * Both supplied and different: hard error (the user's NMOS
//!   advertisement would lie about the Domain's identity).
//! * Only the property supplied (file missing or path empty): use
//!   the property; log at INFO so an MXL-SDK-only deployment is
//!   aware the cross-check was skipped.
//! * Only the file supplied: use the file's `id` as the resolved
//!   `mxl-domain-id`.
//! * Neither property nor file, but `mxl-domain-path` is set: the
//!   NMOS tag is application-resolved (`[""]` in the flow_def);
//!   the data plane still uses the configured path.
//! * Neither property nor path (so no `domain_def.json` either):
//!   [`DomainError::Unconfigured`].
//!
//! The MXL SDK itself does not require `domain_def.json` so the
//! file's absence is **not** an error.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Filename BCP-007-03 reserves inside an MXL Domain's host
/// directory.
pub(crate) const DOMAIN_DEF_FILE_NAME: &str = "domain_def.json";

#[derive(Debug, Deserialize)]
struct DomainDef {
    /// MXL Domain identity (UUID). BCP-007-03 marks this required;
    /// we surface a typed error if a present file omits it.
    id: Option<String>,
}

#[derive(Debug, Error)]
pub(crate) enum DomainError {
    #[error(
        "mxl-domain-id mismatch: property `{property}` != {domain_def_file} `id` `{domain_def}` at `{path}`"
    )]
    Mismatch {
        property: String,
        domain_def: String,
        path: PathBuf,
        domain_def_file: &'static str,
    },
    #[error("failed to read `{path}`: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse `{path}` as JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("`{path}` has no `id` field")]
    MissingId { path: PathBuf },
    #[error("`mxl-domain-id` or `mxl-domain-path` is required")]
    Unconfigured,
}

/// MXL Domain id carried in the configuring flow_def tag
/// `urn:x-nvnmos:tag:mxl-domain-id`. May be a concrete UUID or left
/// unspecified for the element to resolve locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DomainId {
    /// A concrete Domain UUID in NMOS / IS-05.
    Concrete(String),
    /// No Domain UUID is pinned in NMOS (`[""]` tag); the Domain is
    /// resolved locally instead — in this element, via `mxl-domain-path`.
    ApplicationResolved,
}

impl DomainId {
    /// JSON tag value for `urn:x-nvnmos:tag:mxl-domain-id`.
    pub(crate) fn tag_json(&self) -> serde_json::Value {
        match self {
            Self::Concrete(id) => serde_json::json!([id]),
            Self::ApplicationResolved => serde_json::json!([""]),
        }
    }
}

/// Read `<domain_path>/domain_def.json` and extract its `id`. Returns
/// `Ok(None)` if the file does not exist; this is **not** an error
/// (the MXL SDK doesn't require the file).
pub(crate) fn read_domain_def_id(domain_path: &Path) -> Result<Option<String>, DomainError> {
    let file = domain_path.join(DOMAIN_DEF_FILE_NAME);
    let text = match fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(DomainError::Read { path: file, source }),
    };
    let parsed: DomainDef = serde_json::from_str(&text).map_err(|source| DomainError::Parse {
        path: file.clone(),
        source,
    })?;
    let id = parsed.id.unwrap_or_default();
    if id.is_empty() {
        return Err(DomainError::MissingId { path: file });
    }
    Ok(Some(id))
}

/// Combine the user's `mxl-domain-id` property with the `id` read
/// from `<mxl-domain-path>/domain_def.json`. See the module docs for
/// the truth table.
pub(crate) fn resolve_mxl_domain_id(
    property_id: &str,
    domain_path: &str,
) -> Result<DomainId, DomainError> {
    let domain_def_id = if !domain_path.is_empty() {
        read_domain_def_id(Path::new(domain_path))?
    } else {
        None
    };

    match (property_id.is_empty(), domain_def_id) {
        (true, Some(file_id)) => Ok(DomainId::Concrete(file_id)),
        (false, Some(file_id)) if file_id == property_id => Ok(DomainId::Concrete(file_id)),
        (false, Some(file_id)) => Err(DomainError::Mismatch {
            property: property_id.to_owned(),
            domain_def: file_id,
            path: Path::new(domain_path).join(DOMAIN_DEF_FILE_NAME),
            domain_def_file: DOMAIN_DEF_FILE_NAME,
        }),
        (false, None) => Ok(DomainId::Concrete(property_id.to_owned())),
        (true, None) if !domain_path.is_empty() => Ok(DomainId::ApplicationResolved),
        (true, None) => Err(DomainError::Unconfigured),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Helper: write a `domain_def.json` containing the given body
    /// into a fresh tempdir and return the dir's path.
    fn temp_domain_with_file(body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut f = fs::File::create(dir.path().join(DOMAIN_DEF_FILE_NAME))
            .expect("create domain_def.json");
        f.write_all(body.as_bytes()).expect("write domain_def.json");
        dir
    }

    const UUID_A: &str = "11111111-1111-1111-1111-111111111111";
    const UUID_B: &str = "22222222-2222-2222-2222-222222222222";

    #[test]
    fn property_only_no_path() {
        let id = resolve_mxl_domain_id(UUID_A, "").unwrap();
        assert_eq!(id, DomainId::Concrete(UUID_A.to_owned()));
    }

    #[test]
    fn property_only_path_without_file() {
        let dir = tempfile::tempdir().unwrap();
        let id = resolve_mxl_domain_id(UUID_A, dir.path().to_str().unwrap()).unwrap();
        assert_eq!(id, DomainId::Concrete(UUID_A.to_owned()));
    }

    #[test]
    fn file_only() {
        let dir = temp_domain_with_file(&format!(r#"{{"id":"{UUID_A}"}}"#));
        let id = resolve_mxl_domain_id("", dir.path().to_str().unwrap()).unwrap();
        assert_eq!(id, DomainId::Concrete(UUID_A.to_owned()));
    }

    #[test]
    fn path_without_property_or_domain_def_is_application_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let id = resolve_mxl_domain_id("", dir.path().to_str().unwrap()).unwrap();
        assert_eq!(id, DomainId::ApplicationResolved);
        assert_eq!(id.tag_json(), serde_json::json!([""]));
    }

    #[test]
    fn both_agree() {
        let dir = temp_domain_with_file(&format!(r#"{{"id":"{UUID_A}"}}"#));
        let id = resolve_mxl_domain_id(UUID_A, dir.path().to_str().unwrap()).unwrap();
        assert_eq!(id, DomainId::Concrete(UUID_A.to_owned()));
    }

    #[test]
    fn both_disagree_is_hard_error() {
        let dir = temp_domain_with_file(&format!(r#"{{"id":"{UUID_A}"}}"#));
        let err = resolve_mxl_domain_id(UUID_B, dir.path().to_str().unwrap()).unwrap_err();
        assert!(matches!(err, DomainError::Mismatch { .. }), "got: {err:?}");
    }

    #[test]
    fn neither_property_nor_path_is_unconfigured() {
        let err = resolve_mxl_domain_id("", "").unwrap_err();
        assert!(matches!(err, DomainError::Unconfigured), "got: {err:?}");
    }

    #[test]
    fn bad_json_is_parse_error() {
        let dir = temp_domain_with_file("{ not json");
        let err = resolve_mxl_domain_id("", dir.path().to_str().unwrap()).unwrap_err();
        assert!(matches!(err, DomainError::Parse { .. }), "got: {err:?}");
    }

    #[test]
    fn json_without_id_is_missing_id() {
        let dir = temp_domain_with_file(r#"{"label":"no id here"}"#);
        let err = resolve_mxl_domain_id("", dir.path().to_str().unwrap()).unwrap_err();
        assert!(matches!(err, DomainError::MissingId { .. }), "got: {err:?}");
    }

    #[test]
    fn empty_id_string_is_missing_id() {
        let dir = temp_domain_with_file(r#"{"id":""}"#);
        let err = resolve_mxl_domain_id("", dir.path().to_str().unwrap()).unwrap_err();
        assert!(matches!(err, DomainError::MissingId { .. }), "got: {err:?}");
    }
}
