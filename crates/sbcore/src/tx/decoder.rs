//! `PcmDecoder`: the songbird `Decoder` of every live/memory PCM input (W4, §7.1, §7.4).
//!
//! `decode` always yields exactly 960 frames of 48 kHz `f32` in a preallocated `AudioBuffer`: it
//! `try_lock`s the provider, resamples with a preallocated `TickResampler`, applies gain, never
//! allocates, never returns `Err` and never returns 0 frames.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::sync::Arc;

use songbird::input::core::audio::{AsAudioBufferRef, AudioBuffer, AudioBufferRef, Signal};
use songbird::input::core::codecs::{
    CodecDescriptor, CodecParameters, Decoder, DecoderOptions, FinalizeResult,
};
use songbird::input::core::errors::{Error as SymphError, Result as SymphResult};
use songbird::input::core::formats::Packet;

use super::PcmProvider;
use crate::dsp::TickResampler;
use crate::limits::FRAMES_PER_TICK_48K;

/// The decoder half of a live or memory PCM input.
pub struct PcmDecoder {
    p: Arc<dyn PcmProvider>,
    params: CodecParameters,
    /// `rate/50 × ch` input samples.
    in_i16: Box<[i16]>,
    in_f32: Box<[f32]>,
    /// 960 × ch interleaved 48 kHz samples.
    out48: Box<[f32]>,
    resampler: TickResampler,
    buf: AudioBuffer<f32>,
    seen_gen: u64,
    /// Remaining frames of the post-clear fade-in (2.5 ms).
    ramp_in: u32,
    gain: f32,
}

impl Decoder for PcmDecoder {
    fn try_new(params: &CodecParameters, options: &DecoderOptions) -> SymphResult<Self>
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

    fn reset(&mut self) {
        self.resampler.reset();
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.params
    }

    /// phase 0: always one tick of silence.
    fn decode(&mut self, packet: &Packet) -> SymphResult<AudioBufferRef<'_>> {
        self.buf.clear();
        self.buf.render_reserved(Some(FRAMES_PER_TICK_48K));
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
