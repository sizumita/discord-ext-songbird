//! Send path (W4, §7, §12.5).
//!
//! * [`queue`] (+ [`writer`], [`anchor`], [`tags`]): `PcmQueue`, the realtime engine behind
//!   `PlaybackStream`.
//! * [`reader`], [`decoder`], [`probe`]: the songbird live input (`LiveReader` + `PcmDecoder`).
//! * [`opus_stream`], [`encoded`], [`memory`], [`source`]: other inputs.
//! * [`housekeep`]: per-tick wake-ups, events and auto-pause for live sources (ConnWorker).

pub mod anchor;
pub mod decoder;
pub mod encoded;
pub mod housekeep;
pub(crate) mod keep;
pub mod memory;
pub mod opus_stream;
pub mod probe;
pub mod queue;
pub mod reader;
pub mod source;
pub mod tags;
pub mod writer;

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use crate::events::EventHub;
use crate::format::Format;

/// Source of [`LiveSource::id`] values (process-wide, never 0).
static NEXT_SOURCE_ID: AtomicU32 = AtomicU32::new(1);

/// A fresh live-source id: `PcmQueue::new(id, …)` callers (`ConnShared::open_playback`, the
/// internal queue of `spawn_encoded`) and `OpusQueue::new` (which has no id parameter, §12.5) all
/// draw from this one counter, so ids are unique per process. 0 is skipped on wrap-around.
/// See `docs/deviations/W4.md`.
pub fn next_source_id() -> u32 {
    loop {
        let id = NEXT_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

/// Marks the calling thread as a mixer thread on its first reader/decoder call (§5.2 debug
/// enforcement). Tests that drive readers directly set `Role::Harness` first.
#[inline]
pub(crate) fn enter_mixer() {
    use crate::runtime::{Role, role, set_role};
    match role() {
        Role::Unknown => set_role(Role::Mixer),
        r => debug_assert_ne!(r, Role::Python, "mixer entry point on a Python thread"),
    }
}

#[cfg(feature = "test-hooks")]
use std::sync::Arc;

#[cfg(feature = "test-hooks")]
use parking_lot::Mutex;

#[cfg(feature = "test-hooks")]
use crate::tracks::flags::{TrackEndReason, TrackFlags};

/// How a live source closes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CloseMode {
    /// EOF once the queue is empty.
    Drain,
    /// EOF at the next pull; queued audio is freed now.
    Abort,
}

/// The songbird track operations a live source needs (flume sends; never from the mixer).
pub trait TrackControl: Send + Sync {
    fn pause(&self);
    fn play(&self);
    fn stop(&self);
}

impl TrackControl for songbird::tracks::TrackHandle {
    // Errors are ignored: the track may already have ended.
    fn pause(&self) {
        let _ = songbird::tracks::TrackHandle::pause(self);
    }

    fn play(&self) {
        let _ = songbird::tracks::TrackHandle::play(self);
    }

    fn stop(&self) {
        let _ = songbird::tracks::TrackHandle::stop(self);
    }
}

/// Test double for detached connections (§13.2): records calls; `stop()` resolves the flags as a
/// Dispatch `End(Stop)` would. It also keeps the state songbird would report in `TrackInfo`: the
/// pause state (the log is bounded, so it cannot be derived from it), the track volume, loops and
/// the position of the last seek (detached `info()`/`seek()` answer from it).
#[cfg(feature = "test-hooks")]
#[derive(Debug)]
pub struct FakeTrackControl {
    log: Mutex<Vec<&'static str>>,
    flags: Arc<TrackFlags>,
    paused: std::sync::atomic::AtomicBool,
    /// `f32` bits of the songbird track volume.
    volume: AtomicU32,
    /// `None` = infinite.
    loops: Mutex<Option<u32>>,
    position_ms: std::sync::atomic::AtomicU64,
}

#[cfg(feature = "test-hooks")]
impl FakeTrackControl {
    /// Calls beyond this many are not recorded (test-only bound).
    pub const LOG_CAP: usize = 4096;

    /// A control for a track handed over paused (live sources start paused, §4.9).
    pub fn new(flags: Arc<TrackFlags>) -> Self {
        Self::with_paused(flags, true)
    }

    /// A control for a track handed over paused or playing (memory tracks: `paused=`).
    pub fn with_paused(flags: Arc<TrackFlags>, paused: bool) -> Self {
        FakeTrackControl {
            log: Mutex::new(Vec::new()),
            flags,
            paused: std::sync::atomic::AtomicBool::new(paused),
            volume: AtomicU32::new(1.0f32.to_bits()),
            loops: Mutex::new(Some(0)),
            position_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The songbird track volume (`play(volume=)`, `set_volume`).
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Acquire))
    }

    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.to_bits(), Ordering::Release);
    }

    /// Remaining loops (`None` = infinite).
    pub fn loops(&self) -> Option<u32> {
        *self.loops.lock()
    }

    pub fn set_loops(&self, l: Option<u32>) {
        *self.loops.lock() = l;
    }

    /// The position songbird would report: that of the last seek (nothing plays detached).
    pub fn position(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.position_ms.load(Ordering::Acquire))
    }

    /// A seek songbird would perform: records `"seek"` and moves the position.
    pub fn seek(&self, to: std::time::Duration) {
        let ms = u64::try_from(to.as_millis()).unwrap_or(u64::MAX);
        self.position_ms.store(ms, Ordering::Release);
        self.record("seek");
    }

    /// The recorded calls (`"pause"`, `"play"`, `"stop"`, `"seek"`), oldest first.
    pub fn log(&self) -> Vec<&'static str> {
        self.log.lock().clone()
    }

    /// Whether songbird would consider the track paused (the last `pause`/`play` wins).
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    fn record(&self, call: &'static str) {
        let mut log = self.log.lock();
        if log.len() < Self::LOG_CAP {
            log.push(call);
        }
    }
}

