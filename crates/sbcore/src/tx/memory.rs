//! In-memory sources (W4, §7.9): `AudioFile` bytes (probed eagerly), `RawPcm` samples and
//! `OpusPackets`. Each is copied once into an `Arc` shared by the Python object and every track.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::path::Path;
use std::sync::Arc;

use crate::error::CoreError;
use crate::format::{Encoding, Format};

/// What the eager probe found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbeInfo {
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    pub duration_ms: Option<u64>,
}

/// Encoded audio held in memory (`native.AudioFile`).
#[derive(Debug)]
pub struct EncodedBytes {
    data: Arc<[u8]>,
    info: ProbeInfo,
}

impl EncodedBytes {
    /// Probes eagerly (`Source` error if unrecognised); `info` comes from the probe result.
    pub fn new(data: Vec<u8>) -> Result<Self, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    pub fn info(&self) -> &ProbeInfo {
        &self.info
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The shared bytes (each play gets a fresh cursor over them).
    pub fn data(&self) -> &Arc<[u8]> {
        &self.data
    }
}

/// Reads a whole file (≤ `max_bytes`, `try_reserve`). Called under `py.detach`.
/// I/O failures are `CoreError::Io` (→ `OSError`); a file over `max_bytes` is `CoreError::Limit`.
pub fn read_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>, CoreError> {
    Err(CoreError::Internal("unimplemented"))
}

/// Raw PCM held in memory (`native.RawPcm`), interleaved `i16` at a stream rate.
#[derive(Debug)]
pub struct MemPcm {
    fmt: Format,
    samples: Arc<[i16]>,
}

impl MemPcm {
    /// Whole frames of `data` in `fmt` (parsed once).
    pub fn from_bytes(fmt: Format, data: &[u8]) -> Result<Self, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Takes interleaved samples in `fmt` (length must be whole frames).
    pub fn from_i16(fmt: Format, samples: Vec<i16>) -> Result<Self, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Any rate in `SOURCE_RATE_HZ` (pcm16le/f32le); resampled once to 48 kHz unless 8/16/24/48 kHz.
    pub fn from_any_rate(
        rate_hz: u32,
        channels: u8,
        enc: Encoding,
        data: &[u8],
    ) -> Result<Self, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    pub fn format(&self) -> Format {
        self.fmt
    }

    pub fn duration_ms(&self) -> u64 {
        let frames = self.samples.len() / usize::from(self.fmt.channels.max(1));
        self.fmt.ms_for_frames(frames as u64)
    }
}

/// Pre-encoded 20 ms / 48 kHz Opus packets (`native.OpusPackets`).
#[derive(Debug)]
pub struct OpusFrames {
    bytes: Arc<[u8]>,
    /// `len() + 1` offsets: packet `i` is `bytes[offsets[i]..offsets[i + 1]]`.
    offsets: Arc<[u32]>,
}

impl OpusFrames {
    /// Validates every packet (`validate_opus_packet`, ≤ `MAX_OPUS_PACKETS`, ≤ `MAX_SOURCE_BYTES`).
    pub fn from_packets<'a>(it: impl IntoIterator<Item = &'a [u8]>) -> Result<Self, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Number of packets.
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `len() × 20` ms.
    pub fn duration_ms(&self) -> u64 {
        (self.len() as u64).saturating_mul(20)
    }
}
