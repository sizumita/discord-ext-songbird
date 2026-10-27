//! Per-tick resampling between the four stream rates (§7.4, §8.7).
//!
//! rubato's `SincFixedIn<f32>` with `Nearest` interpolation, `oversampling_factor = L` and
//! `sinc_len = 64·M` (for `to/from = L/M` in lowest terms), `f_cutoff = 0.95`, `BlackmanHarris2`,
//! one tick per call. The output passes through a FIFO primed once at construction, so every call
//! yields **exactly** `to/50` frames with a constant latency and no drift.

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

use crate::error::CoreError;
use crate::format::SampleRate;

/// Gives `&mut` access only, which makes a `Send` value safely `Sync` (like `std::sync::Exclusive`).
/// rubato's interpolators are `Send` but not `Sync`; symphonia's `Decoder` requires `Sync`.
pub(crate) struct Exclusive<T>(T);

// SAFETY: no `&self` method exposes the inner value, so shared references cannot be used to reach
// the non-`Sync` interior from several threads.
unsafe impl<T: Send> Sync for Exclusive<T> {}

impl<T> Exclusive<T> {
    pub(crate) fn new(t: T) -> Self {
        Exclusive(t)
    }

    pub(crate) fn get_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

pub(crate) const fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Whether rubato's position accumulator stays exact for this ratio (`from/to` is a short binary
/// fraction), in which case the per-call output count never jitters.
pub(crate) fn step_is_exact(from: u32, to: u32) -> bool {
    let ratio = f64::from(to) / f64::from(from);
    let step = 1.0 / ratio;
    (step * 1024.0).fract() == 0.0 && (step * f64::from(to) - f64::from(from)).abs() == 0.0
}

struct Engine {
    rs: Exclusive<SincFixedIn<f32>>,
    in_planar: Vec<Vec<f32>>,
    out_planar: Vec<Vec<f32>>,
    fifo: Vec<Vec<f32>>,
    prime: usize,
    underruns: u64,
}

/// Resamples one 20 ms tick per call between two stream rates. Construct OFF the mixer thread
/// (rubato logs in its constructor); `process_tick` never allocates or logs.
pub struct TickResampler {
    engine: Option<Engine>,
    channels: usize,
    in_frames: usize,
    out_frames: usize,
    delay: usize,
}

impl std::fmt::Debug for TickResampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TickResampler")
            .field("channels", &self.channels)
            .field("in_frames", &self.in_frames)
            .field("out_frames", &self.out_frames)
            .field("delay", &self.delay)
            .finish()
    }
}

impl TickResampler {
    /// `from == to` ⇒ passthrough. `channels` must be 1 or 2.
    pub fn new(from: SampleRate, to: SampleRate, channels: usize) -> Result<Self, CoreError> {
        if !(1..=2).contains(&channels) {
            return Err(CoreError::invalid("TickResampler supports 1 or 2 channels"));
        }
        let (in_frames, out_frames) = (from.frames_per_tick(), to.frames_per_tick());
        if from == to {
            return Ok(TickResampler {
                engine: None,
                channels,
                in_frames,
                out_frames,
                delay: 0,
            });
        }
        let g = gcd(u64::from(from.hz()), u64::from(to.hz()));
        let (l, m) = (u64::from(to.hz()) / g, u64::from(from.hz()) / g);
        let params = SincInterpolationParameters {
            sinc_len: 64 * m as usize,
            f_cutoff: 0.95,
            oversampling_factor: l as usize,
            interpolation: SincInterpolationType::Nearest,
            window: WindowFunction::BlackmanHarris2,
        };
        let ratio = f64::from(to.hz()) / f64::from(from.hz());
        let mut rs = SincFixedIn::<f32>::new(ratio, 1.0, params, in_frames, channels)
            .map_err(|_| CoreError::Internal("TickResampler::new"))?;
        let in_planar = vec![vec![0.0f32; in_frames]; channels];
        let mut out_planar = vec![vec![0.0f32; rs.output_frames_max()]; channels];
        // Measure the first call's output count, then return to the initial state.
        let (_, first) = rs
            .process_into_buffer(&in_planar, &mut out_planar, None)
            .map_err(|_| CoreError::Internal("TickResampler::new"))?;
        rs.reset();
        let slack = usize::from(!step_is_exact(from.hz(), to.hz()));
        let prime = out_frames.saturating_sub(first) + slack;
        let cap = out_frames + prime + 8;
        let fifo = (0..channels)
            .map(|_| {
                let mut v = Vec::with_capacity(cap);
                v.resize(prime, 0.0f32);
                v
            })
            .collect();
        // With `Nearest`, rubato output j interpolates input time (j + 1)·step − 1 + 1/L (input
        // frames; the sinc table for sub-index s peaks at tap sinc_len/2 − 1 + (1 + s)/L). Output
        // frame k of ours is rubato output k − prime, so input frame 0 appears at
        // k = prime − 1 + (1 − 1/L)·(L/M) = prime + (L − 1)/M − 1.
        let lead = (l as f64 - 1.0) / m as f64 - 1.0;
        let delay = (prime as f64 + lead).round().max(0.0) as usize;
        Ok(TickResampler {
            engine: Some(Engine {
                rs: Exclusive::new(rs),
                in_planar,
                out_planar,
                fifo,
                prime,
                underruns: 0,
            }),
            channels,
            in_frames,
            out_frames,
            delay,
        })
    }