#[cfg(feature = "test-hooks")]
impl TrackControl for FakeTrackControl {
    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.record("pause");
    }

    fn play(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.record("play");
    }

    fn stop(&self) {
        self.record("stop");
        // Like a songbird handle: once the connection is closing the Driver is gone (T2 removed
        // the Dispatch), so no `End(Stop)` arrives and T8 resolves the track `disconnected`.
        if !self.flags.conn_closing() {
            self.flags.resolve(TrackEndReason::Stopped, None);
        }
    }
}

/// Result of one housekeeping pass over a live source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HouseStatus {
    Live,
    /// The reader is gone; deregister.
    Finished,
}

/// A live source the housekeeper polls every tick (RX thread).
pub trait LiveSource: Send + Sync {
    fn id(&self) -> u32;
    /// May lock, ring, emit and pause/resume the track.
    fn housekeep(&self, now: Instant, events: &EventHub) -> HouseStatus;
    fn on_link(&self, up: bool);
    fn close(&self, mode: CloseMode);
    /// T5 / worker exit: like `close(Abort)`, but writers then get
    /// `StreamClosed("connection_closed")`. See `docs/deviations/W4.md`.
    fn conn_closed(&self) {
        self.close(CloseMode::Abort);
    }
    /// `TrackHandle.pause()/resume()` of this source's track: `true` if the source keeps the
    /// user pause itself (it then pauses/resumes its songbird track), `false` to use the handle.
    fn set_user_paused(&self, _paused: bool) -> bool {
        false
    }
    /// `TrackHandle.set_volume()`: `true` if the source applies the volume itself (its decoder),
    /// `false` to use the songbird track's volume.
    fn set_source_volume(&self, _v: f32) -> bool {
        false
    }
    /// The volume the source applies itself (`TrackInfo.volume`; songbird's track stays at 1.0),
    /// or `None` when songbird applies it.
    fn source_volume(&self) -> Option<f32> {
        None
    }
    /// Closed with abort or by the connection: its buffers are freed and it no longer counts
    /// towards `MAX_LIVE_SOURCES_PER_CONN`, although songbird keeps its reader until a live mixer
    /// processes the stop (a parked one holds it until the link is up, bounded by the
    /// connection's `resident_tracks`). Atomics only.
    fn is_released(&self) -> bool {
        false
    }
}

/// Frames moved by one mixer pull.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PullReport {
    /// Input-rate frames taken from the queue (real audio + gap silence).
    pub consumed: u32,
    /// Of which real audio.
    pub real: u32,
}

/// What `PcmDecoder` pulls from (a `PcmQueue` or an in-memory `MemPcm`).
pub trait PcmProvider: Send + Sync + 'static {
    fn format(&self) -> Format;
    /// MIXER THREAD: `try_lock` only; no allocation, syscall or logging. Fills `rate/50 × ch`
    /// samples (zeros on underrun or contention).
    fn pull(&self, out: &mut [i16]) -> PullReport;
    /// Atomics only.
    fn should_end(&self) -> bool;
    /// Bumped by `clear()`: the decoder resets its resampler and declicks.
    fn generation(&self) -> u64;
    fn volume(&self) -> f32;
    /// Memory sources seek; live sources return `None`.
    fn seek_to_frame(&self, _frame: u64) -> Option<u64> {
        None
    }
    /// Called (atomics only) when the reader or decoder holding this provider is dropped, on the
    /// mixer, Idle task or DisposalThread (`reader_gone`, §7.4). See `docs/deviations/W4.md`.
    fn on_reader_drop(&self) {}
    /// Called (atomics only) when a panic was caught in the reader or decoder: the source ends
    /// after one silent tick (§5.7). See `docs/deviations/W4.md`.
    fn on_poison(&self) {}
    /// The producer failed and everything it produced was played: the decoder then returns a
    /// decode error once, so songbird ends the track with `reason="error"` (§7.9). Atomics only.
    fn failed(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_ids_are_unique_and_never_zero() {
        let ids: Vec<u32> = (0..1000).map(|_| next_source_id()).collect();
        assert!(ids.iter().all(|&id| id != 0));
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
    }
}
