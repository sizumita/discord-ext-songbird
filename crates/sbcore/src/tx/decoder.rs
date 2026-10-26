//! `PcmDecoder`: the songbird `Decoder` of every live/memory PCM input (W4, §7.1, §7.4).
//!
//! `decode` always yields exactly 960 frames of 48 kHz `f32` in a preallocated `AudioBuffer`: it
//! pulls from the provider (`try_lock` only), resamples with a preallocated `TickResampler`,
//! applies gain, never allocates and never returns 0 frames. It returns `Err` in exactly one case:
//! a failed producer (encoded decode error) whose audio has drained, so that songbird ends the
//! track with `reason="error"` (§7.9).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use songbird::input::core::audio::{
    AsAudioBufferRef, AudioBuffer, AudioBufferRef, Channels, Signal, SignalSpec,
};
use songbird::input::core::codecs::{
    CODEC_TYPE_PCM_F32LE, CodecDescriptor, CodecParameters, Decoder, DecoderOptions, FinalizeResult,
};
use songbird::input::core::errors::{Error as SymphError, Result as SymphResult};
use songbird::input::core::formats::Packet;
use songbird::input::core::sample::SampleFormat;
use songbird::input::core::units::TimeBase;

use super::{PcmProvider, enter_mixer};
use crate::dsp::TickResampler;
use crate::dsp::mix::i16_to_f32;
use crate::error::CoreError;
use crate::format::SampleRate;
use crate::limits::{FRAMES_PER_TICK_48K, MIX_RATE};

/// Post-clear fade-in (2.5 ms at 48 kHz).
pub(crate) const DECLICK_FRAMES: u32 = 120;
/// Volume ramp length (5 ms at 48 kHz).
pub(crate) const VOLUME_RAMP_FRAMES: u32 = 240;

/// Codec parameters of every PCM input we hand to songbird (§7.1 rule 4).
pub(crate) fn pcm_codec_params(channels: u8) -> CodecParameters {
    let layout = if channels >= 2 {
        Channels::FRONT_LEFT | Channels::FRONT_RIGHT
    } else {
        Channels::FRONT_LEFT
    };
    let mut p = CodecParameters::new();
    p.for_codec(CODEC_TYPE_PCM_F32LE)
        .with_sample_rate(MIX_RATE)
        .with_time_base(TimeBase::new(1, MIX_RATE))
        .with_channels(layout)
        .with_sample_format(SampleFormat::F32)
        .with_bits_per_sample(32)
        .with_bits_per_coded_sample(32)
        .with_max_frames_per_packet(FRAMES_PER_TICK_48K as u64);
    p
}

/// The decoder half of a live or memory PCM input.
pub struct PcmDecoder {
    p: Arc<dyn PcmProvider>,
    params: CodecParameters,
    channels: usize,
    /// `rate/50 × ch` input samples.
    in_i16: Box<[i16]>,
    in_f32: Box<[f32]>,
    /// 960 × ch interleaved 48 kHz samples.
    out48: Box<[f32]>,
    resampler: TickResampler,
    buf: AudioBuffer<f32>,
    seen_gen: u64,
    /// Remaining frames of the post-clear fade-in.
    ramp_in: u32,
    gain: f32,
    target: f32,
    ramp_left: u32,
    ramp_step: f32,
    /// Shared with the paired reader: set when a panic was caught.
    poison: Arc<AtomicBool>,
    reported_failure: bool,
}

impl PcmDecoder {
    /// Allocates every buffer (OFF the mixer thread: rubato logs in its constructor).
    pub(crate) fn new(p: Arc<dyn PcmProvider>, poison: Arc<AtomicBool>) -> Result<Self, CoreError> {
        let fmt = p.format();
        let ch = usize::from(fmt.channels);
        if !(1..=2).contains(&ch) {
            return Err(CoreError::invalid("channels must be 1 or 2"));
        }
        let n_in = fmt.frames_per_tick() * ch;
        let n_out = FRAMES_PER_TICK_48K * ch;
        let resampler = TickResampler::new(fmt.rate, SampleRate::Hz48000, ch)?;
        let params = pcm_codec_params(fmt.channels);
        let spec = SignalSpec::new(MIX_RATE, params.channels.unwrap_or(Channels::FRONT_LEFT));
        let volume = p.volume();
        let seen_gen = p.generation();
        Ok(PcmDecoder {
            p,
            params,
            channels: ch,
            in_i16: vec![0i16; n_in].into_boxed_slice(),
            in_f32: vec![0.0f32; n_in].into_boxed_slice(),
            out48: vec![0.0f32; n_out].into_boxed_slice(),
            resampler,
            buf: AudioBuffer::new(FRAMES_PER_TICK_48K as u64, spec),
            seen_gen,
            ramp_in: 0,
            gain: volume,
            target: volume,
            ramp_left: 0,
            ramp_step: 0.0,
            poison,
            reported_failure: false,
        })
    }