    /// `input` = `from.frames_per_tick() · ch`, `output` = `to.frames_per_tick() · ch`, interleaved.
    /// Missing input is treated as silence; extra output is zero-filled. No allocation, no logging.
    pub fn process_tick(&mut self, input: &[f32], output: &mut [f32]) {
        let ch = self.channels;
        let Some(e) = self.engine.as_mut() else {
            let n = input.len().min(output.len());
            if let (Some(dst), Some(src)) = (output.get_mut(..n), input.get(..n)) {
                dst.copy_from_slice(src);
            }
            if let Some(rest) = output.get_mut(n..) {
                rest.fill(0.0);
            }
            return;
        };
        for plane in &mut e.in_planar {
            plane.fill(0.0);
        }
        for (f, frame) in input.chunks_exact(ch).take(self.in_frames).enumerate() {
            for (plane, s) in e.in_planar.iter_mut().zip(frame) {
                if let Some(d) = plane.get_mut(f) {
                    *d = *s;
                }
            }
        }
        let produced = match e.rs.get_mut().process_into_buffer(
            &e.in_planar,
            e.out_planar.as_mut_slice(),
            None,
        ) {
            Ok((_, n)) => n,
            Err(_) => 0,
        };
        for (fifo, out) in e.fifo.iter_mut().zip(&e.out_planar) {
            let room = fifo.capacity() - fifo.len();
            if let Some(src) = out.get(..produced.min(room)) {
                fifo.extend_from_slice(src); // within capacity: never reallocates
            }
        }
        let avail = e.fifo.first().map_or(0, Vec::len);
        let pop = avail.min(self.out_frames);
        if pop < self.out_frames {
            e.underruns += 1;
        }
        output.fill(0.0);
        for (c, fifo) in e.fifo.iter_mut().enumerate() {
            for (f, s) in fifo.iter().take(pop).enumerate() {
                if let Some(d) = output.get_mut(f * ch + c) {
                    *d = *s;
                }
            }
            fifo.drain(..pop);
        }
    }

    /// Returns to the freshly constructed state (no allocation).
    pub fn reset(&mut self) {
        if let Some(e) = self.engine.as_mut() {
            e.rs.get_mut().reset();
            for fifo in &mut e.fifo {
                fifo.clear();
                fifo.resize(e.prime, 0.0);
            }
        }
    }

    /// Latency added by this resampler, in output frames (0 for passthrough).
    pub fn delay_frames(&self) -> usize {
        self.delay
    }

    pub fn is_passthrough(&self) -> bool {
        self.engine.is_none()
    }

    /// Input samples expected per call (`from/50 · channels`).
    pub fn input_len(&self) -> usize {
        self.in_frames * self.channels
    }

    /// Output samples written per call (`to/50 · channels`).
    pub fn output_len(&self) -> usize {
        self.out_frames * self.channels
    }

    /// Calls that could not fill a whole tick from the FIFO (should stay 0).
    pub fn underruns(&self) -> u64 {
        self.engine.as_ref().map_or(0, |e| e.underruns)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::f64::consts::PI;

    pub(crate) fn tone(
        rate: u32,
        hz: f64,
        amp: f64,
        start: usize,
        n: usize,
        ch: usize,
    ) -> Vec<f32> {
        (start..start + n)
            .flat_map(|i| {
                let x = (amp * (2.0 * PI * hz * i as f64 / f64::from(rate)).sin()) as f32;
                std::iter::repeat_n(x, ch)
            })
            .collect()
    }

    /// Amplitude of the `hz` component of `x` (exact when `x` holds whole cycles).
    pub(crate) fn amplitude(x: &[f32], rate: u32, hz: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, s) in x.iter().enumerate() {
            let ph = 2.0 * PI * hz * i as f64 / f64::from(rate);
            re += f64::from(*s) * ph.cos();
            im += f64::from(*s) * ph.sin();
        }
        2.0 * (re * re + im * im).sqrt() / x.len() as f64
    }

