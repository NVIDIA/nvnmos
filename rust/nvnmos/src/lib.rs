// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Safe Rust bindings to the C `libnvnmos` API.
//!
//! This crate wraps the raw FFI in [`nvnmos_sys`] with a small, RAII-friendly
//! surface:
//!
//! * [`NodeServer`] — owning handle for a running NMOS Node server. Built from
//!   a [`NodeConfig`]; tears down on drop.
//! * [`NodeServer::add_sender`] / [`add_receiver`](NodeServer::add_receiver) /
//!   [`remove_sender`](NodeServer::remove_sender) /
//!   [`remove_receiver`](NodeServer::remove_receiver) — manage the IS-04/IS-05
//!   model after start-up.
//! * [`NodeServer::activate_connection`] — surface an out-of-band activation
//!   (mirrors the `nmos_connection_activate` C function).
//! * [`NodeServer::node_id`] / [`device_id`](NodeServer::device_id) /
//!   [`sender_id`](NodeServer::sender_id) /
//!   [`receiver_id`](NodeServer::receiver_id) /
//!   [`source_id`](NodeServer::source_id) /
//!   [`flow_id`](NodeServer::flow_id) — look up NMOS resource UUIDs on a
//!   running server.
//! * [`make_node_id`] / [`make_device_id`] / [`make_sender_id`] /
//!   [`make_receiver_id`] / [`make_source_id`] / [`make_flow_id`] — pure
//!   functions that compute the same UUIDs deterministically from a seed,
//!   without needing a running server.
//! * [`NodeServer::builder`] — opt into IS-05 [`Activation`] handling and / or
//!   forward libnvnmos's slog output via [`LogMessage`]. Both knobs are
//!   chainable on the returned [`NodeServerBuilder`]; the bare
//!   [`NodeServer::new`] is shorthand for "no callbacks".
//!
//! ## Building
//!
//! See the workspace `README.md` for `NVNMOS_LIB_DIR` / `NVNMOS_INCLUDE_DIR`
//! conventions inherited from `nvnmos-sys`.

#![warn(missing_docs)]

use std::ffi::{CStr, CString, NulError};
use std::os::raw::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::str::Utf8Error;

use nvnmos_sys as sys;

// ============================================================================
// Errors
// ============================================================================

/// Errors returned by this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A caller-supplied string contained an interior NUL byte and could not
    /// cross the C boundary.
    #[error("string contains interior NUL byte")]
    InteriorNul(#[from] NulError),

    /// A caller-supplied [`NodeConfig`] (or one of its nested submessages,
    /// e.g. [`AssetConfig`]) violated a library-side precondition that
    /// the wrapper enforces before crossing the FFI boundary.
    #[error("invalid NodeConfig: {0}")]
    InvalidConfig(&'static str),

    /// The C library reported a failure for the named operation.
    ///
    /// The wrapped string is the name of the underlying C function that
    /// returned `false`.
    #[error("libnvnmos: {0} returned false")]
    Failed(&'static str),

    /// An id returned by the C library was not valid UTF-8.
    ///
    /// `libnvnmos` produces canonical UUID strings (ASCII), so this should not
    /// happen in practice — surfacing it explicitly keeps the boundary honest.
    #[error("libnvnmos returned an id that is not valid UTF-8")]
    InvalidUtf8(#[from] Utf8Error),
}

/// Convenience [`Result`] type for this crate.
pub type Result<T> = std::result::Result<T, Error>;

// ============================================================================
// Transport
// ============================================================================

/// Transport used by an NMOS sender or receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Transport {
    /// RTP, as used by SMPTE ST 2110 (the `urn:x-nmos:transport:rtp.*` family).
    Rtp,
    /// The Media eXchange Layer (the `urn:x-nmos:transport:mxl` family).
    Mxl,
}

impl Transport {
    fn to_ffi(self) -> sys::NvNmosTransport {
        match self {
            Self::Rtp => sys::_NvNmosTransport_NVNMOS_TRANSPORT_RTP,
            Self::Mxl => sys::_NvNmosTransport_NVNMOS_TRANSPORT_MXL,
        }
    }
}

// ============================================================================
// Side
// ============================================================================

/// Whether a resource is a Sender or a Receiver. Carried alongside `name`
/// wherever the role is not otherwise pinned (e.g. activation callbacks
/// and the out-of-band [`NodeServer::activate_connection`] path), so a
/// Sender and a Receiver are permitted to share a `name` on the same Node.
///
/// Mirrors the C [`sys::NvNmosSide`] enum one-for-one; the NMOS
/// connection model defines exactly these two activation targets, so we
/// don't mark this `#[non_exhaustive]` (a third variant would be a C ABI
/// break upstream anyway).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// An NMOS Sender (`/senders/<id>`).
    Sender,
    /// An NMOS Receiver (`/receivers/<id>`).
    Receiver,
}

impl Side {
    fn to_ffi(self) -> sys::NvNmosSide {
        match self {
            Self::Sender => sys::_NvNmosSide_NVNMOS_SIDE_SENDER,
            Self::Receiver => sys::_NvNmosSide_NVNMOS_SIDE_RECEIVER,
        }
    }

    fn from_ffi(side: sys::NvNmosSide) -> Option<Self> {
        match side {
            sys::_NvNmosSide_NVNMOS_SIDE_SENDER => Some(Self::Sender),
            sys::_NvNmosSide_NVNMOS_SIDE_RECEIVER => Some(Self::Receiver),
            _ => None,
        }
    }
}

// ============================================================================
// Activation callback
// ============================================================================

/// An IS-05 Connection API activation reported by the C library through the
/// callback installed via [`NodeServerBuilder::on_activation`].
///
/// `transport_file` is `Some` when the resource is being activated (with the
/// updated SDP or MXL flow definition that should now apply) and `None` when
/// it is being deactivated.
#[derive(Debug)]
pub struct Activation<'a> {
    /// Whether the affected resource is a Sender or a Receiver.
    pub side: Side,
    /// Name of the affected sender or receiver — the same name supplied
    /// when the resource was added (via the `x-nvnmos-name` SDP attribute
    /// or the `urn:x-nvnmos:tag:name` flow-def tag). Unique only for the
    /// given [`Self::side`] on the Node.
    pub name: &'a str,
    /// Updated transport file, or `None` for a deactivation.
    pub transport_file: Option<&'a str>,
}

type ActivationCallback =
    Box<dyn Fn(&Activation<'_>) -> std::result::Result<(), String> + Send + Sync + 'static>;

/// An IS-08 Channel Mapping API activation for one Output.
#[derive(Debug)]
pub struct ChannelMappingActivation<'a> {
    /// Caller-chosen name of the channel mapping; not IS-08 `/properties` name.
    pub name: &'a str,
    /// IS-08 output id just activated.
    pub output_id: &'a str,
    /// Active map for this output only.
    pub active_map: &'a [ChannelMappingActiveMapEntry],
}

/// One output channel entry in an IS-08 active map.
///
/// Dense array: index `i` is output channel `i`.
#[derive(Debug, Clone)]
pub struct ChannelMappingActiveMapEntry {
    /// IS-08 input id, or `None` when unrouted.
    pub input_id: Option<String>,
    /// Input channel index when routed; `None` when unrouted (ignored at the C API).
    pub input_channel: Option<u32>,
}

/// Parent type for an IS-08 Input `/parent` endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMappingParentType {
    /// IS-04 Receiver parent.
    Receiver,
    /// IS-04 Source parent.
    Source,
}

