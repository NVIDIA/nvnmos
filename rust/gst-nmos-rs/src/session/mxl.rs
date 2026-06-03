// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! MXL transport session setup and activation.

use anyhow::{Context, bail};
use gstreamer as gst;

use super::{
    ActivationAck, ActivationPlan, CommonSettings, InnerConfig, Side, TransportConfig,
    caps_format,
};
use crate::domain::{self, DomainIdOrigin};
use crate::flow_def::{self, FlowDefBuildInput, FlowDefOverrides, ValueOrigin};
use crate::types::FlowFormat;

pub(crate) fn synthesise_or_passthrough_mxl(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    resolved_mxl_domain_id: &str,
    resolved: Option<String>,
) -> Result<Option<String>, anyhow::Error> {
    match (resolved, settings.caps.as_ref()) {
        (Some(text), Some(_)) => {
            gst::debug!(
                cat,
                "{element}: transport-file set; `caps` will be cross-checked against the file's `format`"
            );
            Ok(Some(text))
        }
        (Some(text), None) => Ok(Some(text)),
        (None, Some(caps)) => {
            if settings.mxl_flow_id.is_empty() {
                gst::debug!(
                    cat,
                    "{element}: `caps` set but `mxl-flow-id` empty; deferring flow_def \
                     synthesis (the fake chain will be in use until an IS-05 \
                     activation supplies the flow id)"
                );
                return Ok(None);
            }
            let json = flow_def::from_caps(&FlowDefBuildInput {
                flow_id: &settings.mxl_flow_id,
                name: &settings.name,
                mxl_domain_id: resolved_mxl_domain_id,
                label: &settings.label,
                description: &settings.description,
                caps,
            })
            .with_context(|| format!("{element}: synthesising flow_def from caps"))?;
            gst::info!(
                cat,
                "{element}: synthesised flow_def from `caps` (side={:?})",
                settings.side,
            );
            Ok(Some(json))
        }
        (None, None) => Ok(None),
    }
}
pub(crate) fn property_overrides_mxl<'a>(
    settings: &'a CommonSettings,
    resolved_mxl_domain_id: &'a str,
) -> FlowDefOverrides<'a> {
    fn opt(s: &str) -> Option<&str> {
        if s.is_empty() { None } else { Some(s) }
    }
    FlowDefOverrides {
        flow_id: opt(&settings.mxl_flow_id),
        label: opt(&settings.label),
        description: opt(&settings.description),
        name: opt(&settings.name),
        mxl_domain_id: opt(resolved_mxl_domain_id),
        caps_mode: settings.caps_mode,
    }
}

