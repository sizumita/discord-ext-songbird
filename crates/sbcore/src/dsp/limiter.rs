//! Feed-forward peak limiter: instant attack, exponential release, no look-ahead (§8.6).
//!
//! The gain never exceeds `threshold / peak` for the current frame, so the output never exceeds the
//! threshold. Allocation-free.

/// Per-bus limiter state.
#[derive(Clone, Debug)]
pub struct Limiter {
    thr: f32,
    /// Per-frame release coefficient (`exp(-1 / (release_s · rate))`). f64, like `gain`: in f32 the
    /// release would stall about 1e-4 below unity (the per-frame step falls under half an ulp).
    rel: f64,
    gain: f64,
}

impl Limiter {
    /// `threshold_dbfs` is clamped to `-60..=0`, `release_ms` to `1..=10_000`.
    pub fn new(rate: u32, threshold_dbfs: f32, release_ms: f32) -> Self {
        let thr_db = if threshold_dbfs.is_finite() {
            threshold_dbfs.clamp(-60.0, 0.0)
        } else {
            -1.0
        };
        let rel_ms = if release_ms.is_finite() {
            release_ms.clamp(1.0, 10_000.0)
        } else {
            80.0
        };
        let rate = f64::from(rate.max(1));
        Limiter {
            thr: 10f32.powf(thr_db / 20.0),
            rel: (-1.0 / (f64::from(rel_ms) / 1_000.0 * rate)).exp(),
            gain: 1.0,
        }
    }

    /// The current gain (1.0 = unity).
    pub fn gain(&self) -> f32 {
        self.gain as f32
    }

    pub fn reset(&mut self) {
        self.gain = 1.0;
    }

    /// Limits interleaved samples in place (`channels` ≥ 1; a trailing partial frame is left as is).
    pub fn process(&mut self, interleaved: &mut [f32], channels: usize) {
        let ch = channels.max(1);
        for frame in interleaved.chunks_exact_mut(ch) {
            let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let needed = if peak > self.thr {
                f64::from(self.thr / peak)
            } else {
                1.0
            };
            if needed < self.gain {
                self.gain = needed; // instant attack
            } else {
                self.gain = needed - (needed - self.gain) * self.rel; // release towards `needed`
            }
            let g = self.gain as f32;
            for s in frame {
                *s *= g;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amp: f32, n: usize, ch: usize) -> Vec<f32> {
        (0..n)
            .flat_map(|i| {
                let x = amp * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin();
                std::iter::repeat_n(x, ch)
            })
            .collect()
    }

    #[test]
    fn plus_4_dbfs_peaks_at_most_minus_1() {
        for ch in [1usize, 2] {
            let mut l = Limiter::new(48_000, -1.0, 80.0);
            let amp = 10f32.powf(4.0 / 20.0);
            let mut x = sine(amp, 24_000, ch);
            l.process(&mut x, ch);
            let peak = x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let limit = 10f32.powf(-1.0 / 20.0);
            assert!(peak <= limit * (1.0 + 1e-6), "peak {peak} > {limit}");
            assert!(
                peak > limit * 0.99,
                "the limiter should not over-attenuate: {peak}"
            );
        }
    }

    #[test]
    fn unity_gain_again_after_one_second() {
        let mut l = Limiter::new(48_000, -1.0, 80.0);
        let mut loud = sine(2.0, 4_800, 1);
        l.process(&mut loud, 1);
        assert!(l.gain() < 0.5);
        let quiet_in = sine(0.1, 48_000, 1);
        let mut quiet = quiet_in.clone();
        l.process(&mut quiet, 1);
        assert!((l.gain() - 1.0).abs() < 1e-4, "gain {}", l.gain());
        // The last 10 ms pass through unchanged (within 0.01 dB).
        for (o, i) in quiet[47_520..].iter().zip(&quiet_in[47_520..]) {
            assert!((o - i).abs() <= i.abs() * 0.0012 + 1e-7);
        }
    }

    #[test]
    fn below_threshold_is_untouched() {
        let mut l = Limiter::new(48_000, -1.0, 80.0);
        let src = sine(0.5, 960, 2);
        let mut x = src.clone();
        l.process(&mut x, 2);
        assert_eq!(x, src);
    }

    #[test]
    fn non_finite_parameters_fall_back() {
        let l = Limiter::new(0, f32::NAN, f32::INFINITY);
        assert!(l.thr > 0.0 && l.thr <= 1.0);
        assert!(l.rel > 0.0 && l.rel < 1.0);
    }
}
