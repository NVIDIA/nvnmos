// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Session types shared across transport families (MXL, RTP/UDP).

/// Whether the snapshot came from `nmossink` or `nmossrc`. Surfaces in
/// error/log messages so validation failures point the user at the
/// right property name, and selects which gRPC AddSender/AddReceiver
/// call the session opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Side {
    Sender,
    Receiver,
}

impl Side {
    /// Property name the user sets to supply the NMOS name for this
    /// side of element — `sender-name` on `nmossink`, `receiver-name`
    /// on `nmossrc`. Used in validation error messages.
    pub(crate) fn name_property(self) -> &'static str {
        match self {
            Self::Sender => "sender-name",
            Self::Receiver => "receiver-name",
        }
    }

    /// Decode the proto-level `Side` enum value carried on
    /// `ActivationEvent.side`. Returns `None` for `SIDE_UNSPECIFIED`
    /// or any value not in the proto enum — the daemon never sends
    /// those, so the activation handler treats them as a bug and
    /// acks failure.
    pub(crate) fn try_from_proto(value: i32) -> Option<Self> {
        match nvnmos_rpc::v1::Side::try_from(value).ok()? {
            nvnmos_rpc::v1::Side::Sender => Some(Self::Sender),
            nvnmos_rpc::v1::Side::Receiver => Some(Self::Receiver),
            nvnmos_rpc::v1::Side::Unspecified => None,
        }
    }
}
