// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! MXL `flow_def` JSON helpers.
//!
//! The NvNmos transport file for transport=mxl is a JSON document
//! whose top-level `id` is the MXL flow id and whose `format` is the
//! NMOS format URN (`urn:x-nmos:format:video|audio|data`). The
//! element needs both to configure the inner `mxlsink` / `mxlsrc`
//! (`mxlsink.flow-id=` and `mxlsrc.{video,audio,data}-flow-id=`).
//!
//! [`read_flow_def_meta`] parses the JSON into [`FlowDefMeta`].
//! [`resolve_mxl_flow_meta`] combines that with the `mxl-flow-id`
//! property and the caps-derived [`FlowFormat`] (see
//! [`FlowFormat::from_caps`]):
//!
//! * Property only: use it.
//! * File only: use it.
//! * Both agreeing: use the value (DEBUG log at the caller).
//! * Both disagreeing: hard error.
//! * Neither: empty id / `Unspecified` format — the element will
//!   fall back to its fake chain.
//!
//! At NULL→READY the element-level rule is "property overrides
//! file", not "cross-check, error on mismatch". The caller therefore
//! splices the user's properties into the transport file with
//! [`splice_overrides`] before invoking [`resolve_mxl_flow_meta`],
//! at which point only the `Both`-agree and `File`-only branches can
//! be reached. The `Property`-only branch is reachable when the user
//! supplies no transport file at all (deferred/synthesised path).
//! Activations from the daemon are *not* re-spliced — the IS-05
//! PATCH is authoritative, and the activation path passes an empty
//! `property_id` to [`resolve_mxl_flow_meta`] so the file always
//! wins silently.
//!
//! [`build_from_caps`] is the reverse path: given GStreamer essence
//! caps plus property state, emit a `flow_def` JSON document that
//! satisfies the MXL `FlowParser` (its hard requirements: `id`,
//! `format`, non-empty `label`, non-empty `tags["urn:x-nmos:tag:grouphint/v1.0"]`,
//! plus per-format dimensions/rates). Only the caps shapes that
//! `mxlsink` advertises today are accepted: v210 video, F32LE audio,
//! ST 2038 ANC.

use gstreamer as gst;
use serde::Deserialize;
use thiserror::Error;

use crate::types::{CapsMode, FlowFormat};

#[derive(Debug, Deserialize)]
struct RawFlowDef {
    /// MXL flow id (UUID). Required by BCP-007-03 but here we just
    /// surface a typed error if a JSON document omits it.
    id: Option<String>,
    /// NMOS format URN: `urn:x-nmos:format:video|audio|data`.
    /// Optional in the file; the property may supply it instead.
    format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawRational {
    numerator: i64,
    denominator: i64,
}

/// Subset of the flow_def schema needed by [`caps_from_flow_def`].
/// `media_type` selects the GStreamer caps name, the remaining
/// fields populate its structure; everything `RawFlowDef` already
/// covers (`id`, `format`) is repeated so we can deserialise once.
#[derive(Debug, Deserialize)]
struct RawFlowDefForCaps {
    media_type: Option<String>,
    grain_rate: Option<RawRational>,
    sample_rate: Option<RawRational>,
    frame_width: Option<i32>,
    frame_height: Option<i32>,
    channel_count: Option<i32>,
    interlace_mode: Option<String>,
}

#[derive(Debug, Error)]
pub(crate) enum FlowDefError {
    #[error("failed to parse transport file as JSON: {source}")]
    Parse {
        #[source]
        source: serde_json::Error,
    },
    #[error("transport file `format` `{0}` is not a recognised NMOS format URN")]
    UnknownFormat(String),
    #[error(
        "mxl-flow-id mismatch: property `{property}` != transport file top-level `id` `{file}`"
    )]
    IdMismatch { property: String, file: String },
    #[error(
        "flow format mismatch: caps-derived `{property:?}` != transport file `format` `{file:?}`"
    )]
    FormatMismatch {
        property: FlowFormat,
        file: FlowFormat,
    },
    #[error("transport file top-level JSON is not an object")]
    NotAnObject,
    #[error("transport file `tags` is not a JSON object")]
    TagsNotAnObject,
}

