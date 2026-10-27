//! Per-SSRC Opus decoding and 960-frame tick normalisation (W3, §8.4).
//!
//! Each needed SSRC is decoded once to 48 kHz stereo f32 into a carry (≤ `RX_CARRY_MAX_FRAMES`
//! frames); each tick pops exactly 960 frames, zero-filling any shortfall. A lost packet is
//! concealed with exactly 960 frames.

use std::collections::VecDeque;

use songbird::driver::opus;

use crate::error::CoreError;
use crate::limits::{FRAMES_PER_TICK_48K, RX_CARRY_MAX_FRAMES};
use crate::runtime::{LiveCounter, live};

/// Samples of the decode scratch a caller must provide to [`SsrcState::push_packet`]
/// (120 ms of 48 kHz stereo, the largest Opus packet).
pub const DECODE_SCRATCH_LEN: usize = RX_CARRY_MAX_FRAMES * 2;

/// Consecutive decode errors that reset the decoder.
pub const MAX_ERRORS_IN_ROW: u8 = 5;

/// A sequence jump larger than this (either direction) resets the decoder.
pub const MAX_SEQ_JUMP: u16 = 10;

/// Interleaved samples of one 20 ms stereo tick.
const TICK_SAMPLES: usize = FRAMES_PER_TICK_48K * 2;

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
    /// Composition of the carry, oldest first: `(frames, concealed)` runs (adjacent runs of the
    /// same kind are merged, so this stays a handful of entries).
    runs: VecDeque<(u32, bool)>,
    user: Option<u64>,
    last_seq: Option<u16>,
    last_seen_tick: u64,
    errors_in_row: u8,
    /// Frames dropped because the carry was full (not yet collected by the pipeline).
    overflow_frames: u64,
    _live: LiveCounter,
}

impl std::fmt::Debug for SsrcState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SsrcState")
            .field("carry_frames", &self.carry_frames())
            .field("user", &self.user)
            .field("last_seq", &self.last_seq)
            .finish()
    }
}

impl SsrcState {
    pub fn new() -> Result<Self, CoreError> {
        let decoder = opus::Decoder::new(48_000, opus::Channels::Stereo)
            .map_err(|_| CoreError::Internal("opus decoder"))?;
        Ok(SsrcState {
            decoder,
            carry: VecDeque::new(),
            runs: VecDeque::new(),
            user: None,
            last_seq: None,
            last_seen_tick: 0,
            errors_in_row: 0,
            overflow_frames: 0,
            _live: LiveCounter::new(&live::SSRC_STATE),
        })
    }

    /// Decodes one 20/40/60/120 ms packet into the carry; returns the decoded frames.
    ///
    /// `scratch` must hold at least [`DECODE_SCRATCH_LEN`] samples. A sequence jump of more than
    /// [`MAX_SEQ_JUMP`] resets the decoder first. On a decode error nothing is added (the caller
    /// conceals); [`MAX_ERRORS_IN_ROW`] consecutive errors reset the decoder.
    pub fn push_packet(
        &mut self,
        opus: &[u8],
        seq: u16,
        scratch: &mut [f32],
    ) -> Result<usize, CoreError> {
        if let Some(last) = self.last_seq {
            let delta = seq.wrapping_sub(last).cast_signed();
            if delta.unsigned_abs() > MAX_SEQ_JUMP {
                self.reset();
            }
        }
        self.last_seq = Some(seq);
        let out = scratch;
        match self.decoder.decode_float(opus, out, false) {
            Ok(frames) if frames > 0 => {
                self.errors_in_row = 0;
                if let Some(decoded) = out.get(..frames * 2) {
                    append(
                        &mut self.carry,
                        &mut self.runs,
                        &mut self.overflow_frames,
                        decoded,
                        false,
                    );
                }
                Ok(frames)
            }
            _ => {
                self.errors_in_row = self.errors_in_row.saturating_add(1);
                if self.errors_in_row >= MAX_ERRORS_IN_ROW {
                    self.reset();
                }
                Err(CoreError::InvalidAudio("opus decode failed".into()))
            }
        }
    }

    /// Packet-loss concealment: exactly 960 frames into the carry (silence if libopus fails).
    ///
    /// `scratch` must hold at least 1920 samples.
    pub fn conceal(&mut self, scratch: &mut [f32]) {
        let Some(out) = scratch.get_mut(..TICK_SAMPLES) else {
            return;
        };
        let frames = match self.decoder.decode_float(&[], out, false) {
            Ok(n) => n.min(FRAMES_PER_TICK_48K),
            Err(_) => 0,
        };
        if let Some(rest) = out.get_mut(frames * 2..) {
            rest.fill(0.0);
        }
        append(
            &mut self.carry,
            &mut self.runs,
            &mut self.overflow_frames,
            out,
            true,
        );
    }

