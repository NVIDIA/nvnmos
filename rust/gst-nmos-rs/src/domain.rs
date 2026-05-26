// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Translation between the `mxl-domain-id` element property and the
//! filesystem path the inner `mxlsink` or `mxlsrc` element expects in
//! its `domain` property.
//!
//! `mxl-domain-id` is an MXL Domain identifier (carried on MXL
//! flow_def `tags` under `urn:x-nvnmos:tag:mxl-domain-id`). The inner
//! element takes a filesystem path. There is no MXL Domain registry
//! visible to either us or the daemon today, so this is a placeholder
//! that passes the value through verbatim.

/// Stub: translate an MXL Domain id to the filesystem path expected
/// by `mxlsink` or `mxlsrc`. Currently returns the input unchanged.
#[allow(dead_code)]
pub(crate) fn domain_id_to_path(domain_id: &str) -> String {
    domain_id.to_owned()
}
