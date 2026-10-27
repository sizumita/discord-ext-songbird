//! Hard caps and validated ranges (§10, I6).
//!
//! Every user-supplied duration, count or size is checked here **before any arithmetic**
//! ([`check`], then [`mul_checked`]). Violations surface as `LimitExceeded` (a `ValueError`).

use std::fmt;
use std::ops::RangeInclusive;
use std::time::Duration;

pub const TICK_MS: u32 = 20;
pub const MIX_RATE: u32 = 48_000;
pub const FRAMES_PER_TICK_48K: usize = 960;
pub const MAX_WAKE_IDS: usize = 256;
pub const MAX_SUBS_PER_CONN: usize = 64;
pub const MAX_EVENT_STREAMS_PER_CONN: usize = 16;
pub const MAX_LIVE_SOURCES_PER_CONN: usize = 16;
pub const MAX_ENCODED_WORKERS: usize = 16;
pub const MAX_PLAYING_TRACKS_PER_CONN: usize = 32;
pub const MAX_QUEUE_ENTRIES: usize = 10_000;
/// `MAX_LIVE_SOURCES_PER_CONN + MAX_PLAYING_TRACKS_PER_CONN` = 48 (songbird `preallocated_tracks`, §4.8).
pub const PREALLOCATED_TRACKS: u8 = (MAX_LIVE_SOURCES_PER_CONN + MAX_PLAYING_TRACKS_PER_CONN) as u8;
pub const MAX_OPS_PER_CONN: usize = 64;
pub const TICK_CHANNEL_CAP: usize = 50;
pub const RX_CTRL_OVERFLOW_CAP: usize = 64;
pub const ROUTER_QUEUE_CAP: usize = 64;
pub const RX_CARRY_MAX_FRAMES: usize = 5_760;
pub const OPUS_SILENT_RESERVE: usize = 64;
pub const MAX_TAGS: usize = 256;
pub const MAX_TAG_LEN: usize = 128;
pub const MAX_REJECTED_TAGS: usize = 256;
pub const HEARTBEAT_MISS: Duration = Duration::from_secs(3);
pub const LOG_RING_CAP: usize = 1024;
pub const LOG_MSG_MAX: usize = 512;
pub const MAX_SOURCE_BYTES: usize = 1 << 30;
pub const MAX_OPUS_PACKETS: usize = 1_000_000;
pub const CHUNK_MS: RangeInclusive<u32> = 10..=1_000;
pub const MAX_QUEUE_MS_MAX: u32 = 60_000;
pub const PLAYOUT_BUFFER_MS: RangeInclusive<u32> = 20..=200;
pub const PLAYOUT_SPIKE_MS: RangeInclusive<u32> = 0..=200;
pub const PLAYBACK_BUFFER_MS: RangeInclusive<u32> = 100..=120_000;
pub const PREBUFFER_MS: RangeInclusive<u32> = 0..=1_000;
pub const IDLE_MS: RangeInclusive<u32> = 20..=60_000;
pub const MAX_GAP_MS: RangeInclusive<u32> = 0..=10_000;
pub const OUTPUT_LATENCY_MS: RangeInclusive<u32> = 0..=1_000;
// cross-field (PlaybackSpec::validate): prebuffer_ms ≤ max_buffer_ms/2 and max_gap_ms ≤ max_buffer_ms/2
pub const HOLD_MS: RangeInclusive<u32> = 0..=10_000;
pub const REJECT_GAP_MS: RangeInclusive<u32> = 0..=10_000;
pub const SILENCE_MS: RangeInclusive<u64> = 0..=600_000;
pub const SEEK_S: RangeInclusive<f64> = 0.0..=86_400.0;
pub const SOURCE_RATE_HZ: RangeInclusive<u32> = 8_000..=192_000;
pub const EVENT_QUEUE: RangeInclusive<usize> = 1..=4_096;
pub const ENCODED_BUFFER_BYTES: RangeInclusive<usize> = (64 << 10)..=(16 << 20);
pub const OPUS_STREAM_PACKETS: RangeInclusive<usize> = 1..=3_000;
pub const VOLUME: RangeInclusive<f32> = 0.0..=4.0;
/// Worker counts accepted by `configure_runtime` (§5.1).
pub const RUNTIME_WORKERS: RangeInclusive<usize> = 1..=8;

