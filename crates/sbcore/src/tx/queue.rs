//! `PcmQueue`: the realtime engine behind `PlaybackStream` (W4, §7.2–§7.8).
//!
//! Audio is stored at the **input rate** as interleaved `i16`; resampling to 48 kHz happens on the
//! mixer side in a continuous per-track `TickResampler`. Writers take L6w (carry splice, ≤ 7 bytes)
//! then L6 (O(1) commit); `clear()`, `close()`, the housekeeper and T5 take L6 only; the mixer only
//! ever `try_lock`s L6.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use parking_lot::Mutex;

use super::anchor::{Anchor, GapReject};
use super::tags::{RejectSet, TagId, TagTable};
use super::{CloseMode, HouseStatus, LiveSource, PcmProvider, PullReport, TrackControl};
use crate::clock::Clock;
use crate::error::CoreError;
use crate::events::EventHub;
use crate::format::{Carry, Format};
use crate::limits::{
    IDLE_MS, MAX_GAP_MS, OUTPUT_LATENCY_MS, PLAYBACK_BUFFER_MS, PREBUFFER_MS, VOLUME, check,
};
use crate::wake::{WakeId, WakeToken};

/// `end_flags` bit: drain then end.
pub(crate) const END_DRAIN: u8 = 1 << 0;
/// `end_flags` bit: end now.
pub(crate) const END_ABORT: u8 = 1 << 1;
/// `end_flags` bit: a panic poisoned the source (one silent tick, then EOF).
pub(crate) const END_POISON: u8 = 1 << 2;
/// `end_flags` bit: the connection closed.
pub(crate) const END_CONN_CLOSED: u8 = 1 << 3;

/// What a playback does with writes while the voice link is down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectPolicy {
    Buffer,
    Discard,
}

/// `open_playback` options (§9.4).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackSpec {
    pub format: Format,
    pub max_buffer_ms: u32,
    pub prebuffer_ms: u32,
    pub idle_ms: u32,
    pub auto_pause: bool,
    pub max_gap_ms: u32,
    pub output_latency_ms: u32,
    pub volume: f32,
    pub when_disconnected: DisconnectPolicy,
}

impl PlaybackSpec {
    /// Ranges of §12.2 plus `prebuffer_ms ≤ max_buffer_ms/2` and `max_gap_ms ≤ max_buffer_ms/2`.
    pub fn validate(&self) -> Result<(), CoreError> {
        let max_buffer_ms = check("max_buffer_ms", self.max_buffer_ms, PLAYBACK_BUFFER_MS)?;
        check("prebuffer_ms", self.prebuffer_ms, PREBUFFER_MS)?;
        check("idle_ms", self.idle_ms, IDLE_MS)?;
        check("max_gap_ms", self.max_gap_ms, MAX_GAP_MS)?;
        check(
            "output_latency_ms",
            self.output_latency_ms,
            OUTPUT_LATENCY_MS,
        )?;
        check("volume", self.volume, VOLUME)?;
        let half = max_buffer_ms / 2;
        check("prebuffer_ms", self.prebuffer_ms, 0..=half)?;
        check("max_gap_ms", self.max_gap_ms, 0..=half)?;
        Ok(())
    }
}

/// Audio of one write: raw bytes in the stream encoding, or base64 text of them.
#[derive(Clone, Copy, Debug)]
pub enum WriteData<'a> {
    Pcm(&'a [u8]),
    Base64(&'a [u8]),
}

/// Per-write metadata.
#[derive(Clone, Copy, Debug, Default)]
pub struct WriteMeta<'a> {
    /// Provider item id (≤ `MAX_TAG_LEN` bytes).
    pub tag: Option<&'a str>,
    /// Provider timestamp (gap filling, §7.6).
    pub at_ms: Option<i64>,
}

/// Result of an accepted (or silently rejected) write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    Accepted {
        frames: u64,
        gap_frames: u64,
    },
    /// Dropped by `clear()`'s late-write rejection (counted in `rejected_ms`).
    Rejected {
        frames: u64,
    },
}

/// `clear()` options (§7.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClearOpts {
    pub reject_late: bool,
    pub hold_ms: u32,
    pub reject_until_gap_ms: Option<u32>,
}

impl Default for ClearOpts {
    fn default() -> Self {
        ClearOpts {
            reject_late: true,
            hold_ms: 0,
            reject_until_gap_ms: None,
        }
    }
}

