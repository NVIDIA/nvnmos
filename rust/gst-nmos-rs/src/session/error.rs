// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The GStreamer error code an element posts when a state change fails.

use gstreamer as gst;

use crate::daemon::DaemonError;

/// The resource error code for a failure caused by nvnmosd, or `None` when
/// nvnmosd was not involved and the caller's own code applies.
pub(crate) fn resource_error(err: &anyhow::Error) -> Option<gst::ResourceError> {
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<DaemonError>() {
            return Some(match e {
                DaemonError::Transport(_) => gst::ResourceError::NotFound,
                DaemonError::UnsupportedScheme(_) | DaemonError::NodeProperties(_) => {
                    gst::ResourceError::Settings
                }
                DaemonError::Rpc(status) => match status.code() {
                    tonic::Code::InvalidArgument | tonic::Code::AlreadyExists => {
                        gst::ResourceError::Settings
                    }
                    tonic::Code::Unavailable | tonic::Code::ResourceExhausted => {
                        gst::ResourceError::Busy
                    }
                    _ => gst::ResourceError::OpenReadWrite,
                },
                DaemonError::AlreadyAdded | DaemonError::NoResource => {
                    gst::ResourceError::OpenReadWrite
                }
            });
        }
        if cause.is::<tokio::time::error::Elapsed>() {
            return Some(gst::ResourceError::OpenReadWrite);
        }
    }
    None
}

/// Post an ERROR message on `$element` for the failure `$err`. The code
/// comes from [`resource_error`], or is `$fallback` when nvnmosd was not
/// involved. The remaining arguments format the debug string.
///
/// A macro rather than a function so that the message records the caller's
/// file, function, and line.
macro_rules! post_state_change_error {
    ($element:expr, $err:expr, $fallback:expr, $($debug:tt)*) => {{
        let debug = format!($($debug)*);
        match $crate::session::error::resource_error($err) {
            Some(code) => $element.post_error_message(gst::error_msg!(code, ["{debug}"])),
            None => $element.post_error_message(gst::error_msg!($fallback, ["{debug}"])),
        }
    }};
}
pub(crate) use post_state_change_error;

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(status: tonic::Status) -> anyhow::Error {
        anyhow::Error::new(DaemonError::from(status)).context("AddChannelMapping")
    }

    #[test]
    fn rejected_request_is_settings() {
        assert_eq!(
            resource_error(&rpc(tonic::Status::invalid_argument("bad id"))),
            Some(gst::ResourceError::Settings)
        );
        assert_eq!(
            resource_error(&rpc(tonic::Status::already_exists("taken"))),
            Some(gst::ResourceError::Settings)
        );
        assert_eq!(
            resource_error(&anyhow::Error::new(DaemonError::UnsupportedScheme(
                "tcp".into()
            ))),
            Some(gst::ResourceError::Settings)
        );
    }

    #[test]
    fn unavailable_or_exhausted_daemon_is_busy() {
        assert_eq!(
            resource_error(&rpc(tonic::Status::unavailable("node being created"))),
            Some(gst::ResourceError::Busy)
        );
        assert_eq!(
            resource_error(&rpc(tonic::Status::resource_exhausted("no free port"))),
            Some(gst::ResourceError::Busy)
        );
    }

    #[test]
    fn other_rpc_failure_is_open_read_write() {
        assert_eq!(
            resource_error(&rpc(tonic::Status::internal("boom"))),
            Some(gst::ResourceError::OpenReadWrite)
        );
    }

    #[test]
    fn timeout_or_session_state_failure_is_open_read_write() {
        let elapsed = crate::runtime::SHARED_RUNTIME
            .block_on(async {
                tokio::time::timeout(std::time::Duration::ZERO, std::future::pending::<()>()).await
            })
            .unwrap_err();
        assert_eq!(
            resource_error(&anyhow::Error::new(elapsed).context("OpenSession timed out")),
            Some(gst::ResourceError::OpenReadWrite)
        );
        for e in [DaemonError::AlreadyAdded, DaemonError::NoResource] {
            assert_eq!(
                resource_error(&anyhow::Error::new(e)),
                Some(gst::ResourceError::OpenReadWrite)
            );
        }
    }

    #[test]
    fn failure_without_nvnmosd_keeps_caller_code() {
        let err = anyhow::anyhow!("no pads").context("fixation");
        assert_eq!(resource_error(&err), None);
    }
}
