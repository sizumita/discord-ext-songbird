//! Arbitrary-ratio streaming resampler (any rate in `SOURCE_RATE_HZ` → a stream rate).
//!
//! Used off the mixer only: by `RawPcm.from_samples` (once, inside `py.detach`) and by encoded-stream
//! decode threads (§7.9). The filter delay is removed, so output frame `j` is aligned with input time
//! `j / ratio`, and [`StreamResampler::flush`] makes the total output length `round(in · ratio)`.

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

use super::resample::{Exclusive, gcd};
use crate::error::CoreError;
use crate::format::SampleRate;
use crate::limits::{SOURCE_RATE_HZ, check};

const CHUNK: usize = 1024;
const MAX_NEAREST_OVERSAMPLING: u64 = 1024;

struct Engine {
    rs: Exclusive<SincFixedIn<f32>>,
    in_planar: Vec<Vec<f32>>,
    out_planar: Vec<Vec<f32>>,
    filled: usize,
    /// Leading rubato output frames to discard (alignment, see [`alignment`]).
    skip: usize,
    /// Zero frames to emit before the first rubato output (alignment).
    prepend: usize,
    align_skip: usize,
    align_prepend: usize,
}

/// rubato's `SincFixedIn` output `j` interpolates input time `(j + 1)·step − 1 + 1/L` (input frames,
/// `L` = oversampling factor, `step` = 1/ratio): the sinc table for sub-index `s` peaks at tap
/// `sinc_len/2 − 1 + (1 + s)/L`, and the first output starts at index `−sinc_len/2 + step`. Output `k`
/// is aligned with input time `k·step` after dropping `(1 − 1/L)·ratio − 1` leading frames (negative:
/// prepend zeros). Returns `(skip, prepend)`.
fn alignment(ratio: f64, oversampling: usize) -> (usize, usize) {
    let s = ((1.0 - 1.0 / oversampling as f64) * ratio - 1.0).round();
    if s >= 0.0 {
        (s as usize, 0)
    } else {
        (0, (-s) as usize)
    }
}

/// Streaming resampler with variable-size input. Not for the mixer thread (it may allocate).
pub struct StreamResampler {
    engine: Option<Engine>,
    channels: usize,
    from_hz: u32,
    to_hz: u32,
    partial: [f32; 2],
    partial_len: usize,
    total_in: u64,
    total_out: u64,
}

impl std::fmt::Debug for StreamResampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamResampler")
            .field("from_hz", &self.from_hz)
            .field("to_hz", &self.to_hz)
            .field("channels", &self.channels)
            .finish()
    }
}

impl StreamResampler {
    /// `from_hz` ∈ `SOURCE_RATE_HZ` (8–192 kHz), `channels` ∈ {1, 2}. Equal rates pass through.
    pub fn new(from_hz: u32, to: SampleRate, channels: usize) -> Result<Self, CoreError> {
        check("sample_rate", from_hz, SOURCE_RATE_HZ)?;
        if !(1..=2).contains(&channels) {
            return Err(CoreError::invalid("channels must be 1 or 2"));
        }
        let to_hz = to.hz();
        let engine = if from_hz == to_hz {
            None
        } else {
            let g = gcd(u64::from(from_hz), u64::from(to_hz));
            let l = u64::from(to_hz) / g;
            let down = u64::from(from_hz).div_ceil(u64::from(to_hz)).max(1) as usize;
            let (oversampling_factor, interpolation) = if l <= MAX_NEAREST_OVERSAMPLING {
                (l as usize, SincInterpolationType::Nearest)
            } else {
                (256, SincInterpolationType::Cubic)
            };
            let params = SincInterpolationParameters {
                sinc_len: 256 * down,
                f_cutoff: 0.95,
                oversampling_factor,
                interpolation,
                window: WindowFunction::BlackmanHarris2,
            };
            let ratio = f64::from(to_hz) / f64::from(from_hz);
            let rs = SincFixedIn::<f32>::new(ratio, 1.0, params, CHUNK, channels)
                .map_err(|_| CoreError::Internal("StreamResampler::new"))?;
            let out_max = rs.output_frames_max();
            let (skip, prepend) = alignment(ratio, oversampling_factor);
            Some(Engine {
                rs: Exclusive::new(rs),
                in_planar: vec![vec![0.0f32; CHUNK]; channels],
                out_planar: vec![vec![0.0f32; out_max]; channels],
                filled: 0,
                skip,
                prepend,
                align_skip: skip,
                align_prepend: prepend,
            })
        };
        Ok(StreamResampler {
            engine,
            channels,
            from_hz,
            to_hz,
            partial: [0.0; 2],
            partial_len: 0,
            total_in: 0,
            total_out: 0,
        })
    }