/// `native.ItemProgress` data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemProgressData {
    pub tag: Arc<str>,
    pub written_ms: u64,
    /// Clamped to `written_ms`.
    pub played_ms: u64,
    pub heard_ms: u64,
    /// `end_item(tag)` was called and everything written has been played.
    pub complete: bool,
}

/// `native.ClearResult` data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClearSummary {
    pub discarded_ms: u64,
    pub played_ms: u64,
    pub heard_ms: u64,
    pub position_ms: u64,
    pub generation: u64,
    /// Every open tag plus every tag pulled within the last `output_latency_ms`.
    pub items: Vec<ItemProgressData>,
}

/// `native.PlaybackStats` data (+ positions).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaybackStatsData {
    pub written_ms: u64,
    pub played_ms: u64,
    pub heard_ms: u64,
    pub buffered_ms: u64,
    pub position_ms: u64,
    pub underrun_ms: u64,
    pub gap_filled_ms: u64,
    pub rejected_ms: u64,
    pub discarded_ms: u64,
    pub contended_ticks: u64,
    pub clears: u64,
    pub tags_evicted: u64,
    pub idle: bool,
}

/// Limits derived from a validated [`PlaybackSpec`] (input-rate frames and ticks).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PlaybackLimits {
    pub(crate) cap_frames: u64,
    pub(crate) max_gap_ms: u32,
    pub(crate) idle_ticks: u32,
    pub(crate) prebuffer_frames: u64,
    pub(crate) prebuffer_ticks: u32,
    pub(crate) output_latency_ms: u32,
    pub(crate) auto_pause: bool,
    pub(crate) when_disconnected: DisconnectPolicy,
}

/// One queued segment. Gap silence is not materialised.
#[derive(Debug)]
pub(crate) enum Seg {
    Pcm {
        data: Box<[i16]>,
        off: u32,
        tag: Option<TagId>,
    },
    Silence {
        frames: u64,
    },
}

/// Prebuffer gate (§7.3e, §7.4).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Prebuffer {
    pub(crate) armed: bool,
    pub(crate) ticks_left: u32,
}

/// State under L6.
#[derive(Debug)]
pub(crate) struct QueueInner {
    pub(crate) segs: VecDeque<Seg>,
    /// Includes non-materialised silence.
    pub(crate) queued_frames: u64,
    pub(crate) anchor: Option<Anchor>,
    pub(crate) timed_end_ms: Option<i64>,
    pub(crate) gap_reject: Option<GapReject>,
    pub(crate) tags: TagTable,
    pub(crate) rejected: RejectSet,
    pub(crate) hold_until: Option<Instant>,
    pub(crate) gate: Prebuffer,
    /// (pulls, when the housekeeper saw them advance).
    pub(crate) pull_epoch: (u64, Instant),
    pub(crate) generation: u64,
    pub(crate) closed: Option<CloseMode>,
    pub(crate) space_wanted: u64,
    pub(crate) drain_waiting: bool,
    pub(crate) idle_waiting: bool,
    pub(crate) end_waiting: bool,
    pub(crate) active: bool,
    pub(crate) paused_user: bool,
    /// `true` at open: live sources start paused (§4.9).
    pub(crate) paused_idle: bool,
    pub(crate) link_up: bool,
}

/// Lock-free mirrors for the mixer, the housekeeper and Python properties.
#[derive(Debug, Default)]
pub(crate) struct QueueAtomics {
    pub(crate) pulls: AtomicU64,
    pub(crate) position: AtomicU64,
    pub(crate) played_real: AtomicU64,
    pub(crate) queued: AtomicU64,
    pub(crate) written: AtomicU64,
    pub(crate) underrun: AtomicU64,
    pub(crate) gap_filled: AtomicU64,
    pub(crate) rejected: AtomicU64,
    pub(crate) discarded: AtomicU64,
    pub(crate) contended: AtomicU64,
    pub(crate) clears: AtomicU64,
    pub(crate) space_epoch: AtomicU64,
    pub(crate) last_real_pull: AtomicU64,
    pub(crate) generation: AtomicU64,
    pub(crate) volume_bits: AtomicU32,
    /// Snapshot of the carry length for lock-free parsing (§7.3).
    pub(crate) carry_len: AtomicU8,
    pub(crate) tags_evicted: AtomicU64,
    /// `END_DRAIN | END_ABORT | END_POISON | END_CONN_CLOSED`; set BEFORE taking L6.
    pub(crate) end_flags: AtomicU8,
    pub(crate) reader_gone: AtomicBool,
}