#[derive(Debug)]
pub(crate) struct FlowDefMeta {
    pub(crate) id: String,
    pub(crate) format: FlowFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueOrigin {
    /// User supplied the property; no transport file consulted (or
    /// the file did not carry the field).
    Property,
    /// Read from the transport file; user did not supply a property
    /// override.
    File,
    /// User supplied the property and the transport file agreed.
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
/// `Ok(None)` is **not** returned — a transport file we can't parse
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

/// Combine the user's `mxl-flow-id` property and the caps-derived
/// [`FlowFormat`] with `id` / `format` read from the transport file.
/// See the module docs for the truth table. `property_format` is
/// derived from `caps` via [`FlowFormat::from_caps`] (both elements
/// route their caps through the same helper); when `caps` is unset
/// or its media-type isn't recognised it falls back to
/// [`FlowFormat::Unspecified`] and the file or the fake chain
/// decides.
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

/// User-set properties that, when present, are spliced into a
/// transport file before it reaches the daemon. The element-level
/// rule is "property overrides file" — each `Option` field that is
/// `Some` replaces the corresponding field/tag in the JSON; `None`
/// leaves the file's value untouched.
///
/// `caps_mode` always applies (no `Option` wrapping) because the
/// [`crate::types::CapsMode`] enum carries its own "don't touch the
/// file" state in `Auto`.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FlowDefOverrides<'a> {
    /// Top-level `id` (the MXL flow id).
    pub(crate) flow_id: Option<&'a str>,
    /// Top-level `label`.
    pub(crate) label: Option<&'a str>,
    /// Top-level `description`.
    pub(crate) description: Option<&'a str>,
    /// `tags["urn:x-nvnmos:tag:name"]` (the per-side resource name).
    pub(crate) name: Option<&'a str>,
    /// `tags["urn:x-nvnmos:tag:mxl-domain-id"]`.
    pub(crate) mxl_domain_id: Option<&'a str>,
    /// Presence of `tags["urn:x-nvnmos:tag:caps"]`. See [`CapsMode`].
    pub(crate) caps_mode: CapsMode,
}

/// Splice user-set property values into a transport file before
/// handing it to the daemon. Empty-string `Option`s are treated as
/// "unset" by the caller convention used throughout the element
/// code (see [`crate::session::CommonSettings`]) so this function
/// does not have to special-case them — pass `None` when the
/// property is empty.
///
/// libnvnmos's [BCP-007-03 WIP] reading rule for the caps tag is
/// "present + non-empty array means wide"; `CapsMode::Wide` writes
/// `[""]` to satisfy that rule, `CapsMode::Narrow` removes the tag,
/// and `CapsMode::Auto` leaves the file's existing presence alone.
pub(crate) fn splice_overrides(
    text: &str,
    overrides: &FlowDefOverrides<'_>,
) -> Result<String, FlowDefError> {
    let mut value: serde_json::Value =
        serde_json::from_str(text).map_err(|source| FlowDefError::Parse { source })?;
    let object = value.as_object_mut().ok_or(FlowDefError::NotAnObject)?;

    if let Some(flow_id) = overrides.flow_id {
        object.insert("id".to_owned(), serde_json::Value::String(flow_id.to_owned()));
    }
    if let Some(label) = overrides.label {
        object.insert(
            "label".to_owned(),
            serde_json::Value::String(label.to_owned()),
        );
    }
    if let Some(description) = overrides.description {
        object.insert(
            "description".to_owned(),
            serde_json::Value::String(description.to_owned()),
        );
    }

    // We only need a mutable `tags` if at least one tag-affecting
    // override is active. Bypass the tag fix-ups (and the type check)
    // when there's nothing to do, so a transport file without a
    // `tags` object still round-trips cleanly.
    let touches_tags = overrides.name.is_some()
        || overrides.mxl_domain_id.is_some()
        || overrides.caps_mode != CapsMode::Auto;
    if touches_tags {
        let tags_value = object
            .entry("tags".to_owned())
            .or_insert_with(|| serde_json::json!({}));
        let tags = tags_value
            .as_object_mut()
            .ok_or(FlowDefError::TagsNotAnObject)?;
        if let Some(name) = overrides.name {
            tags.insert(
                "urn:x-nvnmos:tag:name".to_owned(),
                serde_json::json!([name]),
            );
        }
        if let Some(mxl_domain_id) = overrides.mxl_domain_id {
            tags.insert(
                "urn:x-nvnmos:tag:mxl-domain-id".to_owned(),
                serde_json::json!([mxl_domain_id]),
            );
        }
        match overrides.caps_mode {
            CapsMode::Auto => {}
            CapsMode::Narrow => {
                tags.remove("urn:x-nvnmos:tag:caps");
            }
            CapsMode::Wide => {
                tags.insert(
                    "urn:x-nvnmos:tag:caps".to_owned(),
                    serde_json::json!([""]),
                );
            }
        }
    }

    Ok(serde_json::to_string(&value).expect("flow_def value is always serialisable"))
}

/// Inputs to [`build_from_caps`]. All borrowed; the builder doesn't
/// hold any of these beyond the call.
#[derive(Debug)]
pub(crate) struct FlowDefBuildInput<'a> {
    /// MXL flow id (UUID). Required and non-empty — this is the value
    /// `mxlsink.flow-id=` / `mxlsrc.{video,audio,data}-flow-id=` ends
    /// up consuming.
    pub(crate) flow_id: &'a str,
    /// NMOS resource name. Used both as the `<group>` portion of the
    /// `urn:x-nmos:tag:grouphint/v1.0` tag (required by MXL
    /// `FlowParser`) and as the value of the
    /// `urn:x-nvnmos:tag:name` tag (required by `libnvnmos`).
    /// Required non-empty.
    pub(crate) name: &'a str,
    /// Resolved MXL Domain id (UUID). Emitted as
    /// `urn:x-nvnmos:tag:mxl-domain-id` — required by `libnvnmos` to
    /// resolve the IS-05 `mxl_domain_id` transport parameter at
    /// activation time.
    pub(crate) mxl_domain_id: &'a str,
    /// NMOS `label`. The MXL `FlowParser` rejects an empty label, so
    /// the builder falls back to `flow_id` when this is empty.
    pub(crate) label: &'a str,
    /// NMOS `description`. Optional; omitted from the JSON when empty.
    pub(crate) description: &'a str,
    /// Essence caps to translate. The first structure is used.
    pub(crate) caps: &'a gst::Caps,
}

#[derive(Debug, Error)]
pub(crate) enum FlowDefBuildError {
    #[error("synthesising flow_def from caps requires a non-empty `mxl-flow-id`")]
    MissingFlowId,
    #[error("synthesising flow_def from caps requires a non-empty `sender-name` / `receiver-name`")]
    MissingName,
    #[error("synthesising flow_def from caps requires a non-empty `mxl-domain-id`")]
    MissingMxlDomainId,
    #[error("caps are empty or ANY; an essence shape is required to synthesise a flow_def")]
    EmptyCaps,
    #[error(
        "caps `{0}` are not supported by the caps→flow_def builder; supported shapes are `video/x-raw,format=v210,…`, `audio/x-raw,format=F32LE,…`, and `meta/x-st-2038,framerate=…,…`"
    )]
    UnsupportedCaps(String),
    #[error("caps `{name}` are missing required field `{field}`{}", .hint.as_deref().map(|h| format!(" ({h})")).unwrap_or_default())]
    MissingCapsField {
        name: String,
        field: &'static str,
        hint: Option<String>,
    },
    #[error("caps `{name}` field `{field}` has unsupported value `{value}`")]
    UnsupportedCapsValue {
        name: String,
        field: &'static str,
        value: String,
    },
}