impl ChannelMappingParentType {
    fn to_ffi(self) -> sys::NvNmosChannelMappingParentType {
        match self {
            Self::Receiver => {
                sys::_NvNmosChannelMappingParentType_NVNMOS_CHANNELMAPPING_PARENT_TYPE_RECEIVER
            }
            Self::Source => {
                sys::_NvNmosChannelMappingParentType_NVNMOS_CHANNELMAPPING_PARENT_TYPE_SOURCE
            }
        }
    }
}

/// IS-08 Input geometry for [`NodeServer::add_channelmapping`].
#[derive(Debug, Clone)]
pub struct ChannelMappingInput {
    /// IS-08 input id (non-empty).
    pub id: String,
    /// IS-08 `/properties` name (not the caller-chosen channel mapping name).
    pub name: String,
    /// IS-08 `/properties` description.
    pub description: String,
    /// IS-08 channel labels; must be non-empty.
    pub channel_labels: Vec<String>,
    /// IS-04 parent resource name; empty → null `/parent`.
    pub parent_name: String,
    /// Receiver vs source when `parent_name` is set.
    pub parent_type: ChannelMappingParentType,
    /// Used when `block_size != 0`; ignored when `block_size` is 0.
    pub reordering: bool,
    /// 0 → default 1 with reordering true.
    pub block_size: u32,
}

/// IS-08 Output geometry for [`NodeServer::add_channelmapping`].
#[derive(Debug, Clone)]
pub struct ChannelMappingOutput {
    /// IS-08 output id (non-empty).
    pub id: String,
    /// IS-08 `/properties` name.
    pub name: String,
    /// IS-08 `/properties` description.
    pub description: String,
    /// IS-08 channel labels; must be non-empty.
    pub channel_labels: Vec<String>,
    /// Caller-chosen sender name for `/sourceid`; empty → null.
    pub sender_name: String,
    /// IS-08 output /caps routable_inputs; `None` → unrestricted (`null`).
    /// Empty string entries → `null` in the IS-08 array (unrouted permitted).
    pub routable_inputs: Option<Vec<String>>,
}

/// Input/output bundle for [`NodeServer::add_channelmapping`].
#[derive(Debug, Clone)]
pub struct ChannelMappingConfig {
    /// IS-08 inputs to add.
    pub inputs: Vec<ChannelMappingInput>,
    /// IS-08 outputs to add.
    pub outputs: Vec<ChannelMappingOutput>,
}
type ChannelMappingActivationCallback = Box<
    dyn Fn(&ChannelMappingActivation<'_>) -> std::result::Result<(), String>
        + Send
        + Sync
        + 'static,
>;

/// A single log message from libnvnmos, passed to the callback installed via
/// [`NodeServerBuilder::on_log`].
///
/// Per libnvnmos's contract, log messages may be emitted from any worker
/// thread; the callback must therefore be `Send + Sync`. The message body
/// already carries source location (filename, line, function) via the
/// `SLOG_FLF` macro on the C side, so the callback typically just forwards
/// `categories` + `message` to its preferred logging sink.
#[derive(Debug)]
pub struct LogMessage<'a> {
    /// Severity level (cf. `LOG_LEVEL_*` constants). The C library has
    /// already filtered messages below [`NodeConfig::log_level`] before
    /// invoking the callback.
    pub level: i32,
    /// Comma-separated list of category tags, e.g. `nmos,Node`.
    pub categories: &'a str,
    /// The message text (typically pre-formatted with `SLOG_FLF` source
    /// location).
    pub message: &'a str,
}

type LogCallback = Box<dyn Fn(&LogMessage<'_>) + Send + Sync + 'static>;

struct CallbackState {
    activation: Option<ActivationCallback>,
    channelmapping_activation: Option<ChannelMappingActivationCallback>,
    log: Option<LogCallback>,
}

/// Trampoline invoked by libnvnmos for IS-05 activations. `server.user_data`
/// carries a stable pointer to the [`CallbackState`] owned by the
/// [`NodeServer`] whose lifetime brackets every callback (see the `Drop`
/// impl).
unsafe extern "C" fn activation_trampoline(
    server: *mut sys::NvNmosNodeServer,
    side: sys::NvNmosSide,
    name: *const c_char,
    transport_file: *const c_char,
) -> bool {
    if server.is_null() || name.is_null() {
        return false;
    }
    let Some(side) = Side::from_ffi(side) else {
        return false;
    };
    // SAFETY: `server` is owned by Rust; `user_data` is set exactly once in
    // `NodeServer::create` and cleared only after `destroy_nmos_node_server`
    // has joined all callback threads.
    let user_data = unsafe { (*server).user_data };
    if user_data.is_null() {
        return false;
    }
    let state = unsafe { &*(user_data as *const CallbackState) };
    let Some(callback) = state.activation.as_ref() else {
        return false;
    };

    // SAFETY: libnvnmos passes valid NUL-terminated UTF-8, or NULL for
    // `transport_file` on deactivation.
    let name = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };
    let transport_file = if transport_file.is_null() {
        None
    } else {
        match unsafe { CStr::from_ptr(transport_file) }.to_str() {
            Ok(s) => Some(s),
            Err(_) => return false,
        }
    };
    let activation = Activation {
        side,
        name,
        transport_file,
    };

    // Catch panics so they don't unwind across the FFI boundary. Map any
    // panic — and any returned `Err(reason)` — to `false`, which libnvnmos
    // surfaces to IS-05 as activation failure.
    match catch_unwind(AssertUnwindSafe(|| callback(&activation))) {
        Ok(Ok(())) => true,
        Ok(Err(_reason)) => false,
        Err(_panic) => false,
    }
}

unsafe extern "C" fn channelmapping_activation_trampoline(
    server: *mut sys::NvNmosNodeServer,
    name: *const c_char,
    output_id: *const c_char,
    active_map: *const sys::NvNmosChannelMappingActiveMapEntry,
    num_active_map: usize,
) -> bool {
    if server.is_null() || name.is_null() || output_id.is_null() {
        return false;
    }
    // SAFETY: `server` is owned by Rust; `user_data` is set exactly once in
    // `NodeServer::create` and cleared only after `destroy_nmos_node_server`
    // has joined all callback threads.
    let user_data = unsafe { (*server).user_data };
    if user_data.is_null() {
        return false;
    }
    let state = unsafe { &*(user_data as *const CallbackState) };
    let Some(callback) = state.channelmapping_activation.as_ref() else {
        return false;
    };

    // SAFETY: libnvnmos passes valid NUL-terminated UTF-8.
    let name = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };
    let output_id = match unsafe { CStr::from_ptr(output_id) }.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };

    let mut channels = Vec::with_capacity(num_active_map);
    if !active_map.is_null() {
        for index in 0..num_active_map {
            let ch = unsafe { &*active_map.add(index) };
            let input_id = if ch.input_id.is_null() {
                None
            } else {
                match unsafe { CStr::from_ptr(ch.input_id) }.to_str() {
                    Ok(s) => Some(s.to_string()),
                    Err(_) => return false,
                }
            };
            let input_channel = input_id.as_ref().map(|_| ch.input_channel);
            channels.push(ChannelMappingActiveMapEntry {
                input_id,
                input_channel,
            });
        }
    }

    let activation = ChannelMappingActivation {
        name,
        output_id,
        active_map: &channels,
    };

    // Catch panics so they don't unwind across the FFI boundary.
    match catch_unwind(AssertUnwindSafe(|| callback(&activation))) {
        Ok(Ok(())) => true,
        Ok(Err(_reason)) => false,
        Err(_panic) => false,
    }
}

