//! The lock-free half of `PcmQueue::write` (W4, §7.3 step 1): pre-checks, base64 decoding into a
//! per-call buffer and parsing at the input rate, all **outside** every lock.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use crate::error::CoreError;
use crate::format::PcmParser;

use super::queue::WriteData;

/// A write parsed outside the locks, ready for the L6w splice and the L6 commit.
#[derive(Debug, Default)]
pub(crate) struct ParsedWrite {
    /// Whole frames after the head bytes, interleaved i16 (exactly sized).
    pub(crate) frames: Box<[i16]>,
    /// Bytes completing the carried partial frame (≤ 7).
    pub(crate) head: [u8; 8],
    pub(crate) head_len: u8,
    /// Trailing partial frame: the tentative new carry (≤ 7 bytes).
    pub(crate) tail: [u8; 8],
    pub(crate) tail_len: u8,
    /// The carry length this parse assumed (`st.carry_len` snapshot).
    pub(crate) carry_snapshot: u8,
}

/// Size cap check before any decoding (`LimitExceeded`), then base64 decode + parse.
pub(crate) fn parse_write(
    parser: &PcmParser,
    data: WriteData<'_>,
    carry_len: u8,
    cap_bytes: u64,
) -> Result<ParsedWrite, CoreError> {
    Err(CoreError::Internal("unimplemented"))
}