/// A limit value that keeps its kind.
///
/// `u128: From<usize>` and `From<f32>` do not exist (checked with rustc 1.99), so a single
/// `Into<u128>` bound cannot cover usize/f32/f64 ranges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LimitNum {
    Int(i128),
    Float(f64),
}

impl fmt::Display for LimitNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LimitNum::Int(v) => write!(f, "{v}"),
            LimitNum::Float(v) => write!(f, "{v}"),
        }
    }
}

/// A scalar that can be range-checked and reported in a [`LimitError`].
pub trait LimitScalar: Copy + PartialOrd {
    fn num(self) -> LimitNum;
}

macro_rules! impl_limit_int {
    ($($t:ty),*) => {$(
        impl LimitScalar for $t {
            #[inline]
            fn num(self) -> LimitNum {
                LimitNum::Int(i128::from(self))
            }
        }
    )*};
}
impl_limit_int!(u8, u16, u32, u64, i64);

impl LimitScalar for usize {
    #[inline]
    fn num(self) -> LimitNum {
        // usize is at most 64 bits on every supported target.
        LimitNum::Int(i128::try_from(self).unwrap_or(i128::MAX))
    }
}

impl LimitScalar for f32 {
    #[inline]
    fn num(self) -> LimitNum {
        LimitNum::Float(f64::from(self))
    }
}

impl LimitScalar for f64 {
    #[inline]
    fn num(self) -> LimitNum {
        LimitNum::Float(self)
    }
}

/// A value outside its validated range (`LimitExceeded` in Python).
#[derive(Debug, Clone, PartialEq)]
pub struct LimitError {
    pub what: &'static str,
    pub value: LimitNum,
    pub min: LimitNum,
    pub max: LimitNum,
}

impl fmt::Display for LimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} = {} is outside the allowed range [{}, {}]",
            self.what, self.value, self.min, self.max
        )
    }
}

impl std::error::Error for LimitError {}

/// `Err` unless `r.contains(&v)`; NaN is never contained, so NaN is rejected too.
pub fn check<T: LimitScalar>(
    what: &'static str,
    v: T,
    r: RangeInclusive<T>,
) -> Result<T, LimitError> {
    if r.contains(&v) {
        Ok(v)
    } else {
        Err(LimitError {
            what,
            value: v.num(),
            min: r.start().num(),
            max: r.end().num(),
        })
    }
}