/// Trampoline invoked by libnvnmos for log messages. Same `user_data`
/// indirection as [`activation_trampoline`].
unsafe extern "C" fn log_trampoline(
    server: *mut sys::NvNmosNodeServer,
    categories: *const c_char,
    level: std::os::raw::c_int,
    message: *const c_char,
) {
    if server.is_null() {
        return;
    }
    let user_data = unsafe { (*server).user_data };
    if user_data.is_null() {
        return;
    }
    let state = unsafe { &*(user_data as *const CallbackState) };
    let Some(callback) = state.log.as_ref() else {
        return;
    };

    let categories = if categories.is_null() {
        ""
    } else {
        unsafe { CStr::from_ptr(categories) }.to_str().unwrap_or("")
    };
    let message = if message.is_null() {
        ""
    } else {
        unsafe { CStr::from_ptr(message) }.to_str().unwrap_or("")
    };
    let log = LogMessage {
        level,
        categories,
        message,
    };
    // Swallow panics — losing log output is worse-case acceptable, unwinding
    // across the FFI boundary is not.
    let _ = catch_unwind(AssertUnwindSafe(|| callback(&log)));
}

// ============================================================================
// ID accessors (pure)
// ============================================================================

const ID_BUF_LEN: usize = sys::NVNMOS_ID_LEN as usize;

fn capture_id(buf: &[u8; ID_BUF_LEN]) -> Result<String> {
    let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(std::str::from_utf8(&buf[..nul])?.to_owned())
}

/// Compute the NMOS Node resource id (the `/self` UUID) that a [`NodeServer`]
/// with the given `seed` will use.
///
/// Pure function of `seed`: calling this before [`NodeServer::new`] yields the
/// same value as [`NodeServer::node_id`] on the resulting server.
pub fn make_node_id(seed: &str) -> Result<String> {
    let cseed = CString::new(seed)?;
    let mut buf = [0u8; ID_BUF_LEN];
    let ok = unsafe {
        sys::nmos_make_node_id(cseed.as_ptr(), buf.as_mut_ptr() as *mut c_char, buf.len())
    };
    if !ok {
        return Err(Error::Failed("nmos_make_node_id"));
    }
    capture_id(&buf)
}

/// Compute the NMOS Device resource id that a [`NodeServer`] with the given
/// `seed` will use.
///
/// Pure function of `seed`. NvNmos creates one Device per Node.
pub fn make_device_id(seed: &str) -> Result<String> {
    let cseed = CString::new(seed)?;
    let mut buf = [0u8; ID_BUF_LEN];
    let ok = unsafe {
        sys::nmos_make_device_id(cseed.as_ptr(), buf.as_mut_ptr() as *mut c_char, buf.len())
    };
    if !ok {
        return Err(Error::Failed("nmos_make_device_id"));
    }
    capture_id(&buf)
}

/// Compute the NMOS Sender resource id that a [`NodeServer`] with the given
/// `seed` will use for the sender with the given `sender_name` (the
/// `x-nvnmos-name` SDP attribute or `urn:x-nvnmos:tag:name` flow-def tag).
pub fn make_sender_id(seed: &str, sender_name: &str) -> Result<String> {
    let cseed = CString::new(seed)?;
    let cname = CString::new(sender_name)?;
    let mut buf = [0u8; ID_BUF_LEN];
    let ok = unsafe {
        sys::nmos_make_sender_id(
            cseed.as_ptr(),
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
        )
    };
    if !ok {
        return Err(Error::Failed("nmos_make_sender_id"));
    }
    capture_id(&buf)
}

/// Compute the NMOS Receiver resource id that a [`NodeServer`] with the given
/// `seed` will use for the receiver with the given `receiver_name` (the
/// `x-nvnmos-name` SDP attribute or `urn:x-nvnmos:tag:name` flow-def tag).
pub fn make_receiver_id(seed: &str, receiver_name: &str) -> Result<String> {
    let cseed = CString::new(seed)?;
    let cname = CString::new(receiver_name)?;
    let mut buf = [0u8; ID_BUF_LEN];
    let ok = unsafe {
        sys::nmos_make_receiver_id(
            cseed.as_ptr(),
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
        )
    };
    if !ok {
        return Err(Error::Failed("nmos_make_receiver_id"));
    }
    capture_id(&buf)
}

/// Compute the IS-04 Source resource id for a seed and sender name.
pub fn make_source_id(seed: &str, sender_name: &str) -> Result<String> {
    let cseed = CString::new(seed)?;
    let cname = CString::new(sender_name)?;
    let mut buf = [0u8; ID_BUF_LEN];
    let ok = unsafe {
        sys::nmos_make_source_id(
            cseed.as_ptr(),
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
        )
    };
    if !ok {
        return Err(Error::Failed("nmos_make_source_id"));
    }
    capture_id(&buf)
}

/// Compute the IS-04 Flow resource id for a seed and sender name.
///
/// This is the Flow id (the Sender's `flow_id` property), not the MXL
/// `mxl_flow_id` IS-05 transport parameter.
pub fn make_flow_id(seed: &str, sender_name: &str) -> Result<String> {
    let cseed = CString::new(seed)?;
    let cname = CString::new(sender_name)?;
    let mut buf = [0u8; ID_BUF_LEN];
    let ok = unsafe {
        sys::nmos_make_flow_id(
            cseed.as_ptr(),
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
        )
    };
    if !ok {
        return Err(Error::Failed("nmos_make_flow_id"));
    }
    capture_id(&buf)
}

struct MarshalledChannelMapping {
    #[allow(dead_code)] // pins FFI arrays referenced by `mapping`
    inputs: Vec<sys::NvNmosChannelMappingInput>,
    #[allow(dead_code)]
    outputs: Vec<sys::NvNmosChannelMappingOutput>,
    _input_label_ptrs: Vec<Vec<*const c_char>>,
    _output_label_ptrs: Vec<Vec<*const c_char>>,
    _output_routable_ptrs: Vec<Vec<*const c_char>>,
    _strings: Vec<CString>,
    mapping: sys::NvNmosChannelMappingConfig,
}

impl MarshalledChannelMapping {
    fn new(mapping: &ChannelMappingConfig) -> Result<Self> {
        let mut strings = Vec::new();
        let mut input_label_ptrs = Vec::new();
        let mut inputs = Vec::with_capacity(mapping.inputs.len());
        for input in &mapping.inputs {
            let id = store_string(&mut strings, &input.id);
            let name = store_optional_string(&mut strings, &input.name);
            let description = store_optional_string(&mut strings, &input.description);
            let parent_name = store_optional_string(&mut strings, &input.parent_name);
            let mut label_ptrs = Vec::with_capacity(input.channel_labels.len());
            for label in &input.channel_labels {
                label_ptrs.push(store_string(&mut strings, label));
            }
            input_label_ptrs.push(label_ptrs);
            let channel_labels = input_label_ptrs.last().unwrap().as_ptr() as *mut _;
            inputs.push(sys::NvNmosChannelMappingInput {
                id,
                name,
                description,
                channel_labels,
                num_channel_labels: input.channel_labels.len(),
                parent_name,
                parent_type: input.parent_type.to_ffi(),
                reordering: input.reordering,
                block_size: input.block_size,
            });
        }

        let mut output_label_ptrs = Vec::new();
        let mut output_routable_ptrs = Vec::new();
        let mut outputs = Vec::with_capacity(mapping.outputs.len());
        for output in &mapping.outputs {
            let id = store_string(&mut strings, &output.id);
            let name = store_optional_string(&mut strings, &output.name);
            let description = store_optional_string(&mut strings, &output.description);
            let sender_name = store_optional_string(&mut strings, &output.sender_name);
            let mut label_ptrs = Vec::with_capacity(output.channel_labels.len());
            for label in &output.channel_labels {
                label_ptrs.push(store_string(&mut strings, label));
            }
            output_label_ptrs.push(label_ptrs);
            let channel_labels = output_label_ptrs.last().unwrap().as_ptr() as *mut _;
            let (routable_inputs, num_routable_inputs) = match output.routable_inputs.as_ref() {
                None => (ptr::null_mut(), 0),
                Some(ids) => {
                    let mut routable_ptrs = Vec::with_capacity(ids.len());
                    for id in ids {
                        routable_ptrs.push(store_string(&mut strings, id));
                    }
                    output_routable_ptrs.push(routable_ptrs);
                    let routable = output_routable_ptrs.last().unwrap();
                    (routable.as_ptr() as *mut _, routable.len())
                }
            };
            outputs.push(sys::NvNmosChannelMappingOutput {
                id,
                name,
                description,
                channel_labels,
                num_channel_labels: output.channel_labels.len(),
                sender_name,
                routable_inputs,
                num_routable_inputs,
            });
        }

        let mapping = sys::NvNmosChannelMappingConfig {
            inputs: inputs.as_ptr(),
            num_inputs: inputs.len(),
            outputs: outputs.as_ptr(),
            num_outputs: outputs.len(),
        };
        Ok(Self {
            inputs,
            outputs,
            _input_label_ptrs: input_label_ptrs,
            _output_label_ptrs: output_label_ptrs,
            _output_routable_ptrs: output_routable_ptrs,
            _strings: strings,
            mapping,
        })
    }

