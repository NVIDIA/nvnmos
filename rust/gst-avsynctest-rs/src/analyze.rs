// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Recovery-side analysis for the aligned A/V test signal: locate the video
//! bar, the audio pips, and the phase-locked caption produced by
//! [`avsyncvideotestsrc`](crate::videosrc) and
//! [`avsyncaudiotestsrc`](crate::audiosrc). Kept transport-agnostic so any
//! pipeline under test — a bare `videoconvert`, an MXL round-trip, an NMOS
//! sender/receiver — can assert A/V (and caption) alignment on recovered
//! samples the same way.

use cdp_types::CDPParser;
use gstreamer as gst;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;

use crate::captions;

/// Brightest-column centroid of the top row, decoded to `GRAY8`. `None` if the
/// row has no bright pixels (should not happen for the always-present bar).
pub fn bar_centroid(sample: &gst::Sample) -> Option<f64> {
    bar_centroid_at(sample, 0, 128.0 / 255.0)
}

/// Recover the bright bar from one scanline of decoded 8/10-bit video. This
/// generalises the original GRAY8 helper without depending on source packing
/// functions or on the source's phase formula.
pub fn bar_centroid_at(sample: &gst::Sample, row: u32, threshold: f64) -> Option<f64> {
    let caps = sample.caps()?;
    let info = gst_video::VideoInfo::from_caps(caps).ok()?;
    let buffer = sample.buffer()?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    let stride = usize::try_from(frame.plane_stride()[0]).ok()?;
    if row >= info.height() {
        return None;
    }
    let data = frame
        .plane_data(0)
        .ok()?
        .get((row as usize).checked_mul(stride)?..)?;
    let width = info.width() as usize;
    let mut sum = 0.0f64;
    let mut count = 0.0f64;
    for x in 0..width {
        let (v, max) = match info.format() {
            gst_video::VideoFormat::Gray8 => (u16::from(*data.get(x)?), 255.),
            gst_video::VideoFormat::Uyvy => (u16::from(*data.get(x * 2 + 1)?), 255.),
            gst_video::VideoFormat::Yuy2 => (u16::from(*data.get(x * 2)?), 255.),
            gst_video::VideoFormat::Uyvp => {
                let bytes = data.get(x / 2 * 5..x / 2 * 5 + 5)?;
                let packed = bytes.iter().fold(0u64, |a, b| (a << 8) | u64::from(*b));
                (
                    ((packed >> if x % 2 == 0 { 20 } else { 0 }) & 1023) as u16,
                    1023.,
                )
            }
            gst_video::VideoFormat::V210 => {
                let (word, shift) = [(0, 10), (1, 0), (1, 20), (2, 10), (3, 0), (3, 20)][x % 6];
                let offset = x / 6 * 16 + word * 4;
                let packed = u32::from_le_bytes(data.get(offset..offset + 4)?.try_into().ok()?);
                (((packed >> shift) & 1023) as u16, 1023.)
            }
            _ => return None,
        };
        if f64::from(v) > max * threshold {
            sum += x as f64;
            count += 1.0;
        }
    }
    (count > 0.0).then_some(sum / count)
}

/// Buffer bytes that are not `f32`-aligned (wrong length or pointer alignment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MisalignedF32leBytes;

impl std::fmt::Display for MisalignedF32leBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("misaligned F32LE buffer")
    }
}

impl std::error::Error for MisalignedF32leBytes {}

/// Reinterpret an F32LE audio buffer's bytes as native `f32` samples (the host
/// is little-endian). Returns [`MisalignedF32leBytes`] if the buffer is not
/// `f32`-aligned. Typically used to turn a recovered audio buffer into the
/// `(time_ns, amplitude)` pairs [`detect_pips`] consumes.
pub fn f32le_samples(bytes: &[u8]) -> Result<&[f32], MisalignedF32leBytes> {
    let (head, body, tail) = unsafe { bytes.align_to::<f32>() };
    if head.is_empty() && tail.is_empty() {
        Ok(body)
    } else {
        Err(MisalignedF32leBytes)
    }
}

/// Like [`f32le_samples`], but copies when the buffer is not `f32`-aligned
/// (valid GStreamer data, e.g. a sub-buffer at an odd offset). A trailing
/// partial sample is dropped.
pub fn f32le_samples_copied(bytes: &[u8]) -> std::borrow::Cow<'_, [f32]> {
    match f32le_samples(bytes) {
        Ok(samples) => samples.into(),
        Err(_) => bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect::<Vec<_>>()
            .into(),
    }
}

