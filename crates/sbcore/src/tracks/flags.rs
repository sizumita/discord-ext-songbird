//! Per-track completion flags (W2, §7.10). Atomics only, so readers on the mixer, Idle task and
//! DisposalThread can record hints without locks.
#![allow(dead_code)] // phase-0 skeleton

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use nonmax::NonMaxU32;
use songbird::tracks::TrackHandle;

use crate::error::CoreError;
use crate::runtime::{LiveCounter, live};

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
    dropped_at_tick: AtomicU64,
    /// Whether `TrackEnded` was emitted.
    reported: AtomicBool,
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
            conn_closing,
            error: OnceLock::new(),
            _live: LiveCounter::new(&live::TRACK_FLAGS),
        }
    }

    /// Authoritative (Dispatch `End`/`Error`, T8): sets the final reason unless one is already
    /// final; overrides hints. Returns whether this call resolved the track.
    pub fn resolve(&self, r: TrackEndReason, error: Option<&str>) -> bool {
        let won = self
            .ended
            .compare_exchange(0, r.code(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if won && let Some(e) = error {
            let _ = self.error.set(e.into());
        }
        won
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
        self.error.get().map(AsRef::as_ref)
    }

    /// songbird's track uuid (`as_u128`), once the track was handed to songbird.
    pub fn uuid(&self) -> Option<u128> {
        self.uuid.get().copied()
    }

    pub(crate) fn set_uuid(&self, uuid: u128) {
        let _ = self.uuid.set(uuid);
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
    r.map_err(|e| CoreError::Track(e.to_string().into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_resolution_wins() {
        let f = TrackFlags::new(1, TrackKind::File, Arc::new(AtomicBool::new(false)));
        assert_eq!(f.ended(), None);
        assert!(f.resolve(TrackEndReason::Error, Some("boom")));
        assert!(!f.resolve(TrackEndReason::Stopped, None));
        assert_eq!(f.ended(), Some(TrackEndReason::Error));
        assert_eq!(f.error(), Some("boom"));
    }

    #[test]
    fn done_guard_sets_dropped_hint() {
        let f = Arc::new(TrackFlags::new(
            1,
            TrackKind::Pcm,
            Arc::new(AtomicBool::new(false)),
        ));
        drop(DoneGuard::new(f.clone()));
        assert_eq!(f.hints() & DROPPED, DROPPED);
        assert_eq!(f.ended(), None);
    }
}
