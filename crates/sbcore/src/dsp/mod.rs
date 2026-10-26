//! DSP building blocks: tick/stream resampling, limiting, G.711, mixing, levels, base64.
//!
//! Everything used on the mixer thread (`TickResampler::process_tick`, `mix::*`, g711 tables) is
//! allocation-free and never logs.

pub mod b64;
pub mod g711;
pub mod level;
pub mod limiter;
pub mod mix;
pub mod resample;
pub mod stream_resample;

pub use level::{Level, measure};
pub use limiter::Limiter;
pub use resample::TickResampler;
pub use stream_resample::StreamResampler;

use crate::format::{Encoding, Format};

/// Encodes f32 samples (±1.0 full scale) into `fmt`'s wire encoding.
/// `dst.len() == src.len() * fmt.sample_width()`; extra `dst` bytes are left untouched.
/// pcm16le rounds and clamps; f32le copies; µ-law/A-law go through i16 and the g711 encoders.
pub fn encode_from_f32(fmt: Format, src: &[f32], dst: &mut [u8]) {
    match fmt.encoding {
        Encoding::Pcm16Le => {
            let (out, _) = dst.as_chunks_mut::<2>();
            for (o, s) in out.iter_mut().zip(src) {
                *o = mix::f32_sample_to_i16(*s).to_le_bytes();
            }
        }
        Encoding::F32Le => {
            let (out, _) = dst.as_chunks_mut::<4>();
            for (o, s) in out.iter_mut().zip(src) {
                *o = s.to_le_bytes();
            }
        }
        Encoding::MuLaw => {
            for (o, s) in dst.iter_mut().zip(src) {
                *o = g711::ulaw_encode(mix::f32_sample_to_i16(*s));
            }
        }
        Encoding::ALaw => {
            for (o, s) in dst.iter_mut().zip(src) {
                *o = g711::alaw_encode(mix::f32_sample_to_i16(*s));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{ALAW_8K, F32_48K, MULAW_8K, PCM16_24K, PcmParser};

    #[test]
    fn encode_from_f32_round_trips_through_the_parser() {
        let src: Vec<f32> = (0..200).map(|i| ((i as f32) * 0.37).sin() * 0.9).collect();
        for fmt in [PCM16_24K, F32_48K, MULAW_8K, ALAW_8K] {
            let mut bytes = vec![0u8; src.len() * fmt.sample_width()];
            encode_from_f32(fmt, &src, &mut bytes);
            let mut back = Vec::new();
            PcmParser::new(fmt).parse_frames(&bytes, &mut back).unwrap();
            assert_eq!(back.len(), src.len());
            let tol = if fmt.encoding.is_g711() { 1_100 } else { 1 };
            for (b, s) in back.iter().zip(&src) {
                let exp = mix::f32_sample_to_i16(*s);
                assert!(
                    (i32::from(*b) - i32::from(exp)).abs() <= tol,
                    "{fmt:?}: {b} vs {exp}"
                );
            }
        }
    }

    #[test]
    fn encode_silence_bytes() {
        let mut b = [0u8; 4];
        encode_from_f32(MULAW_8K, &[0.0; 4], &mut b);
        assert_eq!(b, [0xFF; 4]);
        encode_from_f32(ALAW_8K, &[0.0; 4], &mut b);
        assert_eq!(b, [0xD5; 4]);
    }
}
