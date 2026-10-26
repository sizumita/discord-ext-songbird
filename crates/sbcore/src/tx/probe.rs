//! One probe per input, on the creating thread (W4, §7.1 rule 6).
//!
//! `ProbedMetadata` has no public constructor and no `Clone`, so every `Parsed` needs its own probe:
//! the 16-byte header `b"SbirdRaw" + le32(48000) + le32(ch)` that songbird's `RawReader` recognises
//! supplies `Parsed.meta`, and the drained `MediaSourceStream` (one 64 KiB ring) is what the reader
//! returns from `into_inner`.
#![allow(unused_variables)] // phase-0 skeleton

use songbird::input::core::io::MediaSourceStream;
use songbird::input::core::probe::ProbedMetadata;

use crate::error::CoreError;

/// Probes the raw header for `channels` (1 or 2) and returns its metadata and drained stream.
pub fn probe_raw_header(channels: u8) -> Result<(ProbedMetadata, MediaSourceStream), CoreError> {
    Err(CoreError::Internal("unimplemented"))
}
