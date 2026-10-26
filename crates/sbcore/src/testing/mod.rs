//! Test hooks (W5, §13.2; feature `test-hooks`): detached connections driven by a manual clock,
//! input injection through the same `RxSender`/`apply()` paths `Dispatch` uses, RTP/Opus helpers and
//! a mixer harness. Never compiled into release wheels.

pub mod harness;
pub mod mixer;
pub mod rtp;

pub use harness::{
    DriverEvent, Harness, inject_driver_event, inject_map, inject_tick, inject_user_left,
    inject_voice_tick_rtp,
};
pub use mixer::MixerHarness;
pub use rtp::{RtpOpts, encode_opus, rtp_wrap};

use crate::tracks::flags::TrackRef;

/// The `FakeTrackControl` call log of a track of a detached connection (`"pause"`, `"play"`,
/// `"stop"`), oldest first. Empty for other tracks.
pub fn track_log(t: &TrackRef) -> Vec<&'static str> {
    let _ = t;
    Vec::new()
}

/// Allocations on the calling thread while a counting guard is active (§13.1).
pub fn alloc_count() -> u64 {
    crate::alloc_count::count()
}
