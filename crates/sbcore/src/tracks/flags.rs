//! Per-track completion flags (W2, §7.10). Atomics only, so readers on the mixer, Idle task and
//! DisposalThread can record hints without locks.
//!
//! Resolution is by **priority, not first writer**: Dispatch's `End`/`Error` handler and T8 are
//! authoritative ([`TrackFlags::resolve`]); a reader's drop only records the provisional `DROPPED`
//! hint, which the housekeeper finalises after a 2-tick grace (`TrackRegistry::poll_ended`).

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use nonmax::NonMaxU32;
use songbird::tracks::{ControlError, TrackHandle};

use crate::error::CoreError;
use crate::limits::VOLUME;
use crate::limits::check;
use crate::runtime::{LiveCounter, live};
#[cfg(feature = "test-hooks")]
use crate::tx::FakeTrackControl;

/// Hint bit: the reader/`DoneGuard` was dropped (provisional; never final).
pub const DROPPED: u8 = 1 << 0;
/// Hint bit: a reader returned EOF.
pub const EOF_SEEN: u8 = 1 << 1;
/// Hint bit: a decode error was observed.
pub const DECODE_ERROR: u8 = 1 << 2;

/// Why a track ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrackEndReason {
    Finished,
    Stopped,
    Error,
    Disconnected,
}

impl TrackEndReason {
    /// `"finished"`, `"stopped"`, `"error"` or `"disconnected"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            TrackEndReason::Finished => "finished",
            TrackEndReason::Stopped => "stopped",
            TrackEndReason::Error => "error",
            TrackEndReason::Disconnected => "disconnected",
        }
    }

    const fn code(self) -> u8 {
        match self {
            TrackEndReason::Finished => 1,
            TrackEndReason::Stopped => 2,
            TrackEndReason::Error => 3,
            TrackEndReason::Disconnected => 4,
        }
    }

    const fn from_code(c: u8) -> Option<Self> {
        match c {
            1 => Some(TrackEndReason::Finished),
            2 => Some(TrackEndReason::Stopped),
            3 => Some(TrackEndReason::Error),
            4 => Some(TrackEndReason::Disconnected),
            _ => None,
        }
    }
}

/// What a track plays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrackKind {
    File,
    Pcm,
    Opus,
    Playback,
    EncodedStream,
    OpusStream,
}

impl TrackKind {
    /// `"file"`, `"pcm"`, `"opus"`, `"playback"`, `"encoded_stream"` or `"opus_stream"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            TrackKind::File => "file",
            TrackKind::Pcm => "pcm",
            TrackKind::Opus => "opus",
            TrackKind::Playback => "playback",
            TrackKind::EncodedStream => "encoded_stream",
            TrackKind::OpusStream => "opus_stream",
        }
    }

    /// Live sources cannot seek or loop (`TrackError`).
    pub const fn is_live(self) -> bool {
        matches!(
            self,
            TrackKind::Playback | TrackKind::EncodedStream | TrackKind::OpusStream
        )
    }
}

/// Completion state of one track.
#[derive(Debug)]
pub struct TrackFlags {
    /// Stable per connection.
    pub id: u64,
    pub kind: TrackKind,
    uuid: OnceLock<u128>,
    /// Final reason code (0 = unresolved).
    ended: AtomicU8,
    /// `DROPPED | EOF_SEEN | DECODE_ERROR`.
    hints: AtomicU8,
    /// Housekeeper tick (+1) at which the `DROPPED` hint was first observed (0 = not yet).
    dropped_at_tick: AtomicU64,
    /// Whether `TrackEnded` was emitted.
    reported: AtomicBool,
    /// Handed to songbird (or, detached, to the parking lot).
    started: AtomicBool,
    /// In the registry's polled list.
    watched: AtomicBool,
    conn_closing: Arc<AtomicBool>,
    error: OnceLock<Box<str>>,
    _live: LiveCounter,
}

impl TrackFlags {
    pub fn new(id: u64, kind: TrackKind, conn_closing: Arc<AtomicBool>) -> Self {
        TrackFlags {
            id,
            kind,
            uuid: OnceLock::new(),
            ended: AtomicU8::new(0),
            hints: AtomicU8::new(0),
            dropped_at_tick: AtomicU64::new(0),
            reported: AtomicBool::new(false),
            started: AtomicBool::new(false),
            watched: AtomicBool::new(false),
            conn_closing,
            error: OnceLock::new(),
            _live: LiveCounter::new(&live::TRACK_FLAGS),
        }
    }

