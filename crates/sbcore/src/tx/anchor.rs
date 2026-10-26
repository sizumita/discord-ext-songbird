//! Wall-clock gap filling from provider timestamps (W4, §7.6). Checked arithmetic only: no provider
//! value can overflow the anchor arithmetic or panic `Instant` arithmetic.

use std::time::{Duration, Instant};

/// `|Δ|` beyond this many milliseconds between two timed writes is a discontinuity (one day).
pub(crate) const MAX_DELTA_MS: u64 = 86_400_000;

/// Jitter tolerance: gaps within ±40 ms (2 ticks) append without silence.
pub(crate) const JITTER_US: i128 = 40_000;

/// The gap-fill anchor: provider time `remote_ms` corresponds to wall time `wall`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Anchor {
    pub(crate) remote_ms: i64,
    pub(crate) wall: Instant,
}

/// `clear(reject_until_gap_ms=N)` window (§7.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GapReject {
    pub(crate) cut_end_ms: i64,
    pub(crate) gap_ms: u32,
}

impl GapReject {
    /// Whether a timed write starting at `at_ms` continues the interrupted utterance.
    pub(crate) fn rejects(&self, at_ms: i64) -> bool {
        at_ms < self.cut_end_ms.saturating_add(i64::from(self.gap_ms))
    }
}

/// What a timed write does to the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GapDecision {
    /// Append without silence; the anchor is kept (jitter tolerance).
    Append,
    /// Insert this many input-rate frames of silence first; the anchor is kept.
    Silence { frames: u64 },
    /// Re-anchor at `(at_ms, earliest)` and append without silence: the first timed write, a
    /// discontinuity (beyond `max_gap_ms`, |Δ| over a day, arithmetic overflow) or a late write.
    Reanchor,
}

/// `earliest(now) = now + queued_frames / rate`: when the next written sample would start playing.
pub(crate) fn earliest(now: Instant, queued_frames: u64, rate_hz: u32) -> Instant {
    let us = u128::from(queued_frames) * 1_000_000 / u128::from(rate_hz.max(1));
    let d = Duration::from_micros(u64::try_from(us).unwrap_or(u64::MAX));
    now.checked_add(d).unwrap_or(now)
}

/// Signed `a − b` in microseconds.
pub(crate) fn signed_us(a: Instant, b: Instant) -> i128 {
    if a >= b {
        (a - b).as_micros() as i128
    } else {
        -((b - a).as_micros() as i128)
    }
}

/// Decides the gap for a write timed `at_ms` whose first sample would otherwise start at
/// `earliest` (§7.6 table). The caller turns a `Silence` whose gap plus the write would not fit
/// the buffer into a `Reanchor`.
pub(crate) fn decide(
    anchor: Option<Anchor>,
    at_ms: i64,
    earliest: Instant,
    rate_hz: u32,
    max_gap_ms: u32,
) -> GapDecision {
    let Some(a) = anchor else {
        return GapDecision::Reanchor;
    };
    let Some(delta) = at_ms.checked_sub(a.remote_ms) else {
        return GapDecision::Reanchor;
    };
    if delta.unsigned_abs() > MAX_DELTA_MS {
        return GapDecision::Reanchor;
    }
    let step = Duration::from_millis(delta.unsigned_abs());
    let desired = if delta >= 0 {
        a.wall.checked_add(step)
    } else {
        a.wall.checked_sub(step)
    };
    let Some(desired) = desired else {
        return GapDecision::Reanchor;
    };
    let gap_us = signed_us(desired, earliest);
    if gap_us > i128::from(max_gap_ms) * 1_000 {
        GapDecision::Reanchor
    } else if gap_us > JITTER_US {
        let frames = gap_us * i128::from(rate_hz) / 1_000_000;
        GapDecision::Silence {
            frames: u64::try_from(frames).unwrap_or(0),
        }
    } else if gap_us >= -JITTER_US {
        GapDecision::Append
    } else {
        // Late or overlapping: append immediately (never trim speech) and re-anchor.
        GapDecision::Reanchor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(t0: Instant) -> Option<Anchor> {
        Some(Anchor {
            remote_ms: 0,
            wall: t0,
        })
    }

    #[test]
    fn first_timed_write_anchors() {
        let t0 = Instant::now();
        assert_eq!(decide(None, 500, t0, 24_000, 2_000), GapDecision::Reanchor);
    }

    #[test]
    fn table_rows() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        // On time (within ±40 ms).
        assert_eq!(
            decide(anchor(t0), 1_000, t0 + ms(1_030), 24_000, 2_000),
            GapDecision::Append
        );
        assert_eq!(
            decide(anchor(t0), 1_000, t0 + ms(960), 24_000, 2_000),
            GapDecision::Append
        );
        // Early by 400 ms: 400 ms of silence at 24 kHz.
        assert_eq!(
            decide(anchor(t0), 1_000, t0 + ms(600), 24_000, 2_000),
            GapDecision::Silence { frames: 9_600 }
        );
        // Beyond max_gap: discontinuity.
        assert_eq!(
            decide(anchor(t0), 5_000, t0 + ms(600), 24_000, 2_000),
            GapDecision::Reanchor
        );
        // Late: append + re-anchor.
        assert_eq!(
            decide(anchor(t0), 1_000, t0 + ms(1_500), 24_000, 2_000),
            GapDecision::Reanchor
        );
        // Negative delta (provider rewound) that is still in the future of `earliest`.
        let a = Some(Anchor {
            remote_ms: 10_000,
            wall: t0 + ms(10_000),
        });
        assert_eq!(
            decide(a, 9_500, t0 + ms(9_000), 48_000, 2_000),
            GapDecision::Silence { frames: 24_000 }
        );
    }

    #[test]
    fn extreme_timestamps_never_panic() {
        let t0 = Instant::now();
        for at in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
            for remote in [i64::MIN, 0, i64::MAX] {
                let a = Some(Anchor {
                    remote_ms: remote,
                    wall: t0,
                });
                let _ = decide(a, at, t0, 48_000, 10_000);
                let _ = decide(a, at, t0 + Duration::from_secs(3_600), 8_000, 0);
            }
        }
        assert_eq!(
            decide(anchor(t0), i64::MAX, t0, 24_000, 10_000),
            GapDecision::Reanchor
        );
        assert_eq!(
            decide(anchor(t0), i64::MIN, t0, 24_000, 10_000),
            GapDecision::Reanchor
        );
        let g = GapReject {
            cut_end_ms: i64::MAX,
            gap_ms: 10_000,
        };
        assert!(g.rejects(i64::MAX - 1));
    }

    #[test]
    fn earliest_adds_queued_time() {
        let t0 = Instant::now();
        assert_eq!(earliest(t0, 24_000, 24_000), t0 + Duration::from_secs(1));
        assert_eq!(earliest(t0, 0, 8_000), t0);
        assert_eq!(signed_us(t0, t0 + Duration::from_millis(3)), -3_000);
    }
}
