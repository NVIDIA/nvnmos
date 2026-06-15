// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! MXL transport session setup and activation.

use anyhow::{Context, bail};
use gstreamer as gst;

use super::{
    ActivationAck, ActivationPlan, CommonSettings, FakeKind, InnerConfig, TransportConfig,
    caps_format,
};
use super::types::Side;
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
            kind: FakeKind::Misconfigured,
            detail: "`mxl-domain-path` unset".into(),
        };
    }
    if flow.id.is_empty() {
        return InnerConfig::Fake {
            kind: FakeKind::Misconfigured,
            detail: "`mxl-flow-id` unset (neither property nor transport file supplied it)".into(),
        };
    }
    if settings.side == Side::Receiver && flow.format == FlowFormat::Unspecified {
        return InnerConfig::Fake {
            kind: FakeKind::Misconfigured,
            detail: "`caps` media-type unrecognised or unset on nmossrc \
                      (neither caps nor transport file pinned a flow format)"
                .into(),
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
            kind: FakeKind::NotConfigured,
            detail: String::new(),
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
                        kind: FakeKind::Misconfigured,
                        detail: "mxl-domain-id resolution failed".into(),
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
                kind: FakeKind::Misconfigured,
                detail: "mxl-domain-id unresolved".into(),
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
                    kind: FakeKind::Misconfigured,
                    detail: "flow_def resolution failed".into(),
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

#[cfg(test)]
mod tests {
    use super::super::support::*;
    use super::super::*;
    use super::*;
    use std::sync::Mutex;

    mod transport_config {
        use super::*;

        #[test]
        fn transport_file_mxl_present() {
            let tc = TransportConfig::Mxl {
                domain_path: "/var/lib/mxl/x".to_owned(),
                flow_id: FLOW_ID_A.to_owned(),
                format: FlowFormat::Video,
                transport_file: Some("payload".to_owned()),
            };
            assert_eq!(tc.transport_file(), Some("payload"));
        }

        #[test]
        fn transport_file_mxl_absent() {
            let tc = TransportConfig::Mxl {
                domain_path: "/var/lib/mxl/x".to_owned(),
                flow_id: FLOW_ID_A.to_owned(),
                format: FlowFormat::Video,
                transport_file: None,
            };
            assert_eq!(tc.transport_file(), None);
        }
    }

    #[test]
    fn setup_property_overrides_file_flow_id() {
        let s = CommonSettings {
            mxl_flow_id: FLOW_ID_B.to_owned(),
            ..settings(Side::Sender)
        };
        let overrides = super::property_overrides_mxl(&s, DOMAIN_ID);
        let spliced =
            flow_def::splice_overrides(&video_flow_def(FLOW_ID_A), &overrides).unwrap();
        let v: serde_json::Value = serde_json::from_str(&spliced).unwrap();
        assert_eq!(v["id"], FLOW_ID_B);
        // Subsequent resolve_mxl_flow_meta with property==B and
        // file==B agrees silently; the previous "hard error on
        // mismatch" branch is no longer reachable from the setup
        // path.
        let resolved =
            flow_def::resolve_mxl_flow_meta(FLOW_ID_B, FlowFormat::Video, Some(&spliced)).unwrap();
        assert_eq!(resolved.id, FLOW_ID_B);
        assert_eq!(resolved.id_origin, flow_def::ValueOrigin::Both);
    }

    #[test]
    fn deactivation_is_fake_success() {
        let plan = make_activation_plan(&cat(), "nmossink", &settings(Side::Sender), &req(Side::Sender, None));
        assert!(matches!(plan.inner, InnerConfig::Fake { .. }));
        assert!(matches!(plan.ack, ActivationAck::Success));
    }

    #[test]
    fn side_mismatch_is_failure() {
        let plan = make_activation_plan(
            &cat(),
            "nmossink",
            &settings(Side::Sender),
            &req(Side::Receiver, Some(&video_flow_def(FLOW_ID_A))),
        );
        assert!(matches!(plan.inner, InnerConfig::Fake { .. }));
        match plan.ack {
            ActivationAck::Failure { reason } => assert!(
                reason.contains("side mismatch") || reason.contains("does not match"),
                "expected side-mismatch reason: {reason}"
            ),
            ActivationAck::Success => panic!("expected failure ack on side mismatch"),
        }
    }

    #[test]
    fn nmossrc_caps_st2038_drives_data_format() {
        use std::str::FromStr;
        let caps = gst::Caps::from_str("meta/x-st-2038,framerate=30/1")
            .expect("static caps parse");
        let s = CommonSettings {
            mxl_flow_id: FLOW_ID_A.to_owned(),
            caps: Some(caps),
            ..settings(Side::Receiver)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossrc",
            &s,
            &req(
                Side::Receiver,
                Some(r#"{"id":"00000000-0000-0000-0000-000000000001","format":"urn:x-nmos:format:data"}"#),
            ),
        );
        match plan.inner {
            InnerConfig::Real(TransportConfig::Mxl { format, .. }) => {
                assert_eq!(format, FlowFormat::Data)
            }
            InnerConfig::Real(TransportConfig::Udp { .. })
            | InnerConfig::Real(TransportConfig::NvDsUdp { .. }) => {
                panic!("expected Real(Mxl(data)), got Real(RTP transport)")
            }
            InnerConfig::Fake { kind, .. } => {
                panic!("expected Real(Mxl(data)), got Fake({kind})")
            }
        }
        assert!(matches!(plan.ack, ActivationAck::Success));
    }

    #[test]
    fn nmossrc_caps_unset_falls_back_to_fake() {
        // Receiver with neither `caps` nor a transport file `format`
        // can't pick a `mxlsrc` slot, so it stays on the fake chain.
        let s = CommonSettings {
            mxl_flow_id: FLOW_ID_A.to_owned(),
            ..settings(Side::Receiver)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossrc",
            &s,
            &req(
                Side::Receiver,
                Some(r#"{"id":"00000000-0000-0000-0000-000000000001"}"#),
            ),
        );
        match plan.inner {
            InnerConfig::Fake { kind, detail } => {
                assert_eq!(kind, FakeKind::Misconfigured);
                assert!(
                    detail.contains("caps") && detail.contains("flow format"),
                    "expected caps-driven detail: {detail}",
                );
            }
            InnerConfig::Real(_) => panic!("expected Fake, got Real"),
        }
    }

    #[test]
    fn happy_path_video_is_real_success() {
        let s = CommonSettings {
            mxl_flow_id: FLOW_ID_A.to_owned(),
            caps: Some(video_caps()),
            ..settings(Side::Receiver)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossrc",
            &s,
            &req(Side::Receiver, Some(&video_flow_def(FLOW_ID_A))),
        );
        match plan.inner {
            InnerConfig::Real(TransportConfig::Mxl {
                domain_path, flow_id, format, transport_file,
            }) => {
                assert_eq!(domain_path, "/var/lib/mxl/domain-a");
                assert_eq!(flow_id, FLOW_ID_A);
                assert_eq!(format, FlowFormat::Video);
                assert!(
                    transport_file.is_some(),
                    "make_activation_plan must thread req.transport_file into InnerConfig",
                );
            }
            InnerConfig::Real(TransportConfig::Udp { .. })
            | InnerConfig::Real(TransportConfig::NvDsUdp { .. }) => {
                panic!("expected Real(Mxl), got Real(RTP transport)")
            }
            InnerConfig::Fake { kind, .. } => panic!("expected Real(Mxl), got Fake({kind})"),
        }
        assert!(matches!(plan.ack, ActivationAck::Success));
    }

    /// IS-05 PATCHes legitimately replace the flow id the element
    /// was configured with at NULL→READY. The activation's
    /// transport file is authoritative, so the activation must
    /// silently succeed and the inner be reconfigured against the
    /// new flow id.
    #[test]
    fn activation_flow_id_overrides_element_property() {
        let s = CommonSettings {
            mxl_flow_id: FLOW_ID_B.to_owned(),
            ..settings(Side::Sender)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossink",
            &s,
            &req(Side::Sender, Some(&video_flow_def(FLOW_ID_A))),
        );
        match (&plan.inner, &plan.ack) {
            (InnerConfig::Real(TransportConfig::Mxl { flow_id, .. }), ActivationAck::Success) => {
                assert_eq!(flow_id, FLOW_ID_A, "activation file's id must win");
            }
            other => panic!("expected ack-success + inner using FLOW_ID_A, got: {other:?}"),
        }
    }

    #[test]
    fn domain_path_unset_is_failure_with_live_transport_file() {
        // Activation supplies the spliced transport file, but this
        // host has no `mxl-domain-path` so the element can't bring
        // up mxlsink/mxlsrc. Per design: fake chain + failure ack.
        let s = CommonSettings {
            mxl_domain_path: String::new(),
            mxl_flow_id: FLOW_ID_A.to_owned(),
            ..settings(Side::Sender)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossink",
            &s,
            &req(Side::Sender, Some(&video_flow_def(FLOW_ID_A))),
        );
        match plan.inner {
            InnerConfig::Fake { kind, detail } => {
                assert_eq!(kind, FakeKind::Misconfigured);
                assert!(
                    detail.contains("mxl-domain-path"),
                    "expected mxl-domain-path detail: {detail}",
                );
            }
            InnerConfig::Real(_) => panic!("expected Fake when mxl-domain-path unset"),
        }
        match plan.ack {
            ActivationAck::Failure { reason } => assert!(
                reason.contains("cannot bring up inner data path")
                    && reason.contains("mxl-domain-path"),
                "expected user-facing failure reason: {reason}",
            ),
            ActivationAck::Success => panic!(
                "expected failure ack when activation can't be honoured locally; got Success",
            ),
        }
    }

    #[test]
    fn domain_id_unresolvable_is_failure() {
        let s = CommonSettings {
            mxl_domain_id: String::new(),
            mxl_domain_path: String::new(),
            mxl_flow_id: FLOW_ID_A.to_owned(),
            ..settings(Side::Sender)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossink",
            &s,
            &req(Side::Sender, Some(&video_flow_def(FLOW_ID_A))),
        );
        match plan.ack {
            ActivationAck::Failure { reason } => assert!(
                reason.contains("mxl-domain-id"),
                "expected mxl-domain-id failure reason: {reason}",
            ),
            ActivationAck::Success => {
                panic!("expected failure ack when mxl-domain-id is unresolvable")
            }
        }
    }

    #[test]
    fn bad_transport_file_json_is_failure() {
        let s = CommonSettings {
            mxl_flow_id: FLOW_ID_A.to_owned(),
            ..settings(Side::Sender)
        };
        let plan = make_activation_plan(
            &cat(),
            "nmossink",
            &s,
            &req(Side::Sender, Some("not json")),
        );
        assert!(matches!(plan.inner, InnerConfig::Fake { .. }));
        assert!(matches!(plan.ack, ActivationAck::Failure { .. }));
    }

    mod register_deferred {
        use super::*;
        use std::str::FromStr;

        fn no_session() -> Mutex<Option<Session>> {
            Mutex::new(None)
        }

        fn good_caps() -> gst::Caps {
            cat(); // ensures gst::init() ran
            gst::Caps::from_str(
                "video/x-raw,format=v210,width=1920,height=1080,framerate=50/1,\
                 interlace-mode=progressive,pixel-aspect-ratio=1/1",
            )
            .expect("static caps parse")
        }

        fn sender_settings() -> CommonSettings {
            CommonSettings {
                mxl_flow_id: FLOW_ID_A.to_owned(),
                ..settings(Side::Sender)
            }
        }

        #[test]
        fn empty_caps_is_error() {
            let res = register_deferred(
                &cat(),
                "nmossink",
                &sender_settings(),
                &no_session(),
                gst::Caps::new_empty(),
            );
            let err = res.expect_err("empty caps must be rejected");
            assert!(
                format!("{err:#}").contains("offered no caps"),
                "expected EMPTY-caps reason: {err:#}"
            );
        }

        #[test]
        fn any_caps_is_error() {
            let res = register_deferred(
                &cat(),
                "nmossink",
                &sender_settings(),
                &no_session(),
                gst::Caps::new_any(),
            );
            let err = res.expect_err("ANY caps must be rejected");
            assert!(
                format!("{err:#}").contains("ANY caps"),
                "expected ANY-caps reason: {err:#}"
            );
        }

        #[test]
        fn wrong_side_is_error() {
            // Receiver deferred mode is explicitly out of scope.
            let s = CommonSettings {
                side: Side::Receiver,
                ..sender_settings()
            };
            let res = register_deferred(&cat(), "nmossrc", &s, &no_session(), good_caps());
            let err = res.expect_err("receiver deferred mode is out of scope");
            assert!(
                format!("{err:#}").contains("sender-only"),
                "expected sender-only reason: {err:#}"
            );
        }

        #[test]
        fn missing_domain_id_is_error() {
            let s = CommonSettings {
                mxl_domain_id: String::new(),
                mxl_domain_path: String::new(),
                ..sender_settings()
            };
            let res = register_deferred(&cat(), "nmossink", &s, &no_session(), good_caps());
            let err = res.expect_err("missing mxl-domain-id must be rejected");
            assert!(
                format!("{err:#}").contains("mxl-domain-id"),
                "expected mxl-domain-id reason: {err:#}"
            );
        }

        #[test]
        fn missing_flow_id_is_error_via_builder() {
            let s = CommonSettings {
                mxl_flow_id: String::new(),
                ..sender_settings()
            };
            let res = register_deferred(&cat(), "nmossink", &s, &no_session(), good_caps());
            let err = res.expect_err("missing mxl-flow-id must be rejected");
            assert!(
                format!("{err:#}").contains("flow_id") || format!("{err:#}").contains("flow-id"),
                "expected mxl-flow-id reason: {err:#}"
            );
        }

        #[test]
        fn unsupported_caps_shape_is_error_via_builder() {
            // I420 isn't in the MXL pad template; the builder must
            // reject it, and the user is expected to add a capsfilter.
            let caps = gst::Caps::from_str("video/x-raw,format=I420,width=1920,height=1080")
                .expect("static caps parse");
            let res = register_deferred(&cat(), "nmossink", &sender_settings(), &no_session(), caps);
            let err = res.expect_err("unsupported caps must be rejected");
            // exact message is owned by from_caps; we just want
            // the synthesis-context wrapper to be present.
            assert!(
                format!("{err:#}").contains("synthesising flow_def"),
                "expected synthesis context in error: {err:#}"
            );
        }

        #[test]
        fn no_open_session_is_error() {
            // Caps are valid and validation passes; we should reach
            // the session-take step and surface a clear error.
            let res = register_deferred(
                &cat(),
                "nmossink",
                &sender_settings(),
                &no_session(),
                good_caps(),
            );
            let err = res.expect_err("missing session must be reported");
            assert!(
                format!("{err:#}").contains("no open session"),
                "expected no-open-session reason: {err:#}"
            );
        }
    }

    mod synthesis {
        use super::*;

        fn parse(json: &str) -> serde_json::Value {
            serde_json::from_str(json).expect("synthesised JSON must parse")
        }

        /// Caps + `mxl-flow-id` on a Receiver synthesises a configuring
        /// flow_def the daemon can use to advertise narrow Receiver
        /// Caps on IS-04. The synthesised shape matches what the
        /// equivalent Sender call would produce — `from_caps`
        /// is symmetric.
        #[test]
        fn receiver_caps_and_flow_id_synthesise_flow_def() {
            let s = CommonSettings {
                mxl_flow_id: FLOW_ID_A.to_owned(),
                name: "video-receiver".to_owned(),
                label: "Studio A camera".to_owned(),
                description: "v210 1080p50".to_owned(),
                caps: Some(video_caps()),
                ..settings(Side::Receiver)
            };
            let out = super::synthesise_or_passthrough_mxl(&cat(), "nmossrc", &s, DOMAIN_ID, None)
                .expect("synthesis must succeed");
            let text = out.expect("Receiver synthesis must yield Some(json) when caps + flow id are set");
            let v = parse(&text);
            assert_eq!(v["id"], FLOW_ID_A);
            assert_eq!(v["format"], "urn:x-nmos:format:video");
            assert_eq!(v["media_type"], "video/v210");
            assert_eq!(v["label"], "Studio A camera");
            assert_eq!(v["description"], "v210 1080p50");
            assert_eq!(v["tags"]["urn:x-nvnmos:tag:name"][0], "video-receiver");
            assert_eq!(v["tags"]["urn:x-nvnmos:tag:mxl-domain-id"][0], DOMAIN_ID);
        }

        /// Receiver synthesis is gated on `mxl-flow-id`: without it
        /// we have nothing to subscribe to and no stable id for the
        /// configuring flow_def. Returning `None` puts the element
        /// on the fake chain until an IS-05 activation supplies the
        /// missing piece.
        #[test]
        fn receiver_caps_without_flow_id_returns_none() {
            let s = CommonSettings {
                caps: Some(video_caps()),
                ..settings(Side::Receiver)
            };
            assert!(s.mxl_flow_id.is_empty(), "test precondition");
            let out = super::synthesise_or_passthrough_mxl(&cat(), "nmossrc", &s, DOMAIN_ID, None)
                .expect("absent flow id must not error");
            assert!(out.is_none(), "Receiver without flow id must not synthesise");
        }

        /// Sender synthesis still works the same way. Sanity check
        /// against future refactors of the shared arm.
        #[test]
        fn sender_caps_and_flow_id_synthesise_flow_def() {
            let s = CommonSettings {
                mxl_flow_id: FLOW_ID_A.to_owned(),
                name: "video-sender".to_owned(),
                caps: Some(video_caps()),
                ..settings(Side::Sender)
            };
            let out = super::synthesise_or_passthrough_mxl(&cat(), "nmossink", &s, DOMAIN_ID, None)
                .expect("Sender synthesis must succeed");
            let v = parse(&out.expect("Sender synthesis yields Some(json)"));
            assert_eq!(v["id"], FLOW_ID_A);
            assert_eq!(v["tags"]["urn:x-nvnmos:tag:name"][0], "video-sender");
        }

        /// When the user supplies a literal transport file, it is
        /// passed through verbatim regardless of side or whether
        /// `caps` is also set (caps cross-check happens further down).
        #[test]
        fn passthrough_wins_over_caps_synthesis() {
            let s = CommonSettings {
                mxl_flow_id: FLOW_ID_B.to_owned(),
                caps: Some(video_caps()),
                ..settings(Side::Receiver)
            };
            let resolved = Some(video_flow_def(FLOW_ID_A));
            let out = super::synthesise_or_passthrough_mxl(&cat(), "nmossrc", &s, DOMAIN_ID, resolved.clone())
                .expect("passthrough must succeed");
            assert_eq!(
                out.as_deref(),
                resolved.as_deref(),
                "transport file must pass through unchanged when supplied"
            );
        }
    }
}
