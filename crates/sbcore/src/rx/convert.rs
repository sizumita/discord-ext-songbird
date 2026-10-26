//! Converters (W3, §8.7): one channel map + `TickResampler` (48 kHz → target rate) per
//! `(SourceKey, rate, channels)`, reference-counted by lanes. Exactly `rate/50` frames per tick.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use crate::dsp::TickResampler;
use crate::error::CoreError;
use crate::format::SampleRate;

/// Where a lane's audio comes from: one user's frame or a mix bus.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum SourceKey {
    User(u64),
    Bus(u32),
}

/// Channel map + resampler from the 48 kHz stereo tick frame to `(rate, channels)`.
pub struct Converter {
    rate: SampleRate,
    channels: u8,
    /// 48 kHz scratch after the channel map (960 × channels).
    mapped: Box<[f32]>,
    resampler: TickResampler,
}

impl Converter {
    /// Construct off the mixer thread (rubato logs in its constructor).
    pub fn new(rate: SampleRate, channels: u8) -> Result<Self, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// `out.len() == rate/50 * channels`.
    pub fn process(&mut self, frame48: &[f32; 1920], out: &mut [f32]) {
        out.fill(0.0);
    }
}