/// Build an MXL `flow_def` JSON document from essence caps and
/// property state. The shape matches the reference flows in the MXL
/// SDK (`mxl/lib/tests/data/{v210,audio,data}_flow.json`).
///
/// Only the fields directly derivable from the caps are emitted —
/// the builder doesn't synthesise `colorspace`, `components`, or
/// `transfer_characteristic` from `format=v210` alone. The user can
/// add those by supplying a `transport-file` (which then wins over
/// the builder).
pub(crate) fn build_from_caps(input: &FlowDefBuildInput<'_>) -> Result<String, FlowDefBuildError> {
    if input.flow_id.is_empty() {
        return Err(FlowDefBuildError::MissingFlowId);
    }
    if input.name.is_empty() {
        return Err(FlowDefBuildError::MissingName);
    }
    if input.mxl_domain_id.is_empty() {
        return Err(FlowDefBuildError::MissingMxlDomainId);
    }
    let structure = input
        .caps
        .structure(0)
        .ok_or(FlowDefBuildError::EmptyCaps)?;

    let name = structure.name();
    let (format, body) = match name.as_str() {
        "video/x-raw" => build_video_body(structure)?,
        "audio/x-raw" => build_audio_body(structure)?,
        "meta/x-st-2038" => build_data_body(structure)?,
        other => return Err(FlowDefBuildError::UnsupportedCaps(other.to_owned())),
    };

    let role = match format {
        FlowFormat::Video => "Video",
        FlowFormat::Audio => "Audio",
        FlowFormat::Data => "Ancillary Data",
        FlowFormat::Unspecified => unreachable!("body builders never return Unspecified"),
    };
    let grouphint = format!("{}:{}", input.name, role);
    // `label` is `field_as_string` (required) in nmos-cpp — empty
    // would be accepted as long as the key is present, but a more
    // helpful default is the resource name.
    let label = if input.label.is_empty() {
        input.name
    } else {
        input.label
    };
    let format_urn = format
        .as_format_urn()
        .expect("body builders never return Unspecified");

    let mut value = serde_json::json!({
        "id": input.flow_id,
        "format": format_urn,
        "label": label,
        // `description` is `field_as_string` (required) in nmos-cpp;
        // emit the property value as-is, even if it's empty.
        "description": input.description,
        // The three tags consumed by `libnvnmos` (`name`, `mxl-domain-id`)
        // and by the MXL `FlowParser` (`grouphint`).
        "tags": {
            "urn:x-nmos:tag:grouphint/v1.0": [grouphint],
            "urn:x-nvnmos:tag:name": [input.name],
            "urn:x-nvnmos:tag:mxl-domain-id": [input.mxl_domain_id],
        },
        "parents": [],
    });
    let object = value.as_object_mut().unwrap();
    for (k, v) in body {
        object.insert(k, v);
    }

    Ok(serde_json::to_string(&value).expect("flow_def value is always serialisable"))
}

/// Body fields specific to a video flow_def. Returns the resolved
/// [`FlowFormat`] (always [`FlowFormat::Video`]) alongside, so the
/// outer builder can do its format cross-check uniformly.
///
/// `colorspace` and `components` are emitted even though the caps
/// don't carry them explicitly: `libnvnmos` requires both. They're
/// derived deterministically from `format=v210` (HD-default BT709
/// primaries; standard Y/Cb/Cr 4:2:2 10-bit triple with Cb/Cr at
/// half horizontal resolution). Users wanting BT2020 / a different
/// component layout should supply a `transport-file` instead.
fn build_video_body(
    structure: &gst::StructureRef,
) -> Result<(FlowFormat, Vec<(String, serde_json::Value)>), FlowDefBuildError> {
    let name = structure.name();
    let format = caps_field_string(structure, "format")?;
    let media_type = match format.as_str() {
        "v210" => "video/v210",
        other => {
            return Err(FlowDefBuildError::UnsupportedCapsValue {
                name: name.to_string(),
                field: "format",
                value: other.to_owned(),
            });
        }
    };
    let width = caps_field_i32(structure, "width")?;
    let height = caps_field_i32(structure, "height")?;
    let framerate = caps_field_fraction(structure, "framerate", None)?;

    let chroma_width = width / 2;
    let mut fields: Vec<(String, serde_json::Value)> = vec![
        ("media_type".to_owned(), serde_json::Value::String(media_type.to_owned())),
        (
            "grain_rate".to_owned(),
            serde_json::json!({
                "numerator": framerate.0,
                "denominator": framerate.1,
            }),
        ),
        ("frame_width".to_owned(), serde_json::json!(width)),
        ("frame_height".to_owned(), serde_json::json!(height)),
        ("colorspace".to_owned(), serde_json::Value::String("BT709".to_owned())),
        (
            "components".to_owned(),
            serde_json::json!([
                { "name": "Y",  "width": width,        "height": height, "bit_depth": 10 },
                { "name": "Cb", "width": chroma_width, "height": height, "bit_depth": 10 },
                { "name": "Cr", "width": chroma_width, "height": height, "bit_depth": 10 },
            ]),
        ),
    ];
    if let Ok(interlace_mode) = structure.get::<String>("interlace-mode") {
        fields.push((
            "interlace_mode".to_owned(),
            serde_json::Value::String(interlace_mode),
        ));
    }

    Ok((FlowFormat::Video, fields))
}

