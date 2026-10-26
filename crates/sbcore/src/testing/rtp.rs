//! RTP and Opus helpers for receive tests (§13.2).
#![allow(unused_variables)] // phase-0 skeleton

use crate::error::CoreError;

/// Options of [`rtp_wrap`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtpOpts {
    /// Add a one-word RTP header extension.
    pub extension: bool,
    /// Padding bytes (RTP P bit; the last byte is the count).
    pub padding: u8,
    /// Append a DAVE supplemental block (`… | size | FA FA`).
    pub dave_trailer: bool,
}

/// Encodes interleaved 48 kHz stereo PCM16LE into 20 ms Opus packets (zero-padding the last one).
pub fn encode_opus(pcm48_stereo: &[u8]) -> Result<Vec<Vec<u8>>, CoreError> {
    Err(CoreError::Internal("unimplemented"))
}

/// Wraps an Opus payload in an RTP packet as songbird's Decrypt mode would deliver it.
pub fn rtp_wrap(opus: &[u8], seq: u16, ssrc: u32, opts: RtpOpts) -> Result<Vec<u8>, CoreError> {
    Err(CoreError::Internal("unimplemented"))
}