/// `a * b`, or a [`LimitError`] (range `0..=u64::MAX`) when the product overflows.
pub fn mul_checked(what: &'static str, a: u64, b: u64) -> Result<u64, LimitError> {
    a.checked_mul(b).ok_or(LimitError {
        what,
        value: LimitNum::Int(i128::from(a).saturating_mul(i128::from(b))),
        min: LimitNum::Int(0),
        max: LimitNum::Int(i128::from(u64::MAX)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preallocated_tracks_is_48() {
        assert_eq!(PREALLOCATED_TRACKS, 48);
    }

    #[test]
    fn u32_boundaries() {
        assert_eq!(check("chunk_ms", 10u32, CHUNK_MS), Ok(10));
        assert_eq!(check("chunk_ms", 1_000u32, CHUNK_MS), Ok(1_000));
        let e = check("chunk_ms", 9u32, CHUNK_MS).unwrap_err();
        assert_eq!(e.what, "chunk_ms");
        assert_eq!(e.value, LimitNum::Int(9));
        assert_eq!(e.min, LimitNum::Int(10));
        assert_eq!(e.max, LimitNum::Int(1_000));
        assert!(check("chunk_ms", 1_001u32, CHUNK_MS).is_err());
        assert!(check("chunk_ms", 0u32, CHUNK_MS).is_err());
    }

    #[test]
    fn u64_boundaries() {
        assert!(check("ms", 0u64, SILENCE_MS).is_ok());
        assert!(check("ms", 600_000u64, SILENCE_MS).is_ok());
        assert!(check("ms", 600_001u64, SILENCE_MS).is_err());
        assert!(check("ms", u64::MAX, SILENCE_MS).is_err());
    }

    #[test]
    fn usize_boundaries() {
        assert!(check("max_queue", 1usize, EVENT_QUEUE).is_ok());
        assert!(check("max_queue", 4_096usize, EVENT_QUEUE).is_ok());
        assert!(check("max_queue", 0usize, EVENT_QUEUE).is_err());
        let e = check("max_queue", usize::MAX, EVENT_QUEUE).unwrap_err();
        assert_eq!(e.value, LimitNum::Int(i128::try_from(usize::MAX).unwrap()));
        assert!(check("bytes", 64usize << 10, ENCODED_BUFFER_BYTES).is_ok());
        assert!(check("bytes", (64usize << 10) - 1, ENCODED_BUFFER_BYTES).is_err());
    }

    #[test]
    fn i64_boundaries() {
        assert!(check("ms", -1i64, 0..=600_000i64).is_err());
        assert!(check("ms", 0i64, 0..=600_000i64).is_ok());
        assert_eq!(
            check("ms", i64::MIN, 0..=1i64).unwrap_err().value,
            LimitNum::Int(i128::from(i64::MIN))
        );
    }

    #[test]
    fn float_boundaries_nan_and_inf() {
        assert!(check("volume", 0.0f32, VOLUME).is_ok());
        assert!(check("volume", 4.0f32, VOLUME).is_ok());
        assert!(check("volume", 4.000_001f32, VOLUME).is_err());
        assert!(check("volume", -0.0f32, VOLUME).is_ok());
        assert!(check("volume", f32::NAN, VOLUME).is_err());
        assert!(check("volume", f32::INFINITY, VOLUME).is_err());
        assert!(check("volume", f32::NEG_INFINITY, VOLUME).is_err());
        let e = check("seconds", f64::NAN, SEEK_S).unwrap_err();
        assert!(matches!(e.value, LimitNum::Float(v) if v.is_nan()));
        assert_eq!(e.max, LimitNum::Float(86_400.0));
        assert!(check("seconds", -1.0f64, SEEK_S).is_err());
        assert!(check("seconds", 1e300f64, SEEK_S).is_err());
        assert!(check("seconds", f64::INFINITY, SEEK_S).is_err());
        assert!(check("seconds", 86_400.0f64, SEEK_S).is_ok());
    }

    #[test]
    fn small_int_kinds() {
        assert!(check("x", 3u8, 1..=10u8).is_ok());
        assert!(check("x", 11u16, 1..=10u16).is_err());
    }

    #[test]
    fn mul_checked_overflow() {
        assert_eq!(mul_checked("bytes", 3, 4), Ok(12));
        let e = mul_checked("bytes", u64::MAX, 2).unwrap_err();
        assert_eq!(e.what, "bytes");
        assert_eq!(e.value, LimitNum::Int(i128::from(u64::MAX) * 2));
        assert_eq!(e.max, LimitNum::Int(i128::from(u64::MAX)));
        // Saturates instead of overflowing i128.
        assert!(mul_checked("bytes", u64::MAX, u64::MAX).is_err());
    }

    #[test]
    fn display_mentions_range() {
        let e = check("chunk_ms", 0u32, CHUNK_MS).unwrap_err();
        assert_eq!(
            e.to_string(),
            "chunk_ms = 0 is outside the allowed range [10, 1000]"
        );
    }
}
