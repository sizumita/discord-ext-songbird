//! Converters (W3, §8.7): one channel map + `TickResampler` (48 kHz → target rate) per
//! `(SourceKey, rate, channels)`, reference-counted by lanes. Exactly `rate/50` frames per tick.

use crate::dsp::TickResampler;
use crate::dsp::mix::downmix_stereo;
use crate::error::CoreError;
use crate::format::SampleRate;
use crate::limits::FRAMES_PER_TICK_48K;

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
    /// 48 kHz scratch after the channel map (960 samples for mono; unused for stereo).
    mapped: Box<[f32]>,
    resampler: TickResampler,
}

impl std::fmt::Debug for Converter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Converter")
            .field("rate", &self.rate)
            .field("channels", &self.channels)
            .finish()
    }
}

impl Converter {
    /// Construct off the mixer thread (rubato logs in its constructor).
    pub fn new(rate: SampleRate, channels: u8) -> Result<Self, CoreError> {
        if !(1..=2).contains(&channels) {
            return Err(CoreError::invalid("channels must be 1 or 2"));
        }
        let resampler = TickResampler::new(SampleRate::Hz48000, rate, usize::from(channels))?;
        let mapped = if channels == 1 {
            vec![0.0f32; FRAMES_PER_TICK_48K].into_boxed_slice()
        } else {
            Box::default()
        };
        Ok(Converter {
            rate,
            channels,
            mapped,
            resampler,
        })
    }

    /// `out.len() == rate/50 * channels`. Stereo → mono is `(L + R) · 0.5`.
    pub fn process(&mut self, frame48: &[f32; 1920], out: &mut [f32]) {
        if self.channels == 1 {
            downmix_stereo(frame48, &mut self.mapped);
            self.resampler.process_tick(&self.mapped, out);
        } else {
            self.resampler.process_tick(frame48, out);
        }
    }

    /// Samples written per tick (`rate/50 · channels`).
    pub fn output_len(&self) -> usize {
        self.rate.frames_per_tick() * usize::from(self.channels)
    }

    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Latency added by the resampler, in output frames.
    pub fn delay_frames(&self) -> usize {
        self.resampler.delay_frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::measure;
    use crate::rx::test_util::sine48;

    #[test]
    fn every_rate_and_layout_gives_exact_tick_lengths() {
        let src = sine48(1_000.0, 0.5, 960 * 50);
        for rate in SampleRate::ALL {
            for ch in [1u8, 2] {
                let mut c = Converter::new(rate, ch).unwrap();
                let mut out = vec![0.0f32; c.output_len()];
                assert_eq!(out.len(), rate.frames_per_tick() * usize::from(ch));
                let mut all = Vec::new();
                for frame in src.as_chunks::<1920>().0 {
                    c.process(frame, &mut out);
                    all.extend_from_slice(&out);
                }
                // Skip the resampler warm-up, then the level of a 0.5 sine is ≈ −9.03 dBFS RMS.
                let skip = (c.delay_frames() + 64) * usize::from(ch);
                let l = measure(&all[skip..]);
                assert!(
                    (l.rms_dbfs + 9.03).abs() < 0.2,
                    "{rate:?}/{ch}: {}",
                    l.rms_dbfs
                );
            }
        }
    }

    #[test]
    fn mono_downmix_averages_channels() {
        let mut c = Converter::new(SampleRate::Hz48000, 1).unwrap();
        let mut frame = [0.0f32; 1920];
        for f in frame.as_chunks_mut::<2>().0 {
            f[0] = 0.5;
            f[1] = 0.1;
        }
        let mut out = [0.0f32; 960];
        c.process(&frame, &mut out);
        assert!(out.iter().all(|s| (s - 0.3).abs() < 1e-6));
        assert!(Converter::new(SampleRate::Hz16000, 3).is_err());
    }
}