    fn as_ptr(&self) -> *const sys::NvNmosChannelMappingConfig {
        &self.mapping
    }
}

struct MarshalledActiveMap {
    entries: Vec<sys::NvNmosChannelMappingActiveMapEntry>,
    _strings: Vec<CString>,
}

impl MarshalledActiveMap {
    fn new(active_map: &[ChannelMappingActiveMapEntry]) -> Result<Self> {
        let mut strings = Vec::new();
        let mut entries = Vec::with_capacity(active_map.len());
        for entry in active_map {
            let input_id = entry
                .input_id
                .as_ref()
                .map(|id| store_string(&mut strings, id))
                .unwrap_or(ptr::null());
            entries.push(sys::NvNmosChannelMappingActiveMapEntry {
                input_id,
                input_channel: entry.input_channel.unwrap_or(0),
            });
        }
        Ok(Self {
            entries,
            _strings: strings,
        })
    }

    fn as_ptr(&self) -> *const sys::NvNmosChannelMappingActiveMapEntry {
        self.entries.as_ptr()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

fn store_string(strings: &mut Vec<CString>, value: &str) -> *const c_char {
    strings.push(CString::new(value).expect("interior NUL"));
    strings.last().unwrap().as_ptr()
}

fn store_optional_string(strings: &mut Vec<CString>, value: &str) -> *const c_char {
    if value.is_empty() {
        ptr::null()
    } else {
        store_string(strings, value)
    }
}

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for creating a [`NodeServer`].
///
/// Empty strings on `Option`-like fields (`host_name`, `label`, `description`)
/// mean "let the library use its default" — they map to `NULL` at the FFI
/// boundary, not to an empty C string. Sender / receiver lists in the
/// initial config are not exposed; add them after construction via
/// [`NodeServer::add_sender`] and [`NodeServer::add_receiver`]. The IS-05
/// activation and log callbacks are installed via
/// [`NodeServer::builder`].
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Seed string used to derive all NMOS resource ids deterministically.
    /// Empty means "use a random seed", which is **not** recommended.
    pub seed: String,
    /// Fully-qualified host name. Empty means "use the system hostname".
    pub host_name: String,
    /// Host IP addresses to advertise. Empty means "use the system addresses".
    pub host_addresses: Vec<String>,
    /// HTTP port for the IS-04 / IS-05 APIs. Zero selects the per-API
    /// defaults.
    pub http_port: u16,
    /// Label of the node and device. Empty means "synthesise from asset tags
    /// (or none)".
    pub label: String,
    /// Description of the node and device. Empty means "synthesise from asset
    /// tags (or none)".
    pub description: String,
    /// BCP-002-02 Asset Distinguishing Information. `None` means no
    /// asset-distinguishing tags are emitted into the IS-04 `/self`
    /// resource. Independently, when [`Self::label`] or
    /// [`Self::description`] is empty, libnvnmos fills it in from
    /// these tags if present, otherwise leaves it blank.
    pub asset_tags: Option<AssetConfig>,
    /// Network-services configuration (DNS-SD domain, fixed IS-04
    /// Registration API, optional IS-09 System API). `None` means
    /// libnvnmos falls back to DNS-SD discovery based on `host_name`.
    pub network_services: Option<NetworkServicesConfig>,
    /// Minimum severity at which the C library would emit log callbacks.
    /// Defaults to [`LOG_LEVEL_INFO`]. Messages below this severity are
    /// dropped by libnvnmos before the callback installed via
    /// [`NodeServerBuilder::on_log`] is invoked.
    pub log_level: i32,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            seed: String::new(),
            host_name: String::new(),
            host_addresses: Vec::new(),
            http_port: 0,
            label: String::new(),
            description: String::new(),
            asset_tags: None,
            network_services: None,
            log_level: LOG_LEVEL_INFO,
        }
    }
}

/// BCP-002-02 Asset Distinguishing Information. All fields are
/// required by libnvnmos when this struct is supplied; set
/// [`NodeConfig::asset_tags`] to `None` to opt out entirely.
#[derive(Debug, Clone)]
pub struct AssetConfig {
    /// Manufacturer, e.g. `"Acme"`.
    pub manufacturer: String,
    /// Product name, e.g. `"Widget Pro"`.
    pub product: String,
    /// Instance identifier, e.g. `"XYZ123-456789"`.
    pub instance_id: String,
    /// Function(s) the Node performs, e.g. `["Decoder"]` or `["Encoder",
    /// "Analyzer"]`. Must contain at least one entry.
    pub functions: Vec<String>,
}

/// Owns the C strings backing a [`sys::NvNmosAssetConfig`].
///
/// The C library copies what it needs out of the config struct during
/// `create_nmos_node_server`, so the marshalled holder only needs to
/// outlive that single call. [`Self::sys`] returns a value-typed sys
/// struct whose pointers borrow from `self`; the caller is responsible
/// for keeping `self` alive until `create_nmos_node_server` returns.
struct MarshalledAssetConfig {
    manufacturer: CString,
    product: CString,
    instance_id: CString,
    /// Owns the C strings whose pointers `function_ptrs` borrows. Held
    /// purely for that borrow; never read directly after construction
    /// (the C side reads through the pointer array).
    #[allow(dead_code)]
    functions: Vec<CString>,
    function_ptrs: Vec<*const c_char>,
}

impl MarshalledAssetConfig {
    fn new(asset: &AssetConfig) -> Result<Self> {
        if asset.manufacturer.is_empty() {
            return Err(Error::InvalidConfig("asset_tags.manufacturer is empty"));
        }
        if asset.product.is_empty() {
            return Err(Error::InvalidConfig("asset_tags.product is empty"));
        }
        if asset.instance_id.is_empty() {
            return Err(Error::InvalidConfig("asset_tags.instance_id is empty"));
        }
        if asset.functions.is_empty() {
            return Err(Error::InvalidConfig(
                "asset_tags.functions must contain at least one entry",
            ));
        }
        for f in &asset.functions {
            if f.is_empty() {
                return Err(Error::InvalidConfig(
                    "asset_tags.functions entries must be non-empty",
                ));
            }
        }
        let manufacturer = CString::new(asset.manufacturer.as_str())?;
        let product = CString::new(asset.product.as_str())?;
        let instance_id = CString::new(asset.instance_id.as_str())?;
        let functions: Vec<CString> = asset
            .functions
            .iter()
            .map(|f| CString::new(f.as_str()))
            .collect::<std::result::Result<_, _>>()?;
        let function_ptrs: Vec<*const c_char> = functions.iter().map(|s| s.as_ptr()).collect();
        Ok(Self {
            manufacturer,
            product,
            instance_id,
            functions,
            function_ptrs,
        })
    }