    /// Authoritative (Dispatch `End`/`Error`, T8): sets the final reason unless one is already
    /// final; overrides hints. Returns whether this call resolved the track.
    pub fn resolve(&self, r: TrackEndReason, error: Option<&str>) -> bool {
        if let Some(e) = error {
            // Recorded before the reason is published, so a reader that sees `Error` sees it.
            if self.ended.load(Ordering::Acquire) == 0 {
                let _ = self.error.set(e.into());
            }
        }
        self.ended
            .compare_exchange(0, r.code(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Records a hint bit (`EOF_SEEN` / `DECODE_ERROR` / `DROPPED`); atomics only (mixer-safe).
    pub fn hint(&self, bit: u8) {
        self.hints.fetch_or(bit, Ordering::AcqRel);
    }

    /// The recorded hint bits.
    pub fn hints(&self) -> u8 {
        self.hints.load(Ordering::Acquire)
    }

    /// The final reason, once resolved.
    pub fn ended(&self) -> Option<TrackEndReason> {
        TrackEndReason::from_code(self.ended.load(Ordering::Acquire))
    }

    /// The error message of an `error` end, if any.
    pub fn error(&self) -> Option<&str> {
        match self.ended() {
            Some(TrackEndReason::Error) => self.error.get().map(AsRef::as_ref),
            _ => None,
        }
    }

    /// songbird's track uuid (`as_u128`), once the track was handed to songbird.
    pub fn uuid(&self) -> Option<u128> {
        self.uuid.get().copied()
    }

    /// Whether the track was handed to songbird (a pending queue entry has not been).
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    /// Whether the owning connection is closing.
    pub fn conn_closing(&self) -> bool {
        self.conn_closing.load(Ordering::Acquire)
    }

    pub(crate) fn set_uuid(&self, uuid: u128) {
        let _ = self.uuid.set(uuid);
    }

    pub(crate) fn mark_started(&self) {
        self.started.store(true, Ordering::Release);
    }

    /// `true` the first time (the caller adds the track to the polled list).
    pub(crate) fn set_watched(&self) -> bool {
        !self.watched.swap(true, Ordering::AcqRel)
    }

    /// `true` once for a resolved track (the caller emits `TrackEnded`).
    pub(crate) fn take_report(&self) -> bool {
        self.ended().is_some() && !self.reported.swap(true, Ordering::AcqRel)
    }

    pub(crate) fn is_reported(&self) -> bool {
        self.reported.load(Ordering::Acquire)
    }

    /// Delayed finalisation (§7.10): a `DROPPED` but unresolved track is finalised once `tick` is at
    /// least 2 ticks after the drop was first observed. Returns whether it resolved now.
    pub(crate) fn finalise_dropped(&self, tick: u64) -> bool {
        let hints = self.hints();
        if hints & DROPPED == 0 || self.ended().is_some() {
            return false;
        }
        let seen = self.dropped_at_tick.load(Ordering::Acquire);
        if seen == 0 {
            let _ = self.dropped_at_tick.compare_exchange(
                0,
                tick.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            return false;
        }
        if tick.saturating_add(1) < seen.saturating_add(2) {
            return false;
        }
        let reason = if self.conn_closing() {
            TrackEndReason::Disconnected
        } else if hints & EOF_SEEN != 0 {
            TrackEndReason::Finished
        } else if hints & DECODE_ERROR != 0 {
            TrackEndReason::Error
        } else {
            TrackEndReason::Stopped
        };
        let err = (reason == TrackEndReason::Error).then_some("decode error");
        self.resolve(reason, err)
    }
}

/// Dropped with the reader/input: records the provisional `DROPPED` hint (atomics only; never
/// final). The housekeeper finalises a dropped-but-unresolved track after a 2-tick grace (§7.10).
#[derive(Debug)]
pub struct DoneGuard(Arc<TrackFlags>);

impl DoneGuard {
    pub(crate) fn new(flags: Arc<TrackFlags>) -> Self {
        live::DONE_GUARD.fetch_add(1, Ordering::AcqRel);
        DoneGuard(flags)
    }

    /// The flags this guard reports to (readers record `EOF_SEEN` / `DECODE_ERROR` through it).
    pub fn flags(&self) -> &Arc<TrackFlags> {
        &self.0
    }
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.0.hint(DROPPED);
        live::DONE_GUARD.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What a `NativeTrack` holds: the flags plus songbird's handle once the track started.
#[derive(Clone, Debug)]
pub struct TrackRef {
    pub flags: Arc<TrackFlags>,
    pub handle: Arc<OnceLock<songbird::tracks::TrackHandle>>,
    /// Detached connections (§13.2): the `FakeTrackControl` standing in for the handle.
    #[cfg(feature = "test-hooks")]
    pub fake: Arc<OnceLock<Arc<FakeTrackControl>>>,
}

fn control_err(e: ControlError) -> CoreError {
    CoreError::Track(e.to_string().into())
}

impl TrackRef {
    pub(crate) fn new(flags: Arc<TrackFlags>) -> Self {
        TrackRef {
            flags,
            handle: Arc::new(OnceLock::new()),
            #[cfg(feature = "test-hooks")]
            fake: Arc::new(OnceLock::new()),
        }
    }

    /// Whether the track was handed to songbird (or, detached, to the fake control).
    pub fn is_started(&self) -> bool {
        self.flags.is_started()
    }

    /// Whether `other` refers to the same track.
    pub fn same(&self, other: &TrackRef) -> bool {
        Arc::ptr_eq(&self.flags, &other.flags)
    }

    /// Runs `op` on songbird's handle, or `fake` on the detached fake control.
    fn control(
        &self,
        op: impl FnOnce(&TrackHandle) -> Result<(), CoreError>,
        #[allow(unused_variables)] fake: impl FnOnce(&dyn crate::tx::TrackControl),
    ) -> Result<(), CoreError> {
        if let Some(h) = self.handle.get() {
            return op(h);
        }
        #[cfg(feature = "test-hooks")]
        if let Some(f) = self.fake.get() {
            fake(&**f);
            return Ok(());
        }
        Err(CoreError::Track("track has not started".into()))
    }

    /// `TrackHandle.pause()` (`ControlError` → `Track`).
    pub fn pause(&self) -> Result<(), CoreError> {
        self.control(|h| h.pause().map_err(control_err), |f| f.pause())
    }

    /// `TrackHandle.resume()`.
    pub fn resume(&self) -> Result<(), CoreError> {
        self.control(|h| h.play().map_err(control_err), |f| f.play())
    }

    /// `TrackHandle.stop()` (songbird fires `End(Stop)`; the fake resolves `stopped` directly).
    pub fn stop(&self) -> Result<(), CoreError> {
        self.control(|h| h.stop().map_err(control_err), |f| f.stop())
    }

    /// `0.0..=4.0`; applied by songbird's live mixer.
    pub fn set_volume(&self, v: f32) -> Result<(), CoreError> {
        let v = check("volume", v, VOLUME)?;
        self.control(|h| h.set_volume(v).map_err(control_err), |_| {})
    }

    /// `None` loops forever; `TrackError` for live sources.
    pub fn set_loops(&self, loops: Option<u32>) -> Result<(), CoreError> {
        if self.flags.kind.is_live() {
            return Err(CoreError::Track("live sources cannot loop".into()));
        }
        self.control(|h| set_track_loops(h, loops), |_| {})
    }

    /// The `FakeTrackControl` call log of a detached track (`"pause"`, `"play"`, `"stop"`).
    #[cfg(feature = "test-hooks")]
    pub fn fake_log(&self) -> Vec<&'static str> {
        self.fake.get().map(|f| f.log()).unwrap_or_default()
    }
}

/// Options of `play` / `enqueue`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayOpts {
    pub volume: f32,
    /// `None` = infinite.
    pub loops: Option<u32>,
    pub paused: bool,
}

impl Default for PlayOpts {
    fn default() -> Self {
        PlayOpts {
            volume: 1.0,
            loops: Some(0),
            paused: false,
        }
    }
}

/// A change to a pending queue entry's stored [`PlayOpts`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PendingChange {
    Paused(bool),
    Volume(f32),
    Loops(Option<u32>),
}

impl PendingChange {
    /// Applies the change to stored options.
    pub fn apply_to(self, o: &mut PlayOpts) {
        match self {
            PendingChange::Paused(p) => o.paused = p,
            PendingChange::Volume(v) => o.volume = v,
            PendingChange::Loops(l) => o.loops = l,
        }
    }
}

/// `songbird::tracks::LoopState` for `loops` (`None` = infinite; `n < u32::MAX`).
pub fn loop_state(loops: Option<u32>) -> Result<songbird::tracks::LoopState, CoreError> {
    Ok(match loops {
        None => songbird::tracks::LoopState::Infinite,
        Some(n) => songbird::tracks::LoopState::Finite(
            NonMaxU32::new(n).ok_or(CoreError::Track("loop count must be < 2**32 - 1".into()))?,
        ),
    })
}

/// `TrackHandle.set_loops`: `None` loops forever, `Some(0)` disables looping, `Some(n)` plays `n`
/// more times (`n < u32::MAX`). songbird `ControlError` → `Track`.
pub fn set_track_loops(h: &TrackHandle, loops: Option<u32>) -> Result<(), CoreError> {
    let r = match loops {
        None => h.enable_loop(),
        Some(0) => h.disable_loop(),
        Some(n) => {
            let n = NonMaxU32::new(n)
                .ok_or(CoreError::Track("loop count must be < 2**32 - 1".into()))?;
            h.loop_for(n)
        }
    };
    r.map_err(control_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(kind: TrackKind) -> (Arc<TrackFlags>, Arc<AtomicBool>) {
        let closing = Arc::new(AtomicBool::new(false));
        (Arc::new(TrackFlags::new(1, kind, closing.clone())), closing)
    }

    #[test]
    fn first_resolution_wins() {
        let (f, _) = flags(TrackKind::File);
        assert_eq!(f.ended(), None);
        assert!(f.resolve(TrackEndReason::Error, Some("boom")));
        assert!(!f.resolve(TrackEndReason::Stopped, None));
        assert_eq!(f.ended(), Some(TrackEndReason::Error));
        assert_eq!(f.error(), Some("boom"));
    }

    #[test]
    fn error_text_only_for_error_ends() {
        let (f, _) = flags(TrackKind::File);
        assert!(f.resolve(TrackEndReason::Stopped, None));
        assert!(!f.resolve(TrackEndReason::Error, Some("late")));
        assert_eq!(f.error(), None);
    }

    #[test]
    fn done_guard_sets_dropped_hint() {
        let (f, _) = flags(TrackKind::Pcm);
        drop(DoneGuard::new(f.clone()));
        assert_eq!(f.hints() & DROPPED, DROPPED);
        assert_eq!(f.ended(), None);
    }

    #[test]
    fn dropped_track_is_finalised_after_two_ticks_by_priority() {
        for (hints, closing, want) in [
            (0u8, false, TrackEndReason::Stopped),
            (EOF_SEEN, false, TrackEndReason::Finished),
            (DECODE_ERROR, false, TrackEndReason::Error),
            (EOF_SEEN, true, TrackEndReason::Disconnected),
        ] {
            let (f, c) = flags(TrackKind::File);
            c.store(closing, Ordering::Release);
            f.hint(hints);
            drop(DoneGuard::new(f.clone()));
            assert!(!f.finalise_dropped(10), "first observation");
            assert!(!f.finalise_dropped(11), "1 tick of grace");
            assert!(f.finalise_dropped(12));
            assert_eq!(f.ended(), Some(want));
        }
    }

    #[test]
    fn authoritative_event_within_the_grace_wins() {
        // DoneGuard drop, then End(PlayMode::End) within 2 ticks → finished, not stopped.
        let (f, _) = flags(TrackKind::File);
        drop(DoneGuard::new(f.clone()));
        assert!(!f.finalise_dropped(5));
        assert!(f.resolve(TrackEndReason::Finished, None));
        assert!(!f.finalise_dropped(9));
        assert_eq!(f.ended(), Some(TrackEndReason::Finished));
    }

    #[test]
    fn reports_once() {
        let (f, _) = flags(TrackKind::File);
        assert!(!f.take_report(), "unresolved");
        f.resolve(TrackEndReason::Stopped, None);
        assert!(f.take_report());
        assert!(!f.take_report());
        assert!(f.is_reported());
    }

    #[test]
    fn unstarted_track_refuses_control() {
        let (f, _) = flags(TrackKind::File);
        let t = TrackRef::new(f);
        assert!(!t.is_started());
        assert!(matches!(t.pause(), Err(CoreError::Track(_))));
        assert!(matches!(t.set_volume(5.0), Err(CoreError::Limit(_))));
        let (lf, _) = flags(TrackKind::Playback);
        assert!(matches!(
            TrackRef::new(lf).set_loops(None),
            Err(CoreError::Track(_))
        ));
    }

    #[test]
    fn loop_states() {
        assert!(matches!(
            loop_state(None).unwrap(),
            songbird::tracks::LoopState::Infinite
        ));
        assert!(matches!(
            loop_state(Some(2)).unwrap(),
            songbird::tracks::LoopState::Finite(n) if n.get() == 2
        ));
        assert!(loop_state(Some(u32::MAX)).is_err());
    }
}
