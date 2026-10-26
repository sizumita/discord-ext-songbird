//! Test hooks (W5, §13.2; feature `test-hooks`): detached connections driven by a manual clock,
//! input injection through the same `RxSender`/`apply()` paths `Dispatch` uses, RTP/Opus helpers and
//! a mixer harness. Never compiled into release wheels.
//!
//! # Detached mode contract (for `ConnShared::new_detached`, W2)
//!
//! A detached connection has no songbird `Driver`, so wherever a Driver-backed connection would call
//! `driver.play(track)` a detached one instead:
//!
//! 1. attaches [`fake_track_control`]`(&flags)` as the track's `TrackControl` (live sources:
//!    `PcmQueue::attach_track`, …), so `pause`/`play`/`stop` are recorded ([`track_log`]) and
//!    `stop()` resolves the flags as a Dispatch `End(Stop)` would;
//! 2. hands the songbird `Input` it built to [`park_input`]`(conn, source_id, flags, input)`
//!    (`source_id` = `LiveSource::id()` for live sources, `None` for memory/file tracks).
//!
//! A `MixerHarness` then takes a playback's parked input and drives it; [`Harness::step`] disposes
//! of the inputs of ended tracks (and of every input once the connection is closed), and dropping
//! the `Harness` disposes of whatever is left — the same `DoneGuard` paths songbird would run.

pub mod harness;
pub mod mixer;
mod registry;
pub mod rtp;

pub use harness::{
    DriverEvent, Harness, inject_driver_event, inject_map, inject_tick, inject_user_left,
    inject_voice_tick_rtp,
};
pub use mixer::MixerHarness;
pub use registry::{MAX_PARKED, fake_track_control, park_input};
pub use rtp::{RtpOpts, encode_opus, rtp_wrap};

use crate::tracks::flags::TrackRef;

/// The `FakeTrackControl` call log of a track of a detached connection (`"pause"`, `"play"`,
/// `"stop"`), oldest first. Empty for other tracks.
pub fn track_log(t: &TrackRef) -> Vec<&'static str> {
    registry::fake_log(&t.flags)
}

/// Allocations on the calling thread while a counting guard is active (§13.1). `MixerHarness::pull`
/// holds the guard around the mixer callbacks only.
pub fn alloc_count() -> u64 {
    crate::alloc_count::count()
}