    /// Pops exactly 960 stereo frames (zero-filled shortfall).
    pub fn take_tick(&mut self, out: &mut [f32; 1920]) -> TickFill {
        let n = self.carry_frames().min(FRAMES_PER_TICK_48K);
        let mut written = 0;
        for (o, s) in out.iter_mut().zip(self.carry.drain(..n * 2)) {
            *o = s;
            written += 1;
        }
        if let Some(rest) = out.get_mut(written..) {
            rest.fill(0.0);
        }
        let (real, concealed) = consume_runs(&mut self.runs, n);
        TickFill {
            real_frames: real,
            concealed_frames: concealed,
        }
    }

    /// Resets the decoder and the carry (five consecutive errors, sequence jump > 10).
    pub fn reset(&mut self) {
        let _ = self.decoder.reset_state();
        self.carry.clear();
        self.runs.clear();
        self.last_seq = None;
        self.errors_in_row = 0;
    }

    /// Frames waiting in the carry.
    pub fn carry_frames(&self) -> usize {
        self.carry.len() / 2
    }

    /// The user this SSRC is mapped to.
    pub fn user(&self) -> Option<u64> {
        self.user
    }

    pub fn set_user(&mut self, user: Option<u64>) {
        self.user = user;
    }

    /// Tick at which a packet (or loss) for this SSRC was last seen.
    pub fn last_seen_tick(&self) -> u64 {
        self.last_seen_tick
    }

    pub fn touch(&mut self, tick: u64) {
        self.last_seen_tick = tick;
    }

    /// Takes the count of frames dropped because the carry was full.
    pub fn take_overflow_frames(&mut self) -> u64 {
        std::mem::take(&mut self.overflow_frames)
    }
}

/// Appends interleaved stereo `samples` to the carry, dropping the oldest frames beyond
/// `RX_CARRY_MAX_FRAMES` (counted in `overflow`).
fn append(
    carry: &mut VecDeque<f32>,
    runs: &mut VecDeque<(u32, bool)>,
    overflow: &mut u64,
    samples: &[f32],
    concealed: bool,
) {
    let frames = (samples.len() / 2).min(RX_CARRY_MAX_FRAMES);
    let have = carry.len() / 2;
    let excess = (have + frames).saturating_sub(RX_CARRY_MAX_FRAMES);
    if excess > 0 {
        carry.drain(..excess * 2);
        consume_runs(runs, excess);
        *overflow = overflow.saturating_add(excess as u64);
    }
    let skip = samples.len() / 2 - frames;
    carry.extend(samples.iter().skip(skip * 2).take(frames * 2));
    let frames = u32::try_from(frames).unwrap_or(u32::MAX);
    match runs.back_mut() {
        Some((n, kind)) if *kind == concealed => *n = n.saturating_add(frames),
        _ => runs.push_back((frames, concealed)),
    }
}