    fn run(
        rs: &mut TickResampler,
        from: SampleRate,
        to: SampleRate,
        input: &[f32],
        ch: usize,
    ) -> Vec<f32> {
        let (fi, fo) = (from.frames_per_tick() * ch, to.frames_per_tick() * ch);
        let mut out = Vec::new();
        let mut buf = vec![0.0f32; fo];
        for t in input.chunks(fi) {
            rs.process_tick(t, &mut buf);
            out.extend_from_slice(&buf);
        }
        out
    }

    fn pairs() -> Vec<(SampleRate, SampleRate)> {
        let mut v = Vec::new();
        for a in SampleRate::ALL {
            for b in SampleRate::ALL {
                v.push((a, b));
            }
        }
        v
    }

    #[test]
    fn exact_frames_per_call_no_drift() {
        for (from, to) in pairs() {
            for ch in [1usize, 2] {
                let mut rs = TickResampler::new(from, to, ch).unwrap();
                assert_eq!(rs.input_len(), from.frames_per_tick() * ch);
                assert_eq!(rs.output_len(), to.frames_per_tick() * ch);
                let input = vec![0.25f32; rs.input_len()];
                let mut out = vec![0.0f32; rs.output_len()];
                let ticks = if ch == 1 { 180_000 } else { 2_000 };
                for _ in 0..ticks {
                    rs.process_tick(&input, &mut out);
                }
                assert_eq!(rs.underruns(), 0, "{from:?}→{to:?} ch{ch}: FIFO underran");
                // A DC input converges to the same DC level (no dropped or doubled frames).
                assert!(
                    out.iter().all(|s| (s - 0.25).abs() < 1e-4),
                    "{from:?}→{to:?} ch{ch}"
                );
                if let Some(e) = rs.engine.as_ref() {
                    let level = e.fifo[0].len();
                    assert!(
                        level <= e.prime + 1,
                        "{from:?}→{to:?}: FIFO grew to {level}"
                    );
                }
            }
        }
    }

    #[test]
    fn delay_is_small_and_matches_the_impulse_response() {
        for (from, to) in pairs() {
            let mut rs = TickResampler::new(from, to, 1).unwrap();
            let delay_ms = rs.delay_frames() as f64 * 1_000.0 / f64::from(to.hz());
            // 8 kHz → 48 kHz needs 4.35 ms: rubato's SincFixedIn keeps sinc_len/2 + 1 + ⌈step⌉ input
            // frames of look-ahead (recorded in docs/deviations/w1.md); every other pair is ≤ 4.3 ms.
            let bound = if (from, to) == (SampleRate::Hz8000, SampleRate::Hz48000) {
                4.36
            } else {
                4.3
            };
            assert!(delay_ms <= bound, "{from:?}→{to:?}: {delay_ms:.3} ms");
            // Impulse at input frame 3 of tick 2 → peak at its time in the output + delay.
            let fi = from.frames_per_tick();
            let mut input = vec![0.0f32; fi * 6];
            let at = 2 * fi + 3;
            input[at] = 1.0;
            let out = run(&mut rs, from, to, &input, 1);
            let (peak, _) = out
                .iter()
                .enumerate()
                .fold((0, 0.0f32), |(bi, bv), (i, v)| {
                    if v.abs() > bv { (i, v.abs()) } else { (bi, bv) }
                });
            let expected =
                at as f64 * f64::from(to.hz()) / f64::from(from.hz()) + rs.delay_frames() as f64;
            assert!(
                (peak as f64 - expected).abs() <= 1.0,
                "{from:?}→{to:?}: peak {peak}, expected {expected}"
            );
        }
    }

    #[test]
    fn passband_within_0_2_db() {
        for (from, to) in pairs() {
            let mut rs = TickResampler::new(from, to, 1).unwrap();
            let input = tone(from.hz(), 1_000.0, 0.5, 0, from.hz() as usize * 2, 1);
            let out = run(&mut rs, from, to, &input, 1);
            let tail = &out[to.hz() as usize..];
            let a = amplitude(tail, to.hz(), 1_000.0);
            let db = 20.0 * (a / 0.5).log10();
            assert!(db.abs() <= 0.2, "{from:?}→{to:?}: {db:.4} dB");
        }
    }