    /// Output frames that `in_frames` input frames produce in total (after [`flush`](Self::flush)).
    /// Callers `try_reserve` this before feeding large inputs.
    pub fn expected_output_frames(&self, in_frames: u64) -> u64 {
        let num = u128::from(in_frames) * u128::from(self.to_hz) + u128::from(self.from_hz / 2);
        u64::try_from(num / u128::from(self.from_hz)).unwrap_or(u64::MAX)
    }

    /// Feeds interleaved samples (any length; a trailing partial frame is carried) and appends the
    /// resampled interleaved output to `out`.
    pub fn process(&mut self, interleaved: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        let mut rest = interleaved;
        // Complete a carried partial frame first.
        if self.partial_len > 0 {
            while self.partial_len < ch {
                let Some((s, tail)) = rest.split_first() else {
                    return;
                };
                if let Some(p) = self.partial.get_mut(self.partial_len) {
                    *p = *s;
                }
                self.partial_len += 1;
                rest = tail;
            }
            let frame = self.partial;
            self.partial_len = 0;
            self.push_frame(frame.get(..ch).unwrap_or(&[]), out);
        }
        let mut frames = rest.chunks_exact(ch);
        for frame in frames.by_ref() {
            self.push_frame(frame, out);
        }
        for (i, s) in frames.remainder().iter().enumerate() {
            if let Some(p) = self.partial.get_mut(i) {
                *p = *s;
            }
            self.partial_len = i + 1;
        }
    }

    fn push_frame(&mut self, frame: &[f32], out: &mut Vec<f32>) {
        self.total_in += 1;
        let Some(e) = self.engine.as_mut() else {
            out.extend_from_slice(frame);
            self.total_out += 1;
            return;
        };
        for (plane, s) in e.in_planar.iter_mut().zip(frame) {
            if let Some(d) = plane.get_mut(e.filled) {
                *d = *s;
            }
        }
        e.filled += 1;
        if e.filled == CHUNK {
            self.run_chunk(out);
        }
    }

    fn run_chunk(&mut self, out: &mut Vec<f32>) {
        let ch = self.channels;
        let Some(e) = self.engine.as_mut() else {
            return;
        };
        e.filled = 0;
        let produced = match e.rs.get_mut().process_into_buffer(
            &e.in_planar,
            e.out_planar.as_mut_slice(),
            None,
        ) {
            Ok((_, n)) => n,
            Err(_) => 0,
        };
        let skip = e.skip.min(produced);
        e.skip -= skip;
        let prepend = std::mem::take(&mut e.prepend);
        let emit = produced - skip;
        out.reserve((emit + prepend) * ch);
        out.resize(out.len() + prepend * ch, 0.0);
        for f in skip..produced {
            for plane in &e.out_planar {
                out.push(plane.get(f).copied().unwrap_or(0.0));
            }
        }
        self.total_out += (emit + prepend) as u64;
    }

    /// Ends the stream: drains the filter with silence and trims the output so that the total
    /// number of output frames is exactly `round(total_in · to / from)`. The resampler is then
    /// ready for a new stream.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        let ch = self.channels;
        let expected = self.expected_output_frames(self.total_in);
        self.partial_len = 0;
        if self.engine.is_some() {
            // Each zero-padded chunk advances the output by ≈ CHUNK · ratio frames; the filter
            // delay is far below one chunk, so a few iterations always suffice.
            let mut guard = 0;
            while self.total_out < expected && guard < 64 {
                if let Some(e) = self.engine.as_mut() {
                    for plane in &mut e.in_planar {
                        if let Some(tail) = plane.get_mut(e.filled..) {
                            tail.fill(0.0);
                        }
                    }
                }
                self.run_chunk(out);
                guard += 1;
            }
        }
        if self.total_out > expected {
            let extra = usize::try_from(self.total_out - expected).unwrap_or(usize::MAX);
            let keep = out.len().saturating_sub(extra.saturating_mul(ch));
            out.truncate(keep);
            self.total_out = expected;
        }
        // Reset for reuse.
        if let Some(e) = self.engine.as_mut() {
            e.rs.get_mut().reset();
            e.skip = e.align_skip;
            e.prepend = e.align_prepend;
            e.filled = 0;
        }
        self.total_in = 0;
        self.total_out = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::resample::tests::{amplitude, tone};