    fn sys(&self) -> sys::NvNmosAssetConfig {
        sys::NvNmosAssetConfig {
            manufacturer: self.manufacturer.as_ptr(),
            product: self.product.as_ptr(),
            instance_id: self.instance_id.as_ptr(),
            functions: self.function_ptrs.as_ptr() as *mut _,
            num_functions: self.function_ptrs.len() as u32,
        }
    }
}

/// Owns the C strings backing a [`sys::NvNmosNetworkServicesConfig`].
/// Same lifetime contract as [`MarshalledAssetConfig`].
struct MarshalledNetworkServicesConfig {
    domain: Option<CString>,
    registration_address: Option<CString>,
    registration_version: Option<CString>,
    system_address: Option<CString>,
    system_version: Option<CString>,
    registration_port: u32,
    system_port: u32,
}

impl MarshalledNetworkServicesConfig {
    fn new(services: &NetworkServicesConfig) -> Result<Self> {
        let cstr = |s: &str| -> Result<Option<CString>> {
            if s.is_empty() {
                Ok(None)
            } else {
                Ok(Some(CString::new(s)?))
            }
        };
        Ok(Self {
            domain: cstr(&services.domain)?,
            registration_address: cstr(&services.registration_address)?,
            registration_version: cstr(&services.registration_version)?,
            system_address: cstr(&services.system_address)?,
            system_version: cstr(&services.system_version)?,
            registration_port: u32::from(services.registration_port),
            system_port: u32::from(services.system_port),
        })
    }

    fn sys(&self) -> sys::NvNmosNetworkServicesConfig {
        sys::NvNmosNetworkServicesConfig {
            domain: self.domain.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
            registration_address: self
                .registration_address
                .as_ref()
                .map_or(ptr::null(), |s| s.as_ptr()),
            registration_port: self.registration_port,
            registration_version: self
                .registration_version
                .as_ref()
                .map_or(ptr::null(), |s| s.as_ptr()),
            system_address: self
                .system_address
                .as_ref()
                .map_or(ptr::null(), |s| s.as_ptr()),
            system_port: self.system_port,
            system_version: self
                .system_version
                .as_ref()
                .map_or(ptr::null(), |s| s.as_ptr()),
        }
    }
}

/// Network-services configuration. Mirrors `NvNmosNetworkServicesConfig`
/// from `nvnmos.h`.
///
/// `domain` controls DNS-SD ("local" to force mDNS); when
/// `registration_address` is set DNS-SD is disabled and the supplied
/// IS-04 Registration API is used directly. `system_address` further
/// configures an IS-09 System API; it is honoured only when
/// `registration_address` is also set.
///
/// Each `*_port` of 0 is interpreted by libnvnmos as the protocol
/// default (80 for HTTP). Each `*_version` of empty string falls back
/// to libnvnmos's per-API default (`v1.3` for IS-04 Registration,
/// `v1.0` for IS-09 System).
#[derive(Debug, Clone, Default)]
pub struct NetworkServicesConfig {
    /// DNS domain, or `"local"` to force mDNS. Empty for automatic.
    pub domain: String,
    /// Host name or IP of a fixed IS-04 Registration API. Empty falls
    /// back to DNS-SD.
    pub registration_address: String,
    /// Port for the fixed IS-04 Registration API. 0 → 80.
    pub registration_port: u16,
    /// Version of the fixed IS-04 Registration API. Empty → `v1.3`.
    pub registration_version: String,
    /// Host name or IP of a fixed IS-09 System API. Honoured only when
    /// `registration_address` is also set. Empty disables IS-09.
    pub system_address: String,
    /// Port for the fixed IS-09 System API. 0 → 80.
    pub system_port: u16,
    /// Version of the fixed IS-09 System API. Empty → `v1.0`.
    pub system_version: String,
}

/// Configuration for a sender to add to a running [`NodeServer`].
#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// Transport used by the sender.
    pub transport: Transport,
    /// Transport file. SDP for [`Transport::Rtp`], MXL flow JSON for
    /// [`Transport::Mxl`]. See the C header for the supported attributes /
    /// properties and the `x-nvnmos-*` extensions.
    pub transport_file: String,
}

/// Configuration for a receiver to add to a running [`NodeServer`].
#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    /// Transport used by the receiver.
    pub transport: Transport,
    /// Transport file. SDP for [`Transport::Rtp`], MXL flow JSON for
    /// [`Transport::Mxl`]. See the C header for the supported attributes /
    /// properties and the `x-nvnmos-*` extensions.
    pub transport_file: String,
}

// ============================================================================
// Log levels (re-exported for ergonomics)
// ============================================================================

/// Low-level debugging information.
pub const LOG_LEVEL_DEVEL: i32 = sys::NVNMOS_LOG_DEVEL;
/// Chatty messages such as detailed API request / response tracking.
pub const LOG_LEVEL_VERBOSE: i32 = sys::NVNMOS_LOG_VERBOSE;
/// Higher-level information about expected API events.
pub const LOG_LEVEL_INFO: i32 = sys::NVNMOS_LOG_INFO;
/// Minor problems that may be recovered automatically by the library.
pub const LOG_LEVEL_WARNING: i32 = sys::NVNMOS_LOG_WARNING;
/// More serious recoverable errors.
pub const LOG_LEVEL_ERROR: i32 = sys::NVNMOS_LOG_ERROR;
/// Errors unlikely to be recoverable without restarting the server.
pub const LOG_LEVEL_SEVERE: i32 = sys::NVNMOS_LOG_SEVERE;
/// Errors likely to cause the server to terminate immediately.
pub const LOG_LEVEL_FATAL: i32 = sys::NVNMOS_LOG_FATAL;

// ============================================================================
// NodeServer
// ============================================================================

/// A running NMOS Node server.
///
/// Construct with [`NodeServer::new`] (no callbacks) or
/// [`NodeServer::builder`] when one or both of the activation / log
/// callbacks are required. The handle owns the underlying C
/// `NvNmosNodeServer` and tears it down on drop via
/// `destroy_nmos_node_server`.
pub struct NodeServer {
    raw: Box<sys::NvNmosNodeServer>,
    /// Pinned via `Box` so `user_data` can hold a stable pointer for the
    /// trampolines. `None` when no callbacks were installed. Dropped after
    /// `destroy_nmos_node_server` has returned (which joins libnvnmos's
    /// callback threads). Held but never read directly by Rust; the read
    /// path is through `raw.user_data` inside [`activation_trampoline`] and
    /// [`log_trampoline`].
    #[allow(dead_code)]
    callback_state: Option<Box<CallbackState>>,
}

// SAFETY: `libnvnmos` serialises all model mutations through the `nmos-cpp`
// model lock, so it is safe to share a `&NodeServer` across threads and
// invoke `&self`-taking operations concurrently. There is no per-handle
// state on the Rust side beyond the boxed `NvNmosNodeServer`, so movement
// across threads is also fine.
unsafe impl Send for NodeServer {}
unsafe impl Sync for NodeServer {}