fn build_audio_body(
    structure: &gst::StructureRef,
) -> Result<(FlowFormat, Vec<(String, serde_json::Value)>), FlowDefBuildError> {
    let name = structure.name();
    let format = caps_field_string(structure, "format")?;
    let (media_type, bit_depth) = match format.as_str() {
        "F32LE" => ("audio/float32", 32),
        other => {
            return Err(FlowDefBuildError::UnsupportedCapsValue {
                name: name.to_string(),
                field: "format",
                value: other.to_owned(),
            });
        }
    };
    let rate = caps_field_i32(structure, "rate")?;
    let channels = caps_field_i32(structure, "channels")?;

    let fields: Vec<(String, serde_json::Value)> = vec![
        ("media_type".to_owned(), serde_json::Value::String(media_type.to_owned())),
        (
            "sample_rate".to_owned(),
            serde_json::json!({ "numerator": rate, "denominator": 1 }),
        ),
        ("channel_count".to_owned(), serde_json::json!(channels)),
        ("bit_depth".to_owned(), serde_json::json!(bit_depth)),
    ];

    Ok((FlowFormat::Audio, fields))
}

fn build_data_body(
    structure: &gst::StructureRef,
) -> Result<(FlowFormat, Vec<(String, serde_json::Value)>), FlowDefBuildError> {
    let framerate = caps_field_fraction(
        structure,
        "framerate",
        Some(
            "ST 2038 caps must carry a `framerate` field — \
             insert a `capsfilter caps=\"meta/x-st-2038,framerate=30/1\"` upstream, or use `transport-file` / `transport-file-path` instead",
        ),
    )?;
    let fields: Vec<(String, serde_json::Value)> = vec![
        (
            "media_type".to_owned(),
            serde_json::Value::String("video/smpte291".to_owned()),
        ),
        (
            "grain_rate".to_owned(),
            serde_json::json!({
                "numerator": framerate.0,
                "denominator": framerate.1,
            }),
        ),
    ];
    Ok((FlowFormat::Data, fields))
}

/// Errors from [`caps_from_flow_def`].
#[derive(Debug, Error)]
pub(crate) enum FlowDefCapsError {
    #[error("failed to parse transport file as JSON: {source}")]
    Parse {
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "transport file is missing required field `{0}` (cannot derive essence caps without it)"
    )]
    MissingField(&'static str),
    #[error(
        "transport file `media_type` `{0}` is not supported by the caps reverse mapping; supported: `video/v210`, `audio/float32`, `video/smpte291`"
    )]
    UnsupportedMediaType(String),
    #[error("transport file `{field}` has an out-of-range value `{value}` (must fit in i32)")]
    OutOfRangeField {
        field: &'static str,
        value: i64,
    },
}

/// Reverse of [`build_from_caps`]: turn an MXL `flow_def` JSON
/// document into essence Caps that `nmossrc` advertises on its
/// ghost source pad so downstream caps queries (and a peer-querying
/// deferred `nmossink`) see the concrete shape the flow will carry.
///
/// Supports the same caps shapes the forward path emits:
/// * `media_type=video/v210` → `video/x-raw,format=v210,width=…,height=…,framerate=…[,interlace-mode=…]`
/// * `media_type=audio/float32` → `audio/x-raw,format=F32LE,rate=…,channels=…,layout=interleaved`
/// * `media_type=video/smpte291` → `meta/x-st-2038,framerate=…`
///
/// Other media types produce [`FlowDefCapsError::UnsupportedMediaType`];
/// missing required fields produce [`FlowDefCapsError::MissingField`].
/// The receiver treats both as a hard NULL→READY (or activation)
/// failure — if the user supplied a transport file (or the daemon
/// spliced one), the file is expected to be complete.
pub(crate) fn caps_from_flow_def(text: &str) -> Result<gst::Caps, FlowDefCapsError> {
    let raw: RawFlowDefForCaps =
        serde_json::from_str(text).map_err(|source| FlowDefCapsError::Parse { source })?;
    let media_type = raw
        .media_type
        .ok_or(FlowDefCapsError::MissingField("media_type"))?;
    match media_type.as_str() {
        "video/v210" => {
            let rate = raw
                .grain_rate
                .ok_or(FlowDefCapsError::MissingField("grain_rate"))?;
            let width = raw
                .frame_width
                .ok_or(FlowDefCapsError::MissingField("frame_width"))?;
            let height = raw
                .frame_height
                .ok_or(FlowDefCapsError::MissingField("frame_height"))?;
            let framerate = rational_to_gst_fraction("grain_rate", rate)?;
            let mut builder = gst::Caps::builder("video/x-raw")
                .field("format", "v210")
                .field("width", width)
                .field("height", height)
                .field("framerate", framerate);
            if let Some(im) = raw.interlace_mode {
                builder = builder.field("interlace-mode", im);
            }
            Ok(builder.build())
        }
        "audio/float32" => {
            let rate = raw
                .sample_rate
                .ok_or(FlowDefCapsError::MissingField("sample_rate"))?;
            let channels = raw
                .channel_count
                .ok_or(FlowDefCapsError::MissingField("channel_count"))?;
            // sample_rate is integer in practice; downcast the
            // numerator and require denominator == 1.
            if rate.denominator != 1 {
                return Err(FlowDefCapsError::OutOfRangeField {
                    field: "sample_rate.denominator",
                    value: rate.denominator,
                });
            }
            let rate_i32: i32 = rate.numerator.try_into().map_err(|_| {
                FlowDefCapsError::OutOfRangeField {
                    field: "sample_rate.numerator",
                    value: rate.numerator,
                }
            })?;
            Ok(gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("rate", rate_i32)
                .field("channels", channels)
                .field("layout", "interleaved")
                .build())
        }
        "video/smpte291" => {
            let rate = raw
                .grain_rate
                .ok_or(FlowDefCapsError::MissingField("grain_rate"))?;
            let framerate = rational_to_gst_fraction("grain_rate", rate)?;
            Ok(gst::Caps::builder("meta/x-st-2038")
                .field("framerate", framerate)
                .build())
        }
        other => Err(FlowDefCapsError::UnsupportedMediaType(other.to_owned())),
    }
}