/// Removes `frames` from the front of `runs`; returns `(real, concealed)` frames removed.
fn consume_runs(runs: &mut VecDeque<(u32, bool)>, frames: usize) -> (u32, u32) {
    let mut left = u32::try_from(frames).unwrap_or(u32::MAX);
    let (mut real, mut concealed) = (0u32, 0u32);
    while left > 0 {
        let Some((n, kind)) = runs.front_mut() else {
            break;
        };
        let take = (*n).min(left);
        if *kind {
            concealed += take;
        } else {
            real += take;
        }
        *n -= take;
        left -= take;
        if *n == 0 {
            runs.pop_front();
        }
    }
    (real, concealed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rx::test_util::{opus_frames, opus_packet_ms, sine48};

    fn scratch() -> Vec<f32> {
        vec![0.0; DECODE_SCRATCH_LEN]
    }

    #[test]
    fn concealment_is_exactly_960_frames() {
        let mut st = SsrcState::new().unwrap();
        let mut s = scratch();
        st.conceal(&mut s);
        assert_eq!(st.carry_frames(), 960);
        let mut out = [1.0f32; 1920];
        let fill = st.take_tick(&mut out);
        assert_eq!(
            fill,
            TickFill {
                real_frames: 0,
                concealed_frames: 960
            }
        );
        assert_eq!(st.carry_frames(), 0);
        // After real audio the concealment is still exactly one tick.
        let pkts = opus_frames(&sine48(440.0, 0.5, 960 * 4));
        for (i, p) in pkts.iter().enumerate() {
            st.push_packet(p, i as u16, &mut s).unwrap();
            st.take_tick(&mut out);
        }
        st.conceal(&mut s);
        assert_eq!(st.carry_frames(), 960);
        let fill = st.take_tick(&mut out);
        assert_eq!(fill.concealed_frames, 960);
        assert!(out.iter().any(|x| *x != 0.0), "PLC continues the signal");
    }

    #[test]
    fn multi_frame_packets_normalise_to_960_per_tick() {
        let mut s = scratch();
        let mut out = [0.0f32; 1920];
        for ms in [20usize, 40, 60, 120] {
            let mut st = SsrcState::new().unwrap();
            let p = opus_packet_ms(ms, 0.4);
            let frames = st.push_packet(&p, 1, &mut s).unwrap();
            assert_eq!(frames, 48 * ms, "{ms} ms");
            assert_eq!(st.carry_frames(), 48 * ms);
            for t in 0..ms / 20 {
                let fill = st.take_tick(&mut out);
                assert_eq!(fill.real_frames, 960, "{ms} ms tick {t}");
                assert_eq!(st.carry_frames(), 48 * ms - 960 * (t + 1));
            }
            // Shortfall is zero-filled.
            out.fill(1.0);
            let fill = st.take_tick(&mut out);
            assert_eq!(fill, TickFill::default());
            assert!(out.iter().all(|x| *x == 0.0));
        }
    }

    #[test]
    fn partial_tick_mixes_real_and_concealed() {
        let mut st = SsrcState::new().unwrap();
        let mut s = scratch();
        let p = opus_packet_ms(60, 0.4);
        st.push_packet(&p, 1, &mut s).unwrap();
        let mut out = [0.0f32; 1920];
        st.take_tick(&mut out);
        st.take_tick(&mut out);
        st.conceal(&mut s); // 960 real + 960 concealed queued
        let fill = st.take_tick(&mut out);
        assert_eq!(fill.real_frames, 960);
        let fill = st.take_tick(&mut out);
        assert_eq!(fill.concealed_frames, 960);
    }

    #[test]
    fn carry_overflow_drops_oldest_and_counts() {
        let mut st = SsrcState::new().unwrap();
        let mut s = scratch();
        let p = opus_packet_ms(120, 0.4);
        st.push_packet(&p, 1, &mut s).unwrap();
        assert_eq!(st.carry_frames(), RX_CARRY_MAX_FRAMES);
        assert_eq!(st.take_overflow_frames(), 0);
        st.push_packet(&p, 2, &mut s).unwrap();
        assert_eq!(st.carry_frames(), RX_CARRY_MAX_FRAMES);
        assert_eq!(st.take_overflow_frames(), 5_760);
        st.conceal(&mut s);
        assert_eq!(st.carry_frames(), RX_CARRY_MAX_FRAMES);
        assert_eq!(st.take_overflow_frames(), 960);
        // The newest frames survive: the last tick out is the concealed one.
        let mut out = [0.0f32; 1920];
        let mut last = TickFill::default();
        while st.carry_frames() > 0 {
            last = st.take_tick(&mut out);
        }
        assert_eq!(last.concealed_frames, 960);
    }

    #[test]
    fn sequence_jump_resets_the_decoder() {
        let mut st = SsrcState::new().unwrap();
        let mut s = scratch();
        let p = opus_packet_ms(60, 0.4);
        st.push_packet(&p, 100, &mut s).unwrap();
        assert_eq!(st.carry_frames(), 2880);
        // +10 is fine (carry kept).
        st.push_packet(&p, 110, &mut s).unwrap();
        assert_eq!(st.carry_frames(), 5760);
        // +11 resets first: only the new packet remains.
        st.push_packet(&p, 121, &mut s).unwrap();
        assert_eq!(st.carry_frames(), 2880);
        // Backwards jumps and wrap-around.
        st.push_packet(&p, 100, &mut s).unwrap();
        assert_eq!(st.carry_frames(), 2880);
        st.push_packet(&p, 65_530, &mut s).unwrap();
        assert_eq!(st.carry_frames(), 2880);
        st.push_packet(&p, 3, &mut s).unwrap(); // wraps by +9
        assert_eq!(st.carry_frames(), 5760);
    }

    #[test]
    fn decode_errors_reset_after_five() {
        let mut st = SsrcState::new().unwrap();
        let mut s = scratch();
        let p = opus_packet_ms(40, 0.4);
        st.push_packet(&p, 1, &mut s).unwrap();
        let bad = [0xFFu8, 0xFF, 0xFF];
        for i in 0..4u16 {
            assert!(st.push_packet(&bad, 2 + i, &mut s).is_err());
        }
        assert_eq!(
            st.carry_frames(),
            1920,
            "errors alone do not touch the carry"
        );
        assert!(st.push_packet(&bad, 6, &mut s).is_err());
        assert_eq!(st.carry_frames(), 0, "the fifth error resets");
        assert_eq!(st.push_packet(&p, 7, &mut s).unwrap(), 1920);
    }
}