    #[test]
    fn aliases_and_images_below_minus_120_db() {
        for (from, to) in pairs() {
            if from == to {
                continue;
            }
            let (fhz, thz) = (f64::from(from.hz()), f64::from(to.hz()));
            let mut rs = TickResampler::new(from, to, 1).unwrap();
            // Downsampling: a tone in the stopband (0.75 of the input Nyquist) must not alias.
            // Upsampling: a 1 kHz tone must not create images at k·fs_in ± 1 kHz.
            let (tone_hz, probes): (f64, Vec<f64>) = if to.hz() < from.hz() {
                let f = (0.75 * fhz / 2.0).round();
                let alias = (f - thz * (f / thz).round()).abs();
                (f, vec![alias])
            } else {
                let mut p = Vec::new();
                let mut k = 1.0;
                while k * fhz - 1_000.0 < thz / 2.0 {
                    p.push(k * fhz - 1_000.0);
                    if k * fhz + 1_000.0 < thz / 2.0 {
                        p.push(k * fhz + 1_000.0);
                    }
                    k += 1.0;
                }
                (1_000.0, p)
            };
            let input = tone(from.hz(), tone_hz, 0.9, 0, from.hz() as usize * 2, 1);
            let out = run(&mut rs, from, to, &input, 1);
            let tail = &out[to.hz() as usize..];
            for p in probes {
                let a = amplitude(tail, to.hz(), p);
                let db = 20.0 * (a / 0.9).log10();
                assert!(db <= -120.0, "{from:?}→{to:?}: {p} Hz at {db:.1} dB");
            }
        }
    }

    #[test]
    fn reset_reproduces_a_fresh_instance() {
        for (from, to) in pairs() {
            for ch in [1usize, 2] {
                let input = tone(from.hz(), 440.0, 0.5, 0, from.frames_per_tick() * 7, ch);
                let mut a = TickResampler::new(from, to, ch).unwrap();
                let first = run(&mut a, from, to, &input, ch);
                a.reset();
                let second = run(&mut a, from, to, &input, ch);
                let mut fresh = TickResampler::new(from, to, ch).unwrap();
                let third = run(&mut fresh, from, to, &input, ch);
                assert_eq!(first, second, "{from:?}→{to:?}");
                assert_eq!(first, third, "{from:?}→{to:?}");
            }
        }
    }

    #[test]
    fn short_or_long_buffers_never_panic() {
        let mut rs = TickResampler::new(SampleRate::Hz24000, SampleRate::Hz48000, 2).unwrap();
        let mut out = vec![1.0f32; 10];
        rs.process_tick(&[0.5; 3], &mut out);
        let mut out = vec![1.0f32; 5_000];
        rs.process_tick(&[0.5; 5_000], &mut out);
        assert!(out[1_920..].iter().all(|s| *s == 0.0));
        assert!(TickResampler::new(SampleRate::Hz8000, SampleRate::Hz16000, 3).is_err());
        let mut pt = TickResampler::new(SampleRate::Hz16000, SampleRate::Hz16000, 1).unwrap();
        assert!(pt.is_passthrough());
        let mut o = vec![9.0f32; 4];
        pt.process_tick(&[1.0, 2.0], &mut o);
        assert_eq!(o, vec![1.0, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn zero_allocations_after_construction() {
        for (from, to) in pairs() {
            for ch in [1usize, 2] {
                let mut rs = TickResampler::new(from, to, ch).unwrap();
                let input = tone(from.hz(), 440.0, 0.5, 0, from.frames_per_tick(), ch);
                let mut out = vec![0.0f32; rs.output_len()];
                let guard = crate::alloc_count::CountGuard::new();
                let before = crate::alloc_count::count();
                for _ in 0..200 {
                    rs.process_tick(&input, &mut out);
                }
                rs.reset();
                rs.process_tick(&input, &mut out);
                let allocs = crate::alloc_count::count() - before;
                drop(guard);
                assert_eq!(allocs, 0, "{from:?}→{to:?} ch{ch}");
            }
        }
    }

    #[test]
    fn is_sync_and_send() {
        fn assert_sync<T: Send + Sync>() {}
        assert_sync::<TickResampler>();
    }
}