/// The realtime PCM queue of one `PlaybackStream` (§7.2).
pub struct PcmQueue {
    id: u32,
    fmt: Format,
    lim: PlaybackLimits,
    /// L6w: the partial-frame carry only.
    writer: Mutex<Carry>,
    /// L6.
    inner: Mutex<QueueInner>,
    st: QueueAtomics,
    /// One per connection; mirrors `set_self_mute` (§7.8).
    send_muted: Arc<AtomicBool>,
    wake: WakeToken,
    clock: Clock,
    track: OnceLock<Box<dyn TrackControl>>,
}

impl PcmQueue {
    /// Validates `spec`; the queue starts with `paused_idle = true` (live sources start paused).
    pub fn new(
        id: u32,
        spec: &PlaybackSpec,
        token: WakeToken,
        clock: Clock,
        send_muted: Arc<AtomicBool>,
    ) -> Result<Arc<Self>, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// All-or-nothing write (§7.3): decode + parse outside locks, L6w splice, L6 commit.
    /// `PlaybackFull` consumes nothing.
    pub fn write(
        &self,
        data: WriteData<'_>,
        meta: WriteMeta<'_>,
    ) -> Result<WriteOutcome, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Arms a space wake-up for `nbytes`; `true` ⇒ there is already enough space.
    pub fn arm_space(&self, nbytes: usize) -> bool {
        false
    }

    /// Releases the prebuffer gate and arms the drain wake-up; `true` ⇒ already drained.
    pub fn arm_drain(&self) -> bool {
        false
    }

    /// Arms the Active → Idle wake-up; `true` ⇒ already idle.
    pub fn arm_idle(&self) -> bool {
        false
    }

    /// Arms the reader-gone wake-up (`aclose(drain=True)`); `true` ⇒ already ended.
    pub fn arm_end(&self) -> bool {
        false
    }

    /// Barge-in (§7.7): discards everything queued now (L6 only, never L6w).
    pub fn clear(&self, opts: ClearOpts) -> Result<ClearSummary, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Closes the tag, sets `ended`, releases the prebuffer gate.
    pub fn end_item(&self, tag: &str) {
        // phase 0: tags are not tracked yet.
    }

    /// `pause()` / `resume()` (`paused_user`, plus `track.pause()` / `track.play()`).
    pub fn pause(&self, paused: bool) {
        // phase 0: no track control yet.
    }

    /// 0.0..=4.0, applied by the decoder with a 5 ms ramp.
    pub fn set_volume(&self, v: f32) -> Result<(), CoreError> {
        let v = check("volume", v, VOLUME)?;
        self.st.volume_bits.store(v.to_bits(), Ordering::Release);
        Ok(())
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.st.volume_bits.load(Ordering::Acquire))
    }

    /// Progress of one tag.
    pub fn item(&self, tag: &str) -> Option<ItemProgressData> {
        None
    }

    pub fn stats(&self) -> PlaybackStatsData {
        PlaybackStatsData::default()
    }

    pub fn is_closed(&self) -> bool {
        self.st.end_flags.load(Ordering::Acquire) != 0
    }

    pub fn wake_id(&self) -> WakeId {
        self.wake.id()
    }

    /// Attaches songbird's track (or a `FakeTrackControl`), once.
    pub(crate) fn attach_track(&self, t: Box<dyn TrackControl>) {
        let _ = self.track.set(t);
    }
}

impl PcmProvider for PcmQueue {
    fn format(&self) -> Format {
        self.fmt
    }

    fn pull(&self, out: &mut [i16]) -> PullReport {
        out.fill(0);
        PullReport::default()
    }

    fn should_end(&self) -> bool {
        false
    }

    fn generation(&self) -> u64 {
        self.st.generation.load(Ordering::Acquire)
    }

    fn volume(&self) -> f32 {
        PcmQueue::volume(self)
    }

    fn on_reader_drop(&self) {
        self.st.reader_gone.store(true, Ordering::Release);
    }
}

impl LiveSource for PcmQueue {
    fn id(&self) -> u32 {
        self.id
    }

    fn housekeep(&self, now: Instant, events: &EventHub) -> HouseStatus {
        HouseStatus::Live
    }

    fn on_link(&self, up: bool) {
        // phase 0: link state is not tracked yet.
    }

    fn close(&self, mode: CloseMode) {
        let bit = match mode {
            CloseMode::Drain => END_DRAIN,
            CloseMode::Abort => END_ABORT,
        };
        self.st.end_flags.fetch_or(bit, Ordering::AcqRel);
    }
}