fn rational_to_gst_fraction(
    field: &'static str,
    r: RawRational,
) -> Result<gst::Fraction, FlowDefCapsError> {
    let num: i32 = r
        .numerator
        .try_into()
        .map_err(|_| FlowDefCapsError::OutOfRangeField {
            field,
            value: r.numerator,
        })?;
    let den: i32 = r
        .denominator
        .try_into()
        .map_err(|_| FlowDefCapsError::OutOfRangeField {
            field,
            value: r.denominator,
        })?;
    Ok(gst::Fraction::new(num, den))
}

fn caps_field_string(
    structure: &gst::StructureRef,
    field: &'static str,
) -> Result<String, FlowDefBuildError> {
    structure
        .get::<String>(field)
        .map_err(|_| FlowDefBuildError::MissingCapsField {
            name: structure.name().to_string(),
            field,
            hint: None,
        })
}

fn caps_field_i32(
    structure: &gst::StructureRef,
    field: &'static str,
) -> Result<i32, FlowDefBuildError> {
    structure
        .get::<i32>(field)
        .map_err(|_| FlowDefBuildError::MissingCapsField {
            name: structure.name().to_string(),
            field,
            hint: None,
        })
}

fn caps_field_fraction(
    structure: &gst::StructureRef,
    field: &'static str,
    hint: Option<&str>,
) -> Result<(i32, i32), FlowDefBuildError> {
    structure
        .get::<gst::Fraction>(field)
        .map(|f| (f.numer(), f.denom()))
        .map_err(|_| FlowDefBuildError::MissingCapsField {
            name: structure.name().to_string(),
            field,
            hint: hint.map(str::to_owned),
        })
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

    fn parse_value(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("splice output must be valid JSON")
    }

    #[test]
    fn splice_noop_when_all_overrides_unset() {
        let original = video_flow_def(UUID_A);
        let spliced = splice_overrides(&original, &FlowDefOverrides::default()).unwrap();
        assert_eq!(parse_value(&spliced), parse_value(&original));
    }

    #[test]
    fn splice_overrides_top_level_id_label_description() {
        let original = format!(
            r#"{{"id":"{UUID_A}","format":"urn:x-nmos:format:video","label":"orig","description":"od"}}"#
        );
        let spliced = splice_overrides(
            &original,
            &FlowDefOverrides {
                flow_id: Some(UUID_B),
                label: Some("new label"),
                description: Some("new description"),
                ..FlowDefOverrides::default()
            },
        )
        .unwrap();
        let v = parse_value(&spliced);
        assert_eq!(v["id"], UUID_B);
        assert_eq!(v["label"], "new label");
        assert_eq!(v["description"], "new description");
        // unrelated fields untouched
        assert_eq!(v["format"], "urn:x-nmos:format:video");
    }

    #[test]
    fn splice_overrides_name_and_mxl_domain_id_tags() {
        let original = format!(
            r#"{{"id":"{UUID_A}","format":"urn:x-nmos:format:video","tags":{{"urn:x-nvnmos:tag:name":["old-name"],"urn:x-nvnmos:tag:mxl-domain-id":["00000000-0000-0000-0000-000000000abc"],"urn:x-nmos:tag:grouphint/v1.0":["g:Video"]}}}}"#
        );
        let spliced = splice_overrides(
            &original,
            &FlowDefOverrides {
                name: Some("new-name"),
                mxl_domain_id: Some(DOMAIN_ID),
                ..FlowDefOverrides::default()
            },
        )
        .unwrap();
        let v = parse_value(&spliced);
        assert_eq!(v["tags"]["urn:x-nvnmos:tag:name"], serde_json::json!(["new-name"]));
        assert_eq!(
            v["tags"]["urn:x-nvnmos:tag:mxl-domain-id"],
            serde_json::json!([DOMAIN_ID])
        );
        // grouphint preserved
        assert_eq!(
            v["tags"]["urn:x-nmos:tag:grouphint/v1.0"],
            serde_json::json!(["g:Video"])
        );
    }

    #[test]
    fn splice_creates_tags_object_if_missing_when_needed() {
        let original = video_flow_def(UUID_A);
        let spliced = splice_overrides(
            &original,
            &FlowDefOverrides {
                name: Some("foo"),
                ..FlowDefOverrides::default()
            },
        )
        .unwrap();
        let v = parse_value(&spliced);
        assert_eq!(v["tags"]["urn:x-nvnmos:tag:name"], serde_json::json!(["foo"]));
    }

    #[test]
    fn splice_caps_mode_narrow_removes_tag() {
        let original = format!(
            r#"{{"id":"{UUID_A}","format":"urn:x-nmos:format:video","tags":{{"urn:x-nvnmos:tag:caps":[""]}}}}"#
        );
        let spliced = splice_overrides(
            &original,
            &FlowDefOverrides {
                caps_mode: CapsMode::Narrow,
                ..FlowDefOverrides::default()
            },
        )
        .unwrap();
        let v = parse_value(&spliced);
        assert!(
            v["tags"].as_object().unwrap().get("urn:x-nvnmos:tag:caps").is_none(),
            "tag should be removed; got {v}"
        );
    }

    #[test]
    fn splice_caps_mode_wide_inserts_non_empty_array() {
        let original = video_flow_def(UUID_A);
        let spliced = splice_overrides(
            &original,
            &FlowDefOverrides {
                caps_mode: CapsMode::Wide,
                ..FlowDefOverrides::default()
            },
        )
        .unwrap();
        let v = parse_value(&spliced);
        let arr = v["tags"]["urn:x-nvnmos:tag:caps"]
            .as_array()
            .expect("caps tag must be an array");
        // libnvnmos's rule is "present + non-empty array means wide";
        // assert that's what we wrote.
        assert!(!arr.is_empty(), "wide-mode array must be non-empty; got {v}");
    }

    #[test]
    fn splice_caps_mode_auto_leaves_file_tag_alone() {
        let with_tag = format!(
            r#"{{"id":"{UUID_A}","format":"urn:x-nmos:format:video","tags":{{"urn:x-nvnmos:tag:caps":[""]}}}}"#
        );
        let unchanged = splice_overrides(&with_tag, &FlowDefOverrides::default()).unwrap();
        assert_eq!(parse_value(&unchanged), parse_value(&with_tag));

        let without_tag = video_flow_def(UUID_A);
        let still_without = splice_overrides(&without_tag, &FlowDefOverrides::default()).unwrap();
        let v = parse_value(&still_without);
        // No tags object was needed, so none should have been
        // synthesised either.
        assert!(v.get("tags").is_none(), "auto must not synthesise tags; got {v}");
    }

    #[test]
    fn splice_rejects_non_object_top_level() {
        let err = splice_overrides("[]", &FlowDefOverrides::default()).unwrap_err();
        assert!(matches!(err, FlowDefError::NotAnObject), "got: {err:?}");
    }

    #[test]
    fn splice_rejects_tags_that_are_not_an_object() {
        let original = format!(r#"{{"id":"{UUID_A}","tags":42}}"#);
        let err = splice_overrides(
            &original,
            &FlowDefOverrides {
                name: Some("x"),
                ..FlowDefOverrides::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, FlowDefError::TagsNotAnObject), "got: {err:?}");
    }

    fn ensure_gst_initialised() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            gst::init().expect("gst init for caps→flow_def tests");
        });
    }

    fn caps_from_str(s: &str) -> gst::Caps {
        use std::str::FromStr;
        ensure_gst_initialised();
        gst::Caps::from_str(s).expect("test caps parse")
    }

    const DOMAIN_ID: &str = "1ac254d9-c9be-475a-93a7-f80b9c1063a8";

    fn input<'a>(
        flow_id: &'a str,
        name: &'a str,
        caps: &'a gst::Caps,
    ) -> FlowDefBuildInput<'a> {
        FlowDefBuildInput {
            flow_id,
            name,
            mxl_domain_id: DOMAIN_ID,
            label: "",
            description: "",
            caps,
        }
    }

    fn parse(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("builder emits valid JSON")
    }

    #[test]
    fn build_video_v210_minimal() {
        let caps = caps_from_str(
            "video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001",
        );
        let json = build_from_caps(&input(UUID_A, "cam-1", &caps)).unwrap();
        let v = parse(&json);
        assert_eq!(v["id"], UUID_A);
        assert_eq!(v["format"], "urn:x-nmos:format:video");
        assert_eq!(v["media_type"], "video/v210");
        assert_eq!(v["frame_width"], 1920);
        assert_eq!(v["frame_height"], 1080);
        assert_eq!(v["grain_rate"]["numerator"], 30000);
        assert_eq!(v["grain_rate"]["denominator"], 1001);
        assert_eq!(v["label"], "cam-1", "label falls back to name when property is empty");
        assert_eq!(v["description"], "", "description is emitted even when empty");
        assert_eq!(v["parents"], serde_json::json!([]));
        assert_eq!(
            v["tags"]["urn:x-nmos:tag:grouphint/v1.0"],
            serde_json::json!(["cam-1:Video"]),
        );
        assert_eq!(
            v["tags"]["urn:x-nvnmos:tag:name"],
            serde_json::json!(["cam-1"]),
            "libnvnmos requires the name tag",
        );
        assert_eq!(
            v["tags"]["urn:x-nvnmos:tag:mxl-domain-id"],
            serde_json::json!([DOMAIN_ID]),
            "libnvnmos requires the mxl-domain-id tag",
        );
        assert!(v.get("interlace_mode").is_none(), "no caps field → no interlace_mode emitted");
        assert_eq!(v["colorspace"], "BT709", "BT709 default for v210");
        let components = v["components"].as_array().expect("components is an array");
        assert_eq!(components.len(), 3, "Y/Cb/Cr triple");
        assert_eq!(components[0]["name"], "Y");
        assert_eq!(components[0]["width"], 1920);
        assert_eq!(components[0]["bit_depth"], 10);
        assert_eq!(components[1]["name"], "Cb");
        assert_eq!(components[1]["width"], 960, "Cb is half-width for 4:2:2");
        assert_eq!(components[2]["name"], "Cr");
        assert_eq!(components[2]["width"], 960);
    }

    #[test]
    fn build_video_v210_with_interlace_mode_label_and_description() {
        let caps = caps_from_str(
            "video/x-raw,format=v210,width=720,height=486,framerate=30000/1001,interlace-mode=interlaced-tff",
        );
        let json = build_from_caps(&FlowDefBuildInput {
            label: "Studio A v210",
            description: "long description goes here",
            ..input(UUID_A, "cam-1", &caps)
        })
        .unwrap();
        let v = parse(&json);
        assert_eq!(v["label"], "Studio A v210", "property wins over name fallback");
        assert_eq!(v["description"], "long description goes here");
        assert_eq!(v["interlace_mode"], "interlaced-tff");
    }

    #[test]
    fn build_audio_f32le_minimal() {
        let caps = caps_from_str(
            "audio/x-raw,format=F32LE,rate=48000,channels=2,layout=interleaved",
        );
        let json = build_from_caps(&input(UUID_A, "mic-1", &caps)).unwrap();
        let v = parse(&json);
        assert_eq!(v["format"], "urn:x-nmos:format:audio");
        assert_eq!(v["media_type"], "audio/float32");
        assert_eq!(v["sample_rate"]["numerator"], 48000);
        assert_eq!(v["sample_rate"]["denominator"], 1);
        assert_eq!(v["channel_count"], 2);
        assert_eq!(v["bit_depth"], 32);
        assert_eq!(v["label"], "mic-1", "label falls back to name when property is empty");
        assert_eq!(v["description"], "", "description is emitted even when empty");
        assert_eq!(
            v["tags"]["urn:x-nmos:tag:grouphint/v1.0"],
            serde_json::json!(["mic-1:Audio"]),
        );
        assert!(v.get("colorspace").is_none(), "colorspace is video-only");
        assert!(v.get("components").is_none(), "components is video-only");
    }

    #[test]
    fn build_data_st2038_with_framerate() {
        let caps = caps_from_str("meta/x-st-2038,framerate=30/1");
        let json = build_from_caps(&input(UUID_A, "anc-1", &caps)).unwrap();
        let v = parse(&json);
        assert_eq!(v["format"], "urn:x-nmos:format:data");
        assert_eq!(v["media_type"], "video/smpte291");
        assert_eq!(v["grain_rate"]["numerator"], 30);
        assert_eq!(v["grain_rate"]["denominator"], 1);
        assert_eq!(
            v["tags"]["urn:x-nmos:tag:grouphint/v1.0"],
            serde_json::json!(["anc-1:Ancillary Data"]),
        );
    }

    #[test]
    fn build_data_st2038_without_framerate_is_hard_error() {
        let caps = caps_from_str("meta/x-st-2038");
        let err = build_from_caps(&input(UUID_A, "anc-1", &caps)).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, FlowDefBuildError::MissingCapsField { field: "framerate", .. }));
        assert!(msg.contains("capsfilter"), "missing-framerate error guides the user: {msg}");
    }

    #[test]
    fn missing_flow_id_is_hard_error() {
        let caps =
            caps_from_str("video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001");
        let err = build_from_caps(&input("", "cam-1", &caps)).unwrap_err();
        assert!(matches!(err, FlowDefBuildError::MissingFlowId), "got: {err:?}");
    }

    #[test]
    fn missing_name_is_hard_error() {
        let caps =
            caps_from_str("video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001");
        let err = build_from_caps(&input(UUID_A, "", &caps)).unwrap_err();
        assert!(matches!(err, FlowDefBuildError::MissingName), "got: {err:?}");
    }

    #[test]
    fn missing_mxl_domain_id_is_hard_error() {
        let caps =
            caps_from_str("video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001");
        let err = build_from_caps(&FlowDefBuildInput {
            mxl_domain_id: "",
            ..input(UUID_A, "cam-1", &caps)
        })
        .unwrap_err();
        assert!(matches!(err, FlowDefBuildError::MissingMxlDomainId), "got: {err:?}");
    }

    #[test]
    fn unsupported_video_format_is_hard_error() {
        let caps =
            caps_from_str("video/x-raw,format=I420,width=1920,height=1080,framerate=30000/1001");
        let err = build_from_caps(&input(UUID_A, "cam-1", &caps)).unwrap_err();
        assert!(
            matches!(err, FlowDefBuildError::UnsupportedCapsValue { field: "format", .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn unsupported_caps_is_hard_error() {
        let caps = caps_from_str("application/x-rtp,media=video");
        let err = build_from_caps(&input(UUID_A, "cam-1", &caps)).unwrap_err();
        assert!(matches!(err, FlowDefBuildError::UnsupportedCaps(_)), "got: {err:?}");
    }

    #[test]
    fn missing_video_field_is_hard_error() {
        let caps = caps_from_str("video/x-raw,format=v210,width=1920,height=1080");
        let err = build_from_caps(&input(UUID_A, "cam-1", &caps)).unwrap_err();
        assert!(
            matches!(err, FlowDefBuildError::MissingCapsField { field: "framerate", .. }),
            "got: {err:?}",
        );
    }

    #[test]
    fn build_then_parse_round_trips_through_read_flow_def_meta() {
        let caps =
            caps_from_str("video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001");
        let json = build_from_caps(&input(UUID_B, "cam-1", &caps)).unwrap();
        let meta = read_flow_def_meta(&json).unwrap();
        assert_eq!(meta.id, UUID_B);
        assert_eq!(meta.format, FlowFormat::Video);
    }

    mod caps_from_flow_def {
        use super::*;

        #[test]
        fn video_v210() {
            ensure_gst_initialised();
            let json = r#"{
                "id": "00000000-0000-0000-0000-000000000001",
                "format": "urn:x-nmos:format:video",
                "media_type": "video/v210",
                "grain_rate": { "numerator": 60000, "denominator": 1001 },
                "frame_width": 1920,
                "frame_height": 1080
            }"#;
            let caps = super::caps_from_flow_def(json).unwrap();
            let s = caps.structure(0).expect("structure");
            assert_eq!(s.name().as_str(), "video/x-raw");
            assert_eq!(s.get::<String>("format").unwrap(), "v210");
            assert_eq!(s.get::<i32>("width").unwrap(), 1920);
            assert_eq!(s.get::<i32>("height").unwrap(), 1080);
            let fr = s.get::<gst::Fraction>("framerate").unwrap();
            assert_eq!((fr.numer(), fr.denom()), (60000, 1001));
            assert!(s.get::<String>("interlace-mode").is_err());
        }

        #[test]
        fn video_v210_with_interlace_mode() {
            ensure_gst_initialised();
            let json = r#"{
                "id": "00000000-0000-0000-0000-000000000001",
                "format": "urn:x-nmos:format:video",
                "media_type": "video/v210",
                "grain_rate": { "numerator": 25, "denominator": 1 },
                "frame_width": 720, "frame_height": 576,
                "interlace_mode": "interleaved"
            }"#;
            let caps = super::caps_from_flow_def(json).unwrap();
            let s = caps.structure(0).expect("structure");
            assert_eq!(s.get::<String>("interlace-mode").unwrap(), "interleaved");
        }

        #[test]
        fn audio_f32le() {
            ensure_gst_initialised();
            let json = r#"{
                "id": "00000000-0000-0000-0000-000000000001",
                "format": "urn:x-nmos:format:audio",
                "media_type": "audio/float32",
                "sample_rate": { "numerator": 48000, "denominator": 1 },
                "channel_count": 2,
                "bit_depth": 32
            }"#;
            let caps = super::caps_from_flow_def(json).unwrap();
            let s = caps.structure(0).expect("structure");
            assert_eq!(s.name().as_str(), "audio/x-raw");
            assert_eq!(s.get::<String>("format").unwrap(), "F32LE");
            assert_eq!(s.get::<i32>("rate").unwrap(), 48000);
            assert_eq!(s.get::<i32>("channels").unwrap(), 2);
            assert_eq!(s.get::<String>("layout").unwrap(), "interleaved");
        }

        #[test]
        fn data_st2038() {
            ensure_gst_initialised();
            let json = r#"{
                "id": "00000000-0000-0000-0000-000000000001",
                "format": "urn:x-nmos:format:data",
                "media_type": "video/smpte291",
                "grain_rate": { "numerator": 30, "denominator": 1 }
            }"#;
            let caps = super::caps_from_flow_def(json).unwrap();
            let s = caps.structure(0).expect("structure");
            assert_eq!(s.name().as_str(), "meta/x-st-2038");
            let fr = s.get::<gst::Fraction>("framerate").unwrap();
            assert_eq!((fr.numer(), fr.denom()), (30, 1));
        }

        #[test]
        fn missing_media_type_is_error() {
            let err = super::caps_from_flow_def(r#"{"id":"a"}"#).unwrap_err();
            assert!(
                matches!(err, FlowDefCapsError::MissingField("media_type")),
                "got: {err:?}"
            );
        }

        #[test]
        fn unsupported_media_type_is_error() {
            let err = super::caps_from_flow_def(r#"{"media_type":"image/jpeg"}"#).unwrap_err();
            assert!(matches!(err, FlowDefCapsError::UnsupportedMediaType(_)), "got: {err:?}");
        }

        #[test]
        fn missing_grain_rate_for_video_is_error() {
            let err = super::caps_from_flow_def(
                r#"{"media_type":"video/v210","frame_width":1920,"frame_height":1080}"#,
            )
            .unwrap_err();
            assert!(
                matches!(err, FlowDefCapsError::MissingField("grain_rate")),
                "got: {err:?}"
            );
        }

        #[test]
        fn missing_frame_width_for_video_is_error() {
            let err = super::caps_from_flow_def(
                r#"{"media_type":"video/v210","grain_rate":{"numerator":30,"denominator":1},"frame_height":1080}"#,
            )
            .unwrap_err();
            assert!(
                matches!(err, FlowDefCapsError::MissingField("frame_width")),
                "got: {err:?}"
            );
        }

        #[test]
        fn parse_error_is_surfaced() {
            let err = super::caps_from_flow_def("not json").unwrap_err();
            assert!(matches!(err, FlowDefCapsError::Parse { .. }), "got: {err:?}");
        }

        #[test]
        fn round_trip_with_build_from_caps_video() {
            ensure_gst_initialised();
            let original_caps =
                caps_from_str("video/x-raw,format=v210,width=1920,height=1080,framerate=30000/1001");
            let json = build_from_caps(&input(UUID_A, "cam-1", &original_caps)).unwrap();
            let derived = super::caps_from_flow_def(&json).unwrap();
            // Both should agree on the structure name + the essence
            // fields the builder emits (framerate, width, height,
            // format).
            assert!(
                derived.can_intersect(&original_caps),
                "derived caps `{derived}` should intersect original `{original_caps}`"
            );
        }

        #[test]
        fn round_trip_with_build_from_caps_audio() {
            ensure_gst_initialised();
            let original_caps =
                caps_from_str("audio/x-raw,format=F32LE,rate=48000,channels=2,layout=interleaved");
            let json = build_from_caps(&input(UUID_A, "mic-1", &original_caps)).unwrap();
            let derived = super::caps_from_flow_def(&json).unwrap();
            assert!(
                derived.can_intersect(&original_caps),
                "derived caps `{derived}` should intersect original `{original_caps}`"
            );
        }

        #[test]
        fn round_trip_with_build_from_caps_data() {
            ensure_gst_initialised();
            let original_caps = caps_from_str("meta/x-st-2038,framerate=30/1");
            let json = build_from_caps(&input(UUID_A, "anc-1", &original_caps)).unwrap();
            let derived = super::caps_from_flow_def(&json).unwrap();
            assert!(
                derived.can_intersect(&original_caps),
                "derived caps `{derived}` should intersect original `{original_caps}`"
            );
        }
    }
}