    fn resample_all(from: u32, to: SampleRate, ch: usize, input: &[f32], piece: usize) -> Vec<f32> {
        let mut rs = StreamResampler::new(from, to, ch).unwrap();
        let mut out = Vec::new();
        for p in input.chunks(piece) {
            rs.process(p, &mut out);
        }
        rs.flush(&mut out);
        out
    }

    #[test]
    fn length_accounting_44_1_to_48() {
        for frames in [0usize, 1, 100, 1023, 1024, 1025, 44_100, 44_100 * 3 + 17] {
            for ch in [1usize, 2] {
                let input = vec![0.1f32; frames * ch];
                for piece in [1usize, 7, 4096] {
                    let out = resample_all(44_100, SampleRate::Hz48000, ch, &input, piece);
                    let expected = (frames as u64 * 48_000 + 22_050) / 44_100;
                    assert_eq!(
                        out.len() as u64,
                        expected * ch as u64,
                        "{frames} frames ch{ch} piece {piece}"
                    );
                }
            }
        }
    }

    #[test]
    fn tone_level_and_alignment() {
        for from in [
            44_100u32, 22_050, 192_000, 11_025, 8_000, 8_001, 96_000, 32_000, 16_000,
        ] {
            let input = tone(from, 1_000.0, 0.5, 0, from as usize * 2, 1);
            let out = resample_all(from, SampleRate::Hz48000, 1, &input, 3_333);
            assert_eq!(
                out.len() as u64,
                (from as u64 * 2 * 48_000 + from as u64 / 2) / from as u64,
                "{from}"
            );
            let mid = &out[24_000..72_000];
            let a = amplitude(mid, 48_000, 1_000.0);
            let db = 20.0 * (a / 0.5).log10();
            assert!(db.abs() <= 0.2, "{from}: {db:.4} dB");
            // Aligned to the nearest output frame: the 1 kHz phase matches a 48 kHz reference tone
            // (input time 0 ↔ output frame 0) within half a frame.
            let phase = |x: &[f32], start: usize| {
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (i, v) in x.iter().enumerate() {
                    let ph = 2.0 * std::f64::consts::PI * 1_000.0 * (start + i) as f64 / 48_000.0;
                    re += f64::from(*v) * ph.cos();
                    im += f64::from(*v) * ph.sin();
                }
                im.atan2(re)
            };
            let dphi =
                phase(mid, 24_000) - phase(&tone(48_000, 1_000.0, 0.5, 24_000, 48_000, 1), 24_000);
            let dphi = (dphi + std::f64::consts::PI).rem_euclid(2.0 * std::f64::consts::PI)
                - std::f64::consts::PI;
            let offset_frames = dphi / (2.0 * std::f64::consts::PI * 1_000.0) * 48_000.0;
            assert!(
                offset_frames.abs() <= 0.5,
                "{from}: misaligned by {offset_frames:.3} frames"
            );
        }
    }

    #[test]
    fn passthrough_and_partial_frames() {
        let mut rs = StreamResampler::new(48_000, SampleRate::Hz48000, 2).unwrap();
        let mut out = Vec::new();
        rs.process(&[1.0, 2.0, 3.0], &mut out);
        assert_eq!(out, vec![1.0, 2.0]);
        rs.process(&[4.0], &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
        rs.flush(&mut out);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn validation() {
        assert!(matches!(
            StreamResampler::new(7_999, SampleRate::Hz48000, 2),
            Err(CoreError::Limit(_))
        ));
        assert!(matches!(
            StreamResampler::new(192_001, SampleRate::Hz48000, 2),
            Err(CoreError::Limit(_))
        ));
        assert!(StreamResampler::new(192_000, SampleRate::Hz48000, 3).is_err());
        let rs = StreamResampler::new(44_100, SampleRate::Hz48000, 2).unwrap();
        assert_eq!(rs.expected_output_frames(44_100), 48_000);
        assert_eq!(rs.expected_output_frames(0), 0);
    }
}
