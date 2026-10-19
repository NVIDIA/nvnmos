// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The GStreamer error code an element posts when a state change fails.

use gstreamer as gst;

use crate::daemon::DaemonError;
use crate::domain::DomainError;

/// A GStreamer error code from one of the domains these elements post.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorCode {
    Resource(gst::ResourceError),
    Core(gst::CoreError),
}

impl From<gst::ResourceError> for ErrorCode {
    fn from(code: gst::ResourceError) -> Self {
        Self::Resource(code)
    }
}

impl From<gst::CoreError> for ErrorCode {
    fn from(code: gst::CoreError) -> Self {
        Self::Core(code)
    }
}

/// An error tagged with the code to post for it. It displays as the error
/// it wraps and has the same sources, so tagging adds nothing to the debug
/// string.
#[derive(Debug)]
struct CodedError {
    code: ErrorCode,
    error: anyhow::Error,
}

impl std::fmt::Display for CodedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for CodedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.chain().nth(1)
    }
}

/// Tag the error of a `Result` with the code to post for it.
pub(crate) trait WithCode<T> {
    fn with_code(self, code: impl Into<ErrorCode>) -> anyhow::Result<T>;
}

impl<T, E: Into<anyhow::Error>> WithCode<T> for Result<T, E> {
    fn with_code(self, code: impl Into<ErrorCode>) -> anyhow::Result<T> {
        self.map_err(|e| {
            anyhow::Error::new(CodedError {
                code: code.into(),
                error: e.into(),
            })
        })
    }
}

/// The code for `err`: the outermost code tagged on its chain, or derived
/// from the outermost nvnmosd or MXL Domain failure, or `CoreError::Failed`
/// when the chain has none of these.
pub(crate) fn error_code(err: &anyhow::Error) -> ErrorCode {
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<CodedError>() {
            return e.code;
        }
        if let Some(e) = cause.downcast_ref::<DaemonError>() {
            return ErrorCode::Resource(match e {
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
        if let Some(e) = cause.downcast_ref::<DomainError>() {
            return ErrorCode::Resource(match e {
                DomainError::Read { .. } => gst::ResourceError::OpenRead,
                _ => gst::ResourceError::Settings,
            });
        }
        if cause.is::<tokio::time::error::Elapsed>() {
            return ErrorCode::Resource(gst::ResourceError::OpenReadWrite);
        }
    }
    ErrorCode::Core(gst::CoreError::Failed)
}

/// Post an ERROR message on `$element` for the failure `$err`, with the
/// code from [`error_code`]. The remaining arguments format the debug
/// string.
///
/// A macro rather than a function so that the message records the caller's
/// file, function, and line.
macro_rules! post_state_change_error {
    ($element:expr, $err:expr, $($debug:tt)*) => {{
        let debug = format!($($debug)*);
        match $crate::session::error::error_code($err) {
            $crate::session::error::ErrorCode::Resource(code) => {
                $element.post_error_message(gst::error_msg!(code, ["{debug}"]))
            }
            $crate::session::error::ErrorCode::Core(code) => {
                $element.post_error_message(gst::error_msg!(code, ["{debug}"]))
            }
        }
    }};
}
pub(crate) use post_state_change_error;

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;

    fn rpc(status: tonic::Status) -> anyhow::Error {
        anyhow::Error::new(DaemonError::from(status)).context("AddChannelMapping")
    }

    fn settings() -> ErrorCode {
        ErrorCode::Resource(gst::ResourceError::Settings)
    }

    #[test]
    fn rejected_request_is_settings() {
        assert_eq!(
            error_code(&rpc(tonic::Status::invalid_argument("bad id"))),
            settings()
        );
        assert_eq!(
            error_code(&rpc(tonic::Status::already_exists("taken"))),
            settings()
        );
        assert_eq!(
            error_code(&anyhow::Error::new(DaemonError::UnsupportedScheme(
                "tcp".into()
            ))),
            settings()
        );
    }

    #[test]
    fn unavailable_or_exhausted_daemon_is_busy() {
        assert_eq!(
            error_code(&rpc(tonic::Status::unavailable("node being created"))),
            ErrorCode::Resource(gst::ResourceError::Busy)
        );
        assert_eq!(
            error_code(&rpc(tonic::Status::resource_exhausted("no free port"))),
            ErrorCode::Resource(gst::ResourceError::Busy)
        );
    }

    #[test]
    fn other_rpc_failure_is_open_read_write() {
        assert_eq!(
            error_code(&rpc(tonic::Status::internal("boom"))),
            ErrorCode::Resource(gst::ResourceError::OpenReadWrite)
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
            error_code(&anyhow::Error::new(elapsed).context("OpenSession timed out")),
            ErrorCode::Resource(gst::ResourceError::OpenReadWrite)
        );
        for e in [DaemonError::AlreadyAdded, DaemonError::NoResource] {
            assert_eq!(
                error_code(&anyhow::Error::new(e)),
                ErrorCode::Resource(gst::ResourceError::OpenReadWrite)
            );
        }
    }

    #[test]
    fn unreadable_domain_is_open_read_and_other_domain_failures_are_settings() {
        let read = DomainError::Read {
            path: "/nonexistent/domain_def.json".into(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        assert_eq!(
            error_code(&anyhow::Error::new(read).context("resolving MXL Domain identity")),
            ErrorCode::Resource(gst::ResourceError::OpenRead)
        );
        assert_eq!(
            error_code(&anyhow::Error::new(DomainError::Unconfigured)),
            settings()
        );
    }

    #[test]
    fn untagged_failure_is_core_failed() {
        let err = anyhow::anyhow!("no pads").context("fixation");
        assert_eq!(error_code(&err), ErrorCode::Core(gst::CoreError::Failed));
    }

    #[test]
    fn outermost_tag_wins_and_adds_nothing_to_the_text() {
        let err = Err::<(), _>(anyhow::anyhow!("no such file"))
            .context("reading `transport-file-path`")
            .with_code(gst::ResourceError::OpenRead)
            .context("NULL\u{2192}READY")
            .with_code(gst::CoreError::StateChange)
            .unwrap_err();
        assert_eq!(
            error_code(&err),
            ErrorCode::Core(gst::CoreError::StateChange)
        );
        assert_eq!(
            format!("{err:#}"),
            "NULL\u{2192}READY: reading `transport-file-path`: no such file"
        );
    }

    #[test]
    fn tag_outside_rpc_failure_wins() {
        let err = Err::<(), _>(rpc(tonic::Status::internal("boom")))
            .with_code(gst::ResourceError::Settings)
            .unwrap_err();
        assert_eq!(error_code(&err), settings());
    }
}
