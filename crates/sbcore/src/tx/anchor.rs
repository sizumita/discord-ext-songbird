//! Wall-clock gap filling from provider timestamps (W4, §7.6). Checked arithmetic only: no provider
//! value can overflow the anchor arithmetic or panic `Instant` arithmetic.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::time::Instant;

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

/// What a timed write does to the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GapDecision {
    /// Append without silence (jitter tolerance, late write, untimed write).
    Append,
    /// Insert this many input-rate frames of silence first.
    Silence { frames: u64 },
    /// Discontinuity: re-anchor at (at_ms, earliest) and append without silence.
    Reanchor,
}

/// Decides the gap for a write timed `at_ms` whose first sample would otherwise start at
/// `earliest` (§7.6 table).
pub(crate) fn decide(
    anchor: Option<Anchor>,
    at_ms: i64,
    earliest: Instant,
    rate_hz: u32,
    max_gap_ms: u32,
) -> GapDecision {
    GapDecision::Append
}