/// Running-time centre of each tone pip from `(time_ns, amplitude)` samples. The
/// envelope is binned (5 ms bins, so a 1 kHz tone's zero-crossings never split a
/// pip) and contiguous bins whose energy clears a fraction of the peak are
/// grouped; each pip's time is the *midpoint* of its group. A midpoint is
/// unbiased for the symmetric burst `avsyncaudiotestsrc` emits, whereas an
/// energy-weighted centroid is skewed by how the burst's samples fall across the
/// bin edges.
pub fn detect_pips(audio: &[(u64, f32)]) -> Vec<u64> {
    let Some(&(t0, _)) = audio.first() else {
        return Vec::new();
    };
    let Some(&(tn, _)) = audio.last() else {
        return Vec::new();
    };
    const BIN_NS: u64 = 5_000_000;
    let nbins = ((tn - t0) / BIN_NS + 1) as usize;
    let mut energy = vec![0.0f64; nbins];
    for &(t, a) in audio {
        let b = ((t - t0) / BIN_NS) as usize;
        energy[b] += (a as f64) * (a as f64);
    }
    let peak = energy.iter().copied().fold(0.0, f64::max);
    let threshold = peak * 0.1;
    let bin_centre = |b: usize| t0 + b as u64 * BIN_NS + BIN_NS / 2;
    let mut pips = Vec::new();
    let mut i = 0;
    while i < nbins {
        if energy[i] > threshold {
            let first = i;
            while i < nbins && energy[i] > threshold {
                i += 1;
            }
            pips.push((bin_centre(first) + bin_centre(i - 1)) / 2);
        } else {
            i += 1;
        }
    }
    pips
}

/// Low byte of the first user-data word of every ancillary packet with DID `did`
/// (matched on its low 8 bits) carried by the frame, in attachment order. Reads
/// back the per-frame index [`avsyncvideotestsrc`](crate::videosrc) stamps in the
/// data flow (DID [`ANC_DID`](crate::signal::ANC_DID)). Usually one entry per
/// frame; a lossy live round-trip whose flows aren't co-timed can land two
/// packets on one frame (and none on a neighbour) — never more than two — so the
/// caller can verify every packet was received and placed within one frame of its
/// home index.
pub fn ancillary_indices(buffer: &gst::BufferRef, did: u8) -> Vec<u8> {
    buffer
        .iter_meta::<gst_video::video_meta::AncillaryMeta>()
        .filter(|m| (m.did() & 0xFF) as u8 == did)
        .filter_map(|m| m.data().iter().next().map(|&w| (w & 0xFF) as u8))
        .collect()
}

/// Raw CDP bytes of every CEA-708 caption ancillary carried by the frame (DID
/// [`CC_DID`](captions::CC_DID)), in attachment order — the caption companion to
/// [`ancillary_indices`], so a frame that collected two ST-2038 packets exposes
/// both captions. Decode each with [`decode_caption`].
pub fn caption_cdps(buffer: &gst::BufferRef) -> Vec<Vec<u8>> {
    buffer
        .iter_meta::<gst_video::video_meta::AncillaryMeta>()
        .filter(|m| (m.did() & 0xFF) as u8 == captions::CC_DID)
        .map(|m| m.data().iter().map(|&w| (w & 0xFF) as u8).collect())
        .collect()
}

/// The CEA-708 CDP the video source attaches (DID [`CC_DID`](captions::CC_DID) /
/// SDID [`CC_SDID`](captions::CC_SDID)), as raw bytes recovered from the
/// ancillary meta's 10-bit words. `None` if the frame carries no caption
/// ancillary.
pub fn caption_cdp_bytes(buffer: &gst::BufferRef) -> Option<Vec<u8>> {
    buffer
        .iter_meta::<gst_video::video_meta::AncillaryMeta>()
        .find(|m| (m.did() & 0xFF) as u8 == captions::CC_DID)
        .map(|m| m.data().iter().map(|&w| (w & 0xFF) as u8).collect())
}

/// The caption text carried by one frame's CDP (service
/// [`CC_SERVICE_NO`](captions::CC_SERVICE_NO)), or `Ok(None)` for a null CDP
/// (a valid CDP that carries no caption). `Err` if the CDP fails to parse.
pub fn decode_caption(
    parser: &mut CDPParser,
    cdp: &[u8],
) -> Result<Option<String>, cdp_types::ParserError> {
    parser.parse(cdp)?;
    let Some(packet) = parser.pop_packet() else {
        return Ok(None);
    };
    Ok(caption_packet_text(&packet))
}

/// Extract the configured service text from an already assembled DTVCC packet.
/// Shared by batch recovery tests and the streaming analyzer.
pub(crate) fn caption_packet_text(packet: &cea708_types::DTVCCPacket) -> Option<String> {
    let mut text = String::new();
    for service in packet.services() {
        if service.number() == captions::CC_SERVICE_NO {
            text.extend(service.codes().iter().filter_map(|code| code.char()));
        }
    }
    (!text.is_empty()).then_some(text)
}