/// Builder for a [`NodeServer`].
///
/// Returned by [`NodeServer::builder`]. Use the chained setters to install
/// optional callbacks, then call [`NodeServerBuilder::build`] to start the
/// underlying C node server. Each setter takes ownership of the previously
/// installed callback (if any), so calling the same setter twice keeps only
/// the second closure.
///
/// ```no_run
/// use nvnmos::{NodeConfig, NodeServer, Side};
///
/// let config = NodeConfig { seed: "demo".into(), ..NodeConfig::default() };
/// let server = NodeServer::builder(&config)
///     .on_activation(|act| {
///         let role = match act.side {
///             Side::Sender => "sender",
///             Side::Receiver => "receiver",
///         };
///         println!("activation for {role} {}", act.name);
///         Ok(())
///     })
///     .on_log(|msg| eprintln!("[nvnmos] {}", msg.message))
///     .build()
///     .expect("create_nmos_node_server failed");
/// # drop(server);
/// ```
#[must_use = "NodeServerBuilder does nothing until .build() is called"]
pub struct NodeServerBuilder<'a> {
    config: &'a NodeConfig,
    activation: Option<ActivationCallback>,
    channelmapping_activation: Option<ChannelMappingActivationCallback>,
    log: Option<LogCallback>,
}

impl<'a> NodeServerBuilder<'a> {
    /// Install an IS-05 connection activation callback.
    ///
    /// The callback runs on a libnvnmos worker thread (it must therefore be
    /// `Send + Sync + 'static`) and is invoked synchronously: libnvnmos
    /// blocks the IS-05 PATCH request until the callback returns. Returning
    /// `Ok(())` reports success to the IS-05 controller; returning
    /// `Err(reason)` reports failure. The `reason` is currently consumed by
    /// the trampoline because the C callback signature has no place to
    /// surface it — it can still be logged or recorded inside the closure if
    /// useful.
    ///
    /// Panics inside the callback are caught and treated as failure (a panic
    /// must not unwind across the FFI boundary).
    ///
    /// Note: [`NodeServer::activate_connection`] (the out-of-band sync path)
    /// does **not** invoke this callback, per libnvnmos's
    /// `nmos_connection_activate` contract.
    pub fn on_activation<F>(mut self, callback: F) -> Self
    where
        F: Fn(&Activation<'_>) -> std::result::Result<(), String> + Send + Sync + 'static,
    {
        self.activation = Some(Box::new(callback));
        self
    }

    /// Install an IS-08 channel mapping activation callback.
    ///
    /// Invoked synchronously on a libnvnmos worker thread when an IS-08
    /// controller activates an output. Returning `Ok(())` reports success;
    /// `Err(_)` NACKs the activation. Panics are caught and treated as
    /// failure. [`NodeServer::activate_channelmapping`] does **not** invoke
    /// this callback.
    pub fn on_channelmapping_activation<F>(mut self, callback: F) -> Self
    where
        F: Fn(&ChannelMappingActivation<'_>) -> std::result::Result<(), String>
            + Send
            + Sync
            + 'static,
    {
        self.channelmapping_activation = Some(Box::new(callback));
        self
    }

    /// Install a log-message callback.
    ///
    /// Invoked from any libnvnmos worker thread for every message at or above
    /// [`NodeConfig::log_level`]; surfacing it is the only way to see what
    /// the C library is doing internally (HTTP API requests, IS-05
    /// activation exceptions, mDNS state changes, ...). Must be
    /// `Send + Sync + 'static`. Panics inside the callback are caught and
    /// swallowed (the trampoline falls back to "no log callback installed").
    pub fn on_log<F>(mut self, callback: F) -> Self
    where
        F: Fn(&LogMessage<'_>) + Send + Sync + 'static,
    {
        self.log = Some(Box::new(callback));
        self
    }

    /// Create and start the underlying C node server.
    pub fn build(self) -> Result<NodeServer> {
        NodeServer::create(
            self.config,
            self.activation,
            self.channelmapping_activation,
            self.log,
        )
    }
}

impl NodeServer {
    /// Create and start a server with no callbacks installed.
    ///
    /// Activations performed by remote IS-05 controllers are still accepted
    /// by libnvnmos and recorded in the IS-04 / IS-05 model, but the
    /// application receives no notification and libnvnmos's slog output is
    /// dropped. Use [`Self::builder`] to opt into either.
    ///
    /// Shorthand for `NodeServer::builder(config).build()`.
    pub fn new(config: &NodeConfig) -> Result<Self> {
        Self::create(config, None, None, None)
    }

    /// Begin building a server with one or more optional callbacks. See
    /// [`NodeServerBuilder`].
    pub fn builder(config: &NodeConfig) -> NodeServerBuilder<'_> {
        NodeServerBuilder {
            config,
            activation: None,
            channelmapping_activation: None,
            log: None,
        }
    }

    fn create(
        config: &NodeConfig,
        activation_callback: Option<ActivationCallback>,
        channelmapping_activation_callback: Option<ChannelMappingActivationCallback>,
        log_callback: Option<LogCallback>,
    ) -> Result<Self> {
        // Marshal Rust strings to C strings. These have to outlive the
        // `create_nmos_node_server` call but not the resulting server — the
        // C library copies what it needs into the IS-04/IS-05 model.
        let cseed = if config.seed.is_empty() {
            None
        } else {
            Some(CString::new(config.seed.as_str())?)
        };
        let chost_name = if config.host_name.is_empty() {
            None
        } else {
            Some(CString::new(config.host_name.as_str())?)
        };
        let chost_addresses: Vec<CString> = config
            .host_addresses
            .iter()
            .map(|s| CString::new(s.as_str()))
            .collect::<std::result::Result<_, _>>()?;
        let chost_address_ptrs: Vec<*const c_char> =
            chost_addresses.iter().map(|s| s.as_ptr()).collect();
        let clabel = if config.label.is_empty() {
            None
        } else {
            Some(CString::new(config.label.as_str())?)
        };
        let cdesc = if config.description.is_empty() {
            None
        } else {
            Some(CString::new(config.description.as_str())?)
        };

        // Marshal optional asset_tags. The C contract requires the
        // outer NvNmosAssetConfig pointer to be non-null *only* when
        // every inner string is non-null and `num_functions > 0`;
        // surface that as a Rust-side validation now rather than
        // depending on libnvnmos's late error.
        let asset = match config.asset_tags.as_ref() {
            None => None,
            Some(a) => Some(MarshalledAssetConfig::new(a)?),
        };
        let sys_asset = asset.as_ref().map(|m| m.sys());
        let asset_ptr = sys_asset
            .as_ref()
            .map_or(ptr::null_mut(), |s| s as *const _ as *mut _);

        let services = match config.network_services.as_ref() {
            None => None,
            Some(n) => Some(MarshalledNetworkServicesConfig::new(n)?),
        };
        let sys_services = services.as_ref().map(|m| m.sys());
        let services_ptr = sys_services
            .as_ref()
            .map_or(ptr::null_mut(), |s| s as *const _ as *mut _);

        let callback_state = if activation_callback.is_some()
            || channelmapping_activation_callback.is_some()
            || log_callback.is_some()
        {
            Some(Box::new(CallbackState {
                activation: activation_callback,
                channelmapping_activation: channelmapping_activation_callback,
                log: log_callback,
            }))
        } else {
            None
        };

        let sys_config = sys::NvNmosNodeConfig {
            seed: cseed.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
            host_name: chost_name.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
            host_addresses: if chost_address_ptrs.is_empty() {
                ptr::null_mut()
            } else {
                chost_address_ptrs.as_ptr() as *mut _
            },
            num_host_addresses: chost_address_ptrs.len() as u32,
            http_port: u32::from(config.http_port),
            label: clabel.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
            description: cdesc.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
            asset_tags: asset_ptr,
            network_services: services_ptr,
            log_level: config.log_level,
            connection_activated: callback_state
                .as_ref()
                .filter(|s| s.activation.is_some())
                .map(|_| activation_trampoline as unsafe extern "C" fn(_, _, _, _) -> _),
            channelmapping_activated: callback_state
                .as_ref()
                .filter(|s| s.channelmapping_activation.is_some())
                .map(|_| {
                    channelmapping_activation_trampoline as unsafe extern "C" fn(_, _, _, _, _) -> _
                }),
            log_callback: callback_state
                .as_ref()
                .filter(|s| s.log.is_some())
                .map(|_| log_trampoline as unsafe extern "C" fn(_, _, _, _)),
            ..Default::default()
        };

        let mut raw = Box::<sys::NvNmosNodeServer>::default();
        // Install the trampolines' anchor point *before* libnvnmos can fire
        // any callback. `Box::as_ref` yields a stable pointer that lives as
        // long as `callback_state` is held by `self`.
        if let Some(state) = callback_state.as_ref() {
            raw.user_data = state.as_ref() as *const CallbackState as *mut std::ffi::c_void;
        }
        let ok = unsafe { sys::create_nmos_node_server(&sys_config, &mut *raw) };
        if !ok {
            return Err(Error::Failed("create_nmos_node_server"));
        }
        Ok(Self {
            raw,
            callback_state,
        })
    }

    /// Add a sender to the running server.
    ///
    /// The sender's name (used by `remove_sender`, `sender_id`, and the
    /// connection-activation callback) is the `x-nvnmos-name` SDP attribute
    /// or the `urn:x-nvnmos:tag:name` flow-def tag carried by
    /// `config.transport_file`.
    pub fn add_sender(&self, config: &SenderConfig) -> Result<()> {
        let ctf = CString::new(config.transport_file.as_str())?;
        let sys_cfg = sys::NvNmosSenderConfig {
            transport: config.transport.to_ffi(),
            transport_file: ctf.as_ptr(),
        };
        let ok = unsafe { sys::add_nmos_sender_to_node_server(self.raw_ptr_mut(), &sys_cfg) };
        if !ok {
            return Err(Error::Failed("add_nmos_sender_to_node_server"));
        }
        Ok(())
    }

    /// Add a receiver to the running server.
    ///
    /// The receiver's name (used by `remove_receiver`, `receiver_id`,
    /// and the connection-activation callback) is the `x-nvnmos-name` SDP
    /// attribute or the `urn:x-nvnmos:tag:name` flow-def tag carried by
    /// `config.transport_file`.
    pub fn add_receiver(&self, config: &ReceiverConfig) -> Result<()> {
        let ctf = CString::new(config.transport_file.as_str())?;
        let sys_cfg = sys::NvNmosReceiverConfig {
            transport: config.transport.to_ffi(),
            transport_file: ctf.as_ptr(),
        };
        let ok = unsafe { sys::add_nmos_receiver_to_node_server(self.raw_ptr_mut(), &sys_cfg) };
        if !ok {
            return Err(Error::Failed("add_nmos_receiver_to_node_server"));
        }
        Ok(())
    }

    /// Remove a sender by its name.
    pub fn remove_sender(&self, sender_name: &str) -> Result<()> {
        let cname = CString::new(sender_name)?;
        let ok =
            unsafe { sys::remove_nmos_sender_from_node_server(self.raw_ptr_mut(), cname.as_ptr()) };
        if !ok {
            return Err(Error::Failed("remove_nmos_sender_from_node_server"));
        }
        Ok(())
    }

    /// Remove a receiver by its name.
    pub fn remove_receiver(&self, receiver_name: &str) -> Result<()> {
        let cname = CString::new(receiver_name)?;
        let ok = unsafe {
            sys::remove_nmos_receiver_from_node_server(self.raw_ptr_mut(), cname.as_ptr())
        };
        if !ok {
            return Err(Error::Failed("remove_nmos_receiver_from_node_server"));
        }
        Ok(())
    }

    /// Add channel mapping I/O to the Node.
    ///
    /// `name` is the caller-chosen name of the channel mapping (libnvnmos
    /// settings index), unique per Node.
    /// `mapping` carries IS-08 input/output ids and caller-chosen resource names for IS-04 linkage.
    pub fn add_channelmapping(&self, name: &str, mapping: &ChannelMappingConfig) -> Result<()> {
        let marshalled = MarshalledChannelMapping::new(mapping)?;
        let cname = CString::new(name)?;
        let ok = unsafe {
            sys::add_nmos_channelmapping_to_node_server(
                self.raw_ptr_mut(),
                cname.as_ptr(),
                marshalled.as_ptr(),
            )
        };
        if !ok {
            return Err(Error::Failed("add_nmos_channelmapping_to_node_server"));
        }
        Ok(())
    }

    /// Remove a channel mapping by `name`.
    pub fn remove_channelmapping(&self, name: &str) -> Result<()> {
        let cname = CString::new(name)?;
        let ok = unsafe {
            sys::remove_nmos_channelmapping_from_node_server(self.raw_ptr_mut(), cname.as_ptr())
        };
        if !ok {
            return Err(Error::Failed("remove_nmos_channelmapping_from_node_server"));
        }
        Ok(())
    }

    /// Publish an out-of-band active map (data-plane → model).
    ///
    /// Mirrors [`nmos_channelmapping_activate`]: does **not** invoke the
    /// channelmapping activation callback.
    pub fn activate_channelmapping(
        &self,
        name: &str,
        output_id: &str,
        active_map: &[ChannelMappingActiveMapEntry],
    ) -> Result<()> {
        let cname = CString::new(name)?;
        let coutput_id = CString::new(output_id)?;
        let marshalled = MarshalledActiveMap::new(active_map)?;
        let ok = unsafe {
            sys::nmos_channelmapping_activate(
                self.raw_ptr_mut(),
                cname.as_ptr(),
                coutput_id.as_ptr(),
                marshalled.as_ptr(),
                marshalled.len(),
            )
        };
        if !ok {
            return Err(Error::Failed("nmos_channelmapping_activate"));
        }
        Ok(())
    }

    /// Notify the server that a sender or receiver has been activated (or
    /// deactivated) out-of-band, so the IS-04/IS-05 model can be updated to
    /// match. `side` selects between a Sender and a Receiver with the same
    /// `name` on this Node.
    ///
    /// `transport_file` is the new transport file data, or `None` to mark the
    /// resource as deactivated.
    ///
    /// This is the *out-of-band* path and, per libnvnmos's
    /// `nmos_connection_activate` contract, does **not** invoke the
    /// activation callback installed via
    /// [`NodeServerBuilder::on_activation`]. The callback fires only for
    /// activations driven by remote IS-05 PATCHes.
    pub fn activate_connection(
        &self,
        side: Side,
        name: &str,
        transport_file: Option<&str>,
    ) -> Result<()> {
        let cname = CString::new(name)?;
        let ctf = transport_file.map(CString::new).transpose()?;
        let tf_ptr = ctf.as_ref().map_or(ptr::null(), |s| s.as_ptr());
        let ok = unsafe {
            sys::nmos_connection_activate(self.raw_ptr_mut(), side.to_ffi(), cname.as_ptr(), tf_ptr)
        };
        if !ok {
            return Err(Error::Failed("nmos_connection_activate"));
        }
        Ok(())
    }

    /// Look up the NMOS Node (`/self`) UUID of this running server.
    pub fn node_id(&self) -> Result<String> {
        let mut buf = [0u8; ID_BUF_LEN];
        let ok = unsafe {
            sys::nmos_get_node_id(&*self.raw, buf.as_mut_ptr() as *mut c_char, buf.len())
        };
        if !ok {
            return Err(Error::Failed("nmos_get_node_id"));
        }
        capture_id(&buf)
    }

    /// Look up the NMOS Device UUID of this running server.
    ///
    /// NvNmos creates one Device per Node.
    pub fn device_id(&self) -> Result<String> {
        let mut buf = [0u8; ID_BUF_LEN];
        let ok = unsafe {
            sys::nmos_get_device_id(&*self.raw, buf.as_mut_ptr() as *mut c_char, buf.len())
        };
        if !ok {
            return Err(Error::Failed("nmos_get_device_id"));
        }
        capture_id(&buf)
    }

    /// Look up a sender's NMOS UUID by its name.
    ///
    /// Returns `Ok(None)` if no sender with the given `sender_name`
    /// currently exists on this server.
    pub fn sender_id(&self, sender_name: &str) -> Result<Option<String>> {
        let cname = CString::new(sender_name)?;
        let mut buf = [0u8; ID_BUF_LEN];
        let ok = unsafe {
            sys::nmos_get_sender_id(
                &*self.raw,
                cname.as_ptr(),
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
            )
        };
        if !ok {
            return Ok(None);
        }
        Ok(Some(capture_id(&buf)?))
    }

    /// Look up a receiver's NMOS UUID by its name.
    ///
    /// Returns `Ok(None)` if no receiver with the given `receiver_name`
    /// currently exists on this server.
    pub fn receiver_id(&self, receiver_name: &str) -> Result<Option<String>> {
        let cname = CString::new(receiver_name)?;
        let mut buf = [0u8; ID_BUF_LEN];
        let ok = unsafe {
            sys::nmos_get_receiver_id(
                &*self.raw,
                cname.as_ptr(),
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
            )
        };
        if !ok {
            return Ok(None);
        }
        Ok(Some(capture_id(&buf)?))
    }

    /// Look up the IS-04 Source UUID paired with a sender by its name.
    ///
    /// Returns `Ok(None)` if no sender with the given `sender_name`
    /// currently exists on this server. This is the Source id (IS-08
    /// `/sourceid`), not the Sender id from [`sender_id`](Self::sender_id).
    pub fn source_id(&self, sender_name: &str) -> Result<Option<String>> {
        let cname = CString::new(sender_name)?;
        let mut buf = [0u8; ID_BUF_LEN];
        let ok = unsafe {
            sys::nmos_get_source_id(
                &*self.raw,
                cname.as_ptr(),
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
            )
        };
        if !ok {
            return Ok(None);
        }
        Ok(Some(capture_id(&buf)?))
    }

    /// Look up the IS-04 Flow UUID paired with a sender by its name.
    ///
    /// Returns `Ok(None)` if no sender with the given `sender_name`
    /// currently exists on this server. This is the Flow id (the Sender's
    /// `flow_id` property), not the Sender id from [`sender_id`](Self::sender_id).
    /// The MXL `mxl_flow_id` IS-05 transport parameter may be overridden
    /// but is the same by default.
    pub fn flow_id(&self, sender_name: &str) -> Result<Option<String>> {
        let cname = CString::new(sender_name)?;
        let mut buf = [0u8; ID_BUF_LEN];
        let ok = unsafe {
            sys::nmos_get_flow_id(
                &*self.raw,
                cname.as_ptr(),
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
            )
        };
        if !ok {
            return Ok(None);
        }
        Ok(Some(capture_id(&buf)?))
    }

    fn raw_ptr_mut(&self) -> *mut sys::NvNmosNodeServer {
        // SAFETY (justifying the `&self` → `*mut` cast): the C API takes
        // `NvNmosNodeServer *` for every mutating call, but its internal model
        // lock serialises all mutations, so `&self` is the right Rust-side
        // contract: callers can share the handle freely across threads.
        &*self.raw as *const _ as *mut _
    }
}

impl Drop for NodeServer {
    fn drop(&mut self) {
        // SAFETY: `raw` was produced by `create_nmos_node_server` in
        // `create` and is only ever destroyed here. `destroy_nmos_node_server`
        // joins libnvnmos's worker threads, so no more activation or log
        // callbacks can fire after it returns — which means the subsequent
        // drop of `callback_state` (the box backing `raw.user_data`) is
        // race-free. Ignore the return value because there is nothing useful
        // to do on a destroy failure during drop.
        unsafe {
            sys::destroy_nmos_node_server(&mut *self.raw);
        }
        // `callback_state` drops here in field order; nothing else to do.
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_uuid_shape(id: &str) {
        assert_eq!(id.len(), 36, "expected 36-char UUID, got {id:?}");
        let parts: Vec<usize> = id.split('-').map(str::len).collect();
        assert_eq!(
            parts,
            vec![8, 4, 4, 4, 12],
            "id {id:?} not in 8-4-4-4-12 form"
        );
        assert!(
            id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()),
            "id {id:?} contains non-hex characters",
        );
    }

    #[test]
    fn node_id_has_uuid_shape() {
        let id = make_node_id("test:42").expect("make_node_id");
        assert_uuid_shape(&id);
    }

    #[test]
    fn node_id_is_deterministic() {
        let a = make_node_id("seed-X").unwrap();
        let b = make_node_id("seed-X").unwrap();
        assert_eq!(a, b);
        let c = make_node_id("seed-Y").unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn device_id_has_uuid_shape() {
        let id = make_device_id("test:42").expect("make_device_id");
        assert_uuid_shape(&id);
    }

    #[test]
    fn device_id_is_deterministic_and_distinct_from_node() {
        let node = make_node_id("seed-X").unwrap();
        let a = make_device_id("seed-X").unwrap();
        let b = make_device_id("seed-X").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, node, "device and node ids must not collide");
        let c = make_device_id("seed-Y").unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn sender_and_receiver_ids_diverge() {
        let s = make_sender_id("seed", "video").unwrap();
        let r = make_receiver_id("seed", "video").unwrap();
        assert_uuid_shape(&s);
        assert_uuid_shape(&r);
        assert_ne!(s, r, "sender and receiver ids must not collide");
    }

    #[test]
    fn sender_and_source_ids_diverge() {
        let sender = make_sender_id("seed", "video").unwrap();
        let source = make_source_id("seed", "video").unwrap();
        assert_uuid_shape(&source);
        assert_ne!(sender, source, "sender and source ids must not collide");
        assert_eq!(source, make_source_id("seed", "video").unwrap());
    }

    #[test]
    fn sender_and_flow_ids_diverge() {
        let sender = make_sender_id("seed", "video").unwrap();
        let flow = make_flow_id("seed", "video").unwrap();
        assert_uuid_shape(&flow);
        assert_ne!(sender, flow, "sender and flow ids must not collide");
        assert_eq!(flow, make_flow_id("seed", "video").unwrap());
    }

    #[test]
    fn sender_id_varies_with_name() {
        let a = make_sender_id("seed", "video-1").unwrap();
        let b = make_sender_id("seed", "video-2").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn rejects_interior_nul() {
        assert!(matches!(
            make_node_id("bad\0seed"),
            Err(Error::InteriorNul(_))
        ));
        assert!(matches!(
            make_sender_id("seed", "bad\0name"),
            Err(Error::InteriorNul(_))
        ));
    }

    #[test]
    fn callback_bounds_are_send_sync() {
        // Compile-time witness that the callback bounds are what we promise
        // to libnvnmos: `Send + Sync + 'static`. Mostly insurance that we
        // don't regress the bounds in a future refactor.
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<ActivationCallback>();
        assert_send_sync::<ChannelMappingActivationCallback>();
        assert_send_sync::<LogCallback>();
    }
}