pub(super) fn log_flow_origin(cat: &gst::DebugCategory, field: &str, origin: ValueOrigin) {
    match origin {
        ValueOrigin::Property => gst::debug!(cat, "{field} from property; no transport file constraint"),
        ValueOrigin::File => gst::info!(cat, "{field} taken from transport file"),
        ValueOrigin::Both => gst::debug!(cat, "{field} cross-checked against transport file"),
        ValueOrigin::None => gst::debug!(cat, "{field} not supplied by either source"),
    }
}
pub(crate) fn decide_inner_config_mxl(
    settings: &CommonSettings,
    flow: &flow_def::FlowResolution,
    transport_file: Option<&str>,
) -> InnerConfig {
    if settings.mxl_domain_path.is_empty() {
        return InnerConfig::Fake {
            reason: "`mxl-domain-path` unset".to_owned(),
        };
    }
    if flow.id.is_empty() {
        return InnerConfig::Fake {
            reason: "`mxl-flow-id` unset (neither property nor transport file supplied it)".to_owned(),
        };
    }
    if settings.side == Side::Receiver && flow.format == FlowFormat::Unspecified {
        return InnerConfig::Fake {
            reason:
                "`caps` media-type unrecognised or unset on nmossrc \
                 (neither caps nor transport file pinned a flow format)"
                    .to_owned(),
        };
    }
    InnerConfig::Real(TransportConfig::Mxl {
        domain_path: settings.mxl_domain_path.clone(),
        flow_id: flow.id.clone(),
        format: flow.format,
        transport_file: transport_file.map(str::to_owned),
    })
}
pub(super) fn resolve_inner_config_mxl(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    resolved_transport_file: Option<String>,
) -> Result<(InnerConfig, Option<String>), anyhow::Error> {
    let domain_resolution =
        domain::resolve_mxl_domain_id(&settings.mxl_domain_id, &settings.mxl_domain_path)
            .with_context(|| format!("{element}: resolving MXL Domain identity"))?;
    if domain_resolution.id.is_empty() {
        bail!(
            "{element}: `mxl-domain-id` is required when transport=mxl \
             (set the property directly or supply an `mxl-domain-path` whose `domain_def.json` provides the id)"
        );
    }
    match domain_resolution.origin {
        DomainIdOrigin::Property => gst::debug!(
            cat,
            "mxl-domain-id from property; no `domain_def.json` consulted",
        ),
        DomainIdOrigin::DomainDef => gst::info!(
            cat,
            "mxl-domain-id taken from `domain_def.json` at `{}`",
            settings.mxl_domain_path,
        ),
        DomainIdOrigin::Both => gst::debug!(
            cat,
            "mxl-domain-id cross-checked against `domain_def.json` at `{}`",
            settings.mxl_domain_path,
        ),
        DomainIdOrigin::None => unreachable!("empty id rejected above"),
    }

    let transport_file = synthesise_or_passthrough_mxl(
        cat,
        element,
        settings,
        &domain_resolution.id,
        resolved_transport_file,
    )?;

    // Property-overrides-file: splice any user-set identity/cosmetic
    // properties (name, flow_id, mxl-domain-id, label, description,
    // receiver-caps-mode) into the transport file before the daemon
    // sees it. `caps` and `transport-caps` remain cross-checked by
    // `resolve_mxl_flow_meta` below — they describe the essence
    // shape and a mismatch is a real error.
    let transport_file = match transport_file {
        Some(text) => Some(
            flow_def::splice_overrides(&text, &property_overrides_mxl(settings, &domain_resolution.id))
                .with_context(|| format!("{element}: splicing property overrides into transport file"))?,
        ),
        None => None,
    };

    let caps_format = caps_format(settings);
    let flow = flow_def::resolve_mxl_flow_meta(
        &settings.mxl_flow_id,
        caps_format,
        transport_file.as_deref(),
    )
    .with_context(|| format!("{element}: resolving MXL flow id / format"))?;
    log_flow_origin(cat, "mxl-flow-id", flow.id_origin);
    log_flow_origin(cat, "caps format", flow.format_origin);

    let mut inner = decide_inner_config_mxl(settings, &flow, transport_file.as_deref());
    // Deferred-mode case (sender only): no resource is going to be
    // registered at NULL→READY because neither `transport-file*` nor
    // `caps` was supplied. Keep the fake chain so we don't bring
    // `mxlsink` up against an unregistered Flow (which would fail to
    // preroll); the inner is swapped to `mxlsink` only after
    // `register_deferred` registers the Sender at READY→PAUSED.
    if transport_file.is_none()
        && settings.side == Side::Sender
        && matches!(inner, InnerConfig::Real(_))
    {
        inner = InnerConfig::Fake {
            reason: "deferred — peer caps will drive registration at READY\u{2192}PAUSED"
                .to_owned(),
        };
    }

    Ok((inner, transport_file))
}
pub(super) fn resolve_activation_inner_mxl(
    cat: &gst::DebugCategory,
    element: &str,
    settings: &CommonSettings,
    transport_file: &str,
) -> Result<InnerConfig, Box<ActivationPlan>> {
    let domain_resolution =
        match domain::resolve_mxl_domain_id(&settings.mxl_domain_id, &settings.mxl_domain_path) {
            Ok(r) => r,
            Err(e) => {
                return Err(Box::new(ActivationPlan {
                    inner: InnerConfig::Fake {
                        reason: "mxl-domain-id resolution failed".to_owned(),
                    },
                    ack: ActivationAck::Failure {
                        reason: format!(
                            "{element}: resolving MXL Domain identity for activation: {e:#}"
                        ),
                    },
                }));
            }
        };
    if domain_resolution.id.is_empty() {
        return Err(Box::new(ActivationPlan {
            inner: InnerConfig::Fake {
                reason: "mxl-domain-id unresolved".to_owned(),
            },
            ack: ActivationAck::Failure {
                reason: format!(
                    "{element}: activation rejected — `mxl-domain-id` is not resolvable on this \
                     host (neither the property nor `mxl-domain-path`/`domain_def.json` \
                     supplied an id)",
                ),
            },
        }));
    }
    match domain_resolution.origin {
        DomainIdOrigin::Property | DomainIdOrigin::DomainDef | DomainIdOrigin::Both => gst::debug!(
            cat,
            "{element}: activation mxl-domain-id resolved (origin={:?})",
            domain_resolution.origin,
        ),
        DomainIdOrigin::None => unreachable!("empty id handled above"),
    }

    // Activation: the daemon's transport file is authoritative. Pass
    // an empty `property_id` so the file always wins silently (the
    // element's `mxl-flow-id` property is just a NULL→READY default;
    // an IS-05 PATCH legitimately replaces it). The `caps` format
    // cross-check stays because a v210 video activation arriving at
    // an `nmossrc` configured for audio is a real misconfiguration
    // the element must ack-fail.
    let flow = match flow_def::resolve_mxl_flow_meta(
        "",
        caps_format(settings),
        Some(transport_file),
    ) {
        Ok(r) => r,
        Err(e) => {
            return Err(Box::new(ActivationPlan {
                inner: InnerConfig::Fake {
                    reason: "flow_def resolution failed".to_owned(),
                },
                ack: ActivationAck::Failure {
                    reason: format!(
                        "{element}: resolving MXL flow id / format from activation \
                         transport file: {e:#}"
                    ),
                },
            }));
        }
    };

    Ok(decide_inner_config_mxl(settings, &flow, Some(transport_file)))
}
