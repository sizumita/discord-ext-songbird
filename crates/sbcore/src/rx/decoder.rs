//! Per-SSRC Opus decoding and 960-frame tick normalisation (W3, §8.4).
//!
//! Each needed SSRC is decoded once to 48 kHz stereo f32 into a carry (≤ `RX_CARRY_MAX_FRAMES`
//! frames); each tick pops exactly 960 frames, zero-filling any shortfall. A lost packet is
//! concealed with exactly 960 frames.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::VecDeque;

use songbird::driver::opus;

use crate::error::CoreError;
use crate::runtime::LiveCounter;

/// How one tick's 960 frames were filled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickFill {
    pub real_frames: u32,
    pub concealed_frames: u32,
}

/// Decoder state of one SSRC.
pub struct SsrcState {
    /// libopus, 48 kHz stereo.
    decoder: opus::Decoder,
    /// Interleaved stereo f32, ≤ `RX_CARRY_MAX_FRAMES` frames.
    carry: VecDeque<f32>,
    /// Frames of the carry that came from concealment.
    concealed_in_carry: u32,
    user: Option<u64>,
    last_seq: Option<u16>,
    last_seen_tick: u64,
    errors_in_row: u8,
    _live: LiveCounter,
}

impl SsrcState {
    pub fn new() -> Result<Self, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Decodes one 20/40/60/120 ms packet into the carry; returns the decoded frames.
    pub fn push_packet(
        &mut self,
        opus: &[u8],
        seq: u16,
        scratch: &mut [f32],
    ) -> Result<usize, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Packet-loss concealment: exactly 960 frames into the carry.
    pub fn conceal(&mut self, scratch: &mut [f32]) {
        // phase 0: nothing is concealed.
    }

    /// Pops exactly 960 stereo frames (zero-filled shortfall).
    pub fn take_tick(&mut self, out: &mut [f32; 1920]) -> TickFill {
        out.fill(0.0);
        TickFill::default()
    }

    /// Resets the decoder and the carry (five consecutive errors, sequence jump > 10).
    pub fn reset(&mut self) {
        self.carry.clear();
        self.concealed_in_carry = 0;
        self.last_seq = None;
        self.errors_in_row = 0;
    }
}