    pub(crate) fn params(&self) -> &CodecParameters {
        &self.params
    }

    /// One tick: pull, resample, gain, fill `buf`. No allocation.
    fn decode_inner(&mut self) {
        let p = &*self.p;
        let generation = p.generation();
        if generation != self.seen_gen {
            // clear() / seek: drop the resampler's pre-clear tail and fade in (declick).
            self.seen_gen = generation;
            self.resampler.reset();
            self.ramp_in = DECLICK_FRAMES;
        }
        self.in_i16.fill(0);
        let _ = p.pull(&mut self.in_i16);
        i16_to_f32(&self.in_i16, &mut self.in_f32);
        self.resampler.process_tick(&self.in_f32, &mut self.out48);
        let target = p.volume();
        if target != self.target {
            self.target = target;
            self.ramp_left = VOLUME_RAMP_FRAMES;
            self.ramp_step = (target - self.gain) / VOLUME_RAMP_FRAMES as f32;
        }
        let ch = self.channels;
        for frame in self.out48.chunks_exact_mut(ch) {
            if self.ramp_left > 0 {
                self.ramp_left -= 1;
                self.gain = if self.ramp_left == 0 {
                    self.target
                } else {
                    self.gain + self.ramp_step
                };
            }
            let mut g = self.gain;
            if self.ramp_in > 0 {
                let k = DECLICK_FRAMES - self.ramp_in;
                g *= (k + 1) as f32 / DECLICK_FRAMES as f32;
                self.ramp_in -= 1;
            }
            for s in frame {
                *s *= g;
            }
        }
        self.write_buf();
    }

    /// Copies `out48` (interleaved) into the planar `buf` (exactly 960 frames).
    fn write_buf(&mut self) {
        self.buf.clear();
        self.buf.render_reserved(Some(FRAMES_PER_TICK_48K));
        if self.channels == 2 {
            let (l, r) = self.buf.chan_pair_mut(0, 1);
            let (frames, _) = self.out48.as_chunks::<2>();
            for ((dl, dr), [a, b]) in l.iter_mut().zip(r.iter_mut()).zip(frames) {
                *dl = *a;
                *dr = *b;
            }
        } else {
            let plane = self.buf.chan_mut(0);
            for (d, s) in plane.iter_mut().zip(self.out48.iter()) {
                *d = *s;
            }
        }
    }

    fn silence(&mut self) {
        self.out48.fill(0.0);
        self.write_buf();
    }
}

impl Decoder for PcmDecoder {
    fn try_new(_params: &CodecParameters, _options: &DecoderOptions) -> SymphResult<Self>
    where
        Self: Sized,
    {
        Err(SymphError::Unsupported(
            "PcmDecoder is constructed by sbcore, not the codec registry",
        ))
    }

    fn supported_codecs() -> &'static [CodecDescriptor]
    where
        Self: Sized,
    {
        &[]
    }

    /// songbird calls this after a seek or a passthrough switch: restart the resampler.
    fn reset(&mut self) {
        self.resampler.reset();
        self.ramp_in = DECLICK_FRAMES;
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.params
    }

    fn decode(&mut self, _packet: &Packet) -> SymphResult<AudioBufferRef<'_>> {
        enter_mixer();
        if self.p.failed() && !self.reported_failure {
            self.reported_failure = true;
            return Err(SymphError::DecodeError("decode_error"));
        }
        if self.poison.load(Ordering::Acquire) {
            self.silence();
        } else if catch_unwind(AssertUnwindSafe(|| self.decode_inner())).is_err() {
            // §5.7: silence once, then the reader returns EOF.
            self.poison.store(true, Ordering::Release);
            self.p.on_poison();
            self.silence();
        }
        Ok(self.buf.as_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> AudioBufferRef<'_> {
        self.buf.as_audio_buffer_ref()
    }
}

impl Drop for PcmDecoder {
    fn drop(&mut self) {
        self.p.on_reader_drop();
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests;
