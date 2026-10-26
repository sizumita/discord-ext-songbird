//! `PcmQueue`: the realtime engine behind `PlaybackStream` (W4, §7.2–§7.8).
//!
//! Audio is stored at the **input rate** as interleaved `i16`; resampling to 48 kHz happens on the
//! mixer side in a continuous per-track `TickResampler`. Writers take L6w (carry splice, ≤ 7 bytes)
//! then L6 (O(1) commit); `clear()`, `close()`, the housekeeper and T5 take L6 only; the mixer only
//! ever `try_lock`s L6.
//!
//! Memory: the PCM itself is bounded by `cap_frames`; the number of queued segments is bounded by
//! `max_segs = cap_frames / MIN_SEG_FRAMES + 64`, so the per-write overhead of pathological
//! one-frame writes stays proportional to the cap (`docs/deviations/W4.md`). Segments the mixer has
//! consumed go to a fixed-capacity graveyard that the housekeeper frees on the RX thread, so the
//! mixer neither allocates nor frees on the PCM path.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use smallvec::SmallVec;

use super::anchor::{self, Anchor, GapDecision, GapReject};
use super::tags::{RejectSet, TagId, TagTable};
use super::writer::parse_write;
use super::{CloseMode, HouseStatus, LiveSource, PcmProvider, PullReport, TrackControl};
use crate::clock::Clock;
use crate::error::{CloseReason, CoreError};
use crate::events::{EventData, EventHub};
use crate::format::{Carry, Format, PcmParser};
use crate::limits::{
    HOLD_MS, IDLE_MS, MAX_GAP_MS, MAX_TAG_LEN, OUTPUT_LATENCY_MS, PLAYBACK_BUFFER_MS, PREBUFFER_MS,
    REJECT_GAP_MS, TICK_MS, VOLUME, check,
};
use crate::runtime::{LiveCounter, live};
use crate::sync::lock_checked;
use crate::wake::{WAKE_CONN, WakeId, WakeToken};

/// `end_flags` bit: drain then end.
pub(crate) const END_DRAIN: u8 = 1 << 0;
/// `end_flags` bit: end now.
pub(crate) const END_ABORT: u8 = 1 << 1;
/// `end_flags` bit: a panic poisoned the source (one silent tick, then EOF).
pub(crate) const END_POISON: u8 = 1 << 2;
/// `end_flags` bit: the connection closed.
pub(crate) const END_CONN_CLOSED: u8 = 1 << 3;
/// `end_flags` bit: the producer failed (encoded decode error): the decoder reports an error once
/// the queue has drained, so the track ends with `TrackEnded(reason="error")`.
pub(crate) const END_FAILED: u8 = 1 << 4;

/// Bits that end the source at the next pull.
const END_NOW: u8 = END_ABORT | END_POISON | END_CONN_CLOSED;

/// Minimum average segment size the segment cap is sized for.
const MIN_SEG_FRAMES: u64 = 32;
/// Capacity of the consumed-segment graveyard (mixer pushes without reallocating).
const GRAVE_CAP: usize = 32;
/// Initial segment-deque capacity (grows lazily up to `max_segs`).
const SEGS_INITIAL: usize = 16;

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

/// Limits derived from a validated [`PlaybackSpec`] (input-rate frames).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PlaybackLimits {
    pub(crate) cap_frames: u64,
    pub(crate) max_segs: usize,
    pub(crate) max_gap_ms: u32,
    pub(crate) idle_ms: u32,
    pub(crate) prebuffer_frames: u64,
    pub(crate) prebuffer_ticks: u32,
    pub(crate) output_latency_ms: u32,
    pub(crate) auto_pause: bool,
    pub(crate) when_disconnected: DisconnectPolicy,
}

impl PlaybackLimits {
    fn new(spec: &PlaybackSpec) -> Self {
        let fmt = spec.format;
        let cap_frames = fmt.frames_for_ms(spec.max_buffer_ms) as u64;
        PlaybackLimits {
            cap_frames,
            max_segs: usize::try_from(cap_frames / MIN_SEG_FRAMES)
                .unwrap_or(usize::MAX)
                .saturating_add(64),
            max_gap_ms: spec.max_gap_ms,
            idle_ms: spec.idle_ms,
            prebuffer_frames: fmt.frames_for_ms(spec.prebuffer_ms) as u64,
            prebuffer_ticks: spec.prebuffer_ms.div_ceil(TICK_MS),
            output_latency_ms: spec.output_latency_ms,
            auto_pause: spec.auto_pause,
            when_disconnected: spec.when_disconnected,
        }
    }
}

/// One queued segment. Gap silence is not materialised.
#[derive(Debug)]
pub(crate) enum Seg {
    Pcm {
        data: Box<[i16]>,
        /// Samples already consumed.
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
    /// Real audio only.
    pub(crate) queued_real: u64,
    pub(crate) anchor: Option<Anchor>,
    pub(crate) timed_end_ms: Option<i64>,
    pub(crate) gap_reject: Option<GapReject>,
    pub(crate) tags: TagTable,
    pub(crate) rejected: RejectSet,
    pub(crate) hold_until: Option<Instant>,
    pub(crate) gate: Prebuffer,
    pub(crate) generation: u64,
    pub(crate) closed: Option<CloseMode>,
    pub(crate) space_wanted: u64,
    pub(crate) drain_waiting: bool,
    pub(crate) idle_waiting: bool,
    pub(crate) end_waiting: bool,
    pub(crate) active: bool,
    /// The next accepted write starts a turn (never written, idle, or cleared since).
    pub(crate) fresh_turn: bool,
    /// Wall time of the last accepted write or clear (idle detection).
    pub(crate) last_activity: Instant,
    pub(crate) write_seq: u64,
    pub(crate) paused_user: bool,
    /// `true` at open: live sources start paused (§4.9).
    pub(crate) paused_idle: bool,
    /// What the attached track was last told (`play` ⇔ true).
    pub(crate) track_playing: bool,
    pub(crate) stop_sent: bool,
    pub(crate) link_up: bool,
    /// Consumed segments, freed by the housekeeper (capacity `GRAVE_CAP`, never grown).
    pub(crate) dead: Vec<Box<[i16]>>,
}

/// Lock-free mirrors for the mixer, the housekeeper and Python properties.
#[derive(Debug, Default)]
pub(crate) struct QueueAtomics {
    /// Number of pulls so far (written by the mixer only).
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
    /// Pull count (index + 1) of the last pull that played real audio; 0 = never.
    pub(crate) last_real_pull: AtomicU64,
    pub(crate) generation: AtomicU64,
    pub(crate) volume_bits: AtomicU32,
    /// Snapshot of the carry length for lock-free parsing (§7.3).
    pub(crate) carry_len: AtomicU8,
    pub(crate) tags_evicted: AtomicU64,
    /// `END_*` bits; set BEFORE taking L6.
    pub(crate) end_flags: AtomicU8,
    pub(crate) reader_gone: AtomicBool,
    pub(crate) active: AtomicBool,
    /// Pull epoch (§7.5): `epoch_pulls` pulls had happened at `base + epoch_us`. Written by the
    /// housekeeper only; a torn read is off by at most one tick.
    pub(crate) epoch_pulls: AtomicU64,
    pub(crate) epoch_us: AtomicU64,
}

/// Housekeeper-only state (RX thread; never contended).
#[derive(Debug)]
struct HouseLocal {
    spare: Vec<Box<[i16]>>,
    state: HouseState,
}

#[derive(Clone, Copy, Debug, Default)]
struct HouseState {
    seen_pulls: u64,
    seen_real: u64,
    announced: bool,
    ended_reported: bool,
}

/// Side effects collected under L6 and executed after unlocking.
#[derive(Default)]
struct After {
    ring: bool,
    stop: bool,
    drop_segs: VecDeque<Seg>,
    /// The samples of a write that was not committed.
    drop_samples: Option<Box<[i16]>>,
}

/// The realtime PCM queue of one `PlaybackStream` (§7.2).
pub struct PcmQueue {
    id: u32,
    fmt: Format,
    parser: PcmParser,
    lim: PlaybackLimits,
    /// L6w: the partial-frame carry only.
    writer: Mutex<Carry>,
    /// L6.
    inner: Mutex<QueueInner>,
    st: QueueAtomics,
    /// One per connection; mirrors `set_self_mute` (§7.8).
    send_muted: Arc<AtomicBool>,
    /// `None` for the internal queue of an encoded stream.
    wake: Option<WakeToken>,
    clock: Clock,
    base: Instant,
    /// `PlaybackStarted/Idle/Ended` events (off for internal queues).
    emit_events: bool,
    house: Mutex<HouseLocal>,
    track: OnceLock<Box<dyn TrackControl>>,
    _live: LiveCounter,
}

impl std::fmt::Debug for PcmQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PcmQueue")
            .field("id", &self.id)
            .field("fmt", &self.fmt)
            .finish_non_exhaustive()
    }
}

fn ms_of(fmt: Format, frames: u64) -> u64 {
    fmt.ms_for_frames(frames)
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
        Self::new_internal(id, spec, Some(token), clock, send_muted, true)
    }

    /// Like [`PcmQueue::new`]; `wake: None` and `emit_events: false` for internal queues.
    pub(crate) fn new_internal(
        id: u32,
        spec: &PlaybackSpec,
        wake: Option<WakeToken>,
        clock: Clock,
        send_muted: Arc<AtomicBool>,
        emit_events: bool,
    ) -> Result<Arc<Self>, CoreError> {
        spec.validate()?;
        let now = clock.now();
        let st = QueueAtomics::default();
        st.volume_bits
            .store(spec.volume.to_bits(), Ordering::Relaxed);
        let inner = QueueInner {
            segs: VecDeque::with_capacity(SEGS_INITIAL),
            queued_frames: 0,
            queued_real: 0,
            anchor: None,
            timed_end_ms: None,
            gap_reject: None,
            tags: TagTable::new(),
            rejected: RejectSet::new(),
            hold_until: None,
            gate: Prebuffer::default(),
            generation: 0,
            closed: None,
            space_wanted: 0,
            drain_waiting: false,
            idle_waiting: false,
            end_waiting: false,
            active: false,
            fresh_turn: true,
            last_activity: now,
            write_seq: 0,
            paused_user: false,
            paused_idle: true,
            track_playing: false,
            stop_sent: false,
            link_up: true,
            dead: Vec::with_capacity(GRAVE_CAP),
        };
        Ok(Arc::new(PcmQueue {
            id,
            fmt: spec.format,
            parser: PcmParser::new(spec.format),
            lim: PlaybackLimits::new(spec),
            writer: Mutex::new(Carry::new()),
            inner: Mutex::new(inner),
            st,
            send_muted,
            wake,
            clock,
            base: now,
            emit_events,
            house: Mutex::new(HouseLocal {
                spare: Vec::with_capacity(GRAVE_CAP),
                state: HouseState::default(),
            }),
            track: OnceLock::new(),
            _live: LiveCounter::new(&live::PCM_QUEUE),
        }))
    }

    fn ms(&self, frames: u64) -> u64 {
        ms_of(self.fmt, frames)
    }

    fn channels(&self) -> usize {
        usize::from(self.fmt.channels)
    }

    fn ring(&self) {
        if let Some(w) = &self.wake {
            w.notify();
        }
    }

    /// `StreamClosed(reason)` once any end flag is set.
    fn check_open(&self) -> Result<(), CoreError> {
        let flags = self.st.end_flags.load(Ordering::Acquire);
        if flags == 0 {
            return Ok(());
        }
        Err(CoreError::StreamClosed(Self::close_reason(flags)))
    }

    fn close_reason(flags: u8) -> CloseReason {
        if flags & END_CONN_CLOSED != 0 {
            CloseReason::ConnectionClosed
        } else if flags & (END_POISON | END_FAILED) != 0 {
            CloseReason::InternalError
        } else {
            CloseReason::Closed
        }
    }

    /// Tells the attached track to play or pause when the desired state changed (under L6).
    fn sync_track(&self, g: &mut QueueInner) {
        // A drain-closed queue keeps playing until it is empty.
        let desired = !g.paused_user && !g.paused_idle && g.closed != Some(CloseMode::Abort);
        if desired == g.track_playing {
            return;
        }
        if let Some(t) = self.track.get() {
            if desired {
                t.play();
            } else {
                t.pause();
            }
            g.track_playing = desired;
        }
    }

    /// Drops the carry if a `clear()` made it stale; keeps the lock-free snapshot in step.
    fn sync_carry(&self, w: &mut Carry, generation: u64) {
        w.sync_gen(generation);
        self.st
            .carry_len
            .store(u8::try_from(w.len()).unwrap_or(0), Ordering::Release);
    }

    /// All-or-nothing write (§7.3): decode + parse outside locks, L6w splice, L6 commit.
    /// `PlaybackFull` consumes nothing.
    pub fn write(
        &self,
        data: WriteData<'_>,
        meta: WriteMeta<'_>,
    ) -> Result<WriteOutcome, CoreError> {
        self.check_open()?;
        if let Some(tag) = meta.tag
            && tag.len() > MAX_TAG_LEN
        {
            return Err(CoreError::invalid("tag must be at most 128 bytes"));
        }
        let cap_bytes = self
            .lim
            .cap_frames
            .saturating_mul(self.fmt.frame_bytes() as u64);
        let ch = self.channels();
        let mut parsed = parse_write(
            &self.parser,
            data,
            self.st.carry_len.load(Ordering::Acquire),
            cap_bytes,
        )?;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let mut w = lock_checked(&self.writer);
            self.sync_carry(&mut w, self.st.generation.load(Ordering::Acquire));
            let actual = u8::try_from(w.len()).unwrap_or(0);
            if actual != parsed.carry_snapshot {
                // Another writer changed the carry since our snapshot: re-parse (§7.3 step 2).
                if attempt >= 3 {
                    parsed = parse_write(&self.parser, data, actual, cap_bytes)?;
                } else {
                    drop(w);
                    self.check_open()?;
                    parsed = parse_write(&self.parser, data, actual, cap_bytes)?;
                    continue;
                }
            }
            let saved = *w;
            let synced_gen = w.generation();
            let mut head_ok = false;
            let head = parsed.head;
            let head = head.get(..usize::from(parsed.head_len)).unwrap_or(&[]);
            if parsed.head_slot {
                if let Some(slot) = parsed.samples.get_mut(..ch) {
                    head_ok = w.splice(&self.parser, head, slot);
                }
            } else if !head.is_empty() {
                let mut scratch = [0i16; 2];
                let _ = w.splice(&self.parser, head, &mut scratch);
            }
            let mut after = After::default();
            let result = {
                let mut g = lock_checked(&self.inner);
                if g.generation != synced_gen && parsed.carry_snapshot > 0 && attempt < 3 {
                    // A clear() slipped in between L6w and L6: the carried bytes are pre-clear
                    // audio. Retry with the carry dropped.
                    let generation = g.generation;
                    drop(g);
                    *w = saved;
                    self.sync_carry(&mut w, generation);
                    drop(w);
                    self.check_open()?;
                    parsed = parse_write(&self.parser, data, 0, cap_bytes)?;
                    continue;
                }
                if g.generation != synced_gen {
                    head_ok = false;
                }
                let samples = std::mem::take(&mut parsed.samples);
                let off = if parsed.head_slot && !head_ok { ch } else { 0 };
                let frames = parsed.body_frames + u64::from(head_ok);
                let r = self.commit(&mut g, samples, off, frames, meta, &mut after);
                match r {
                    Commit::Accepted { .. } => {
                        if parsed.head_slot || parsed.carry_snapshot == 0 {
                            w.set(parsed.tail(), g.generation);
                        }
                        // Otherwise a short write extended the carry in `splice` (same generation).
                        self.st
                            .carry_len
                            .store(u8::try_from(w.len()).unwrap_or(0), Ordering::Release);
                    }
                    Commit::Rejected { .. } => {
                        w.reset();
                        self.st.carry_len.store(0, Ordering::Release);
                    }
                    Commit::Full { .. } | Commit::Closed(_) => {
                        *w = saved;
                    }
                }
                r
            };
            drop(w);
            self.finish(after);
            return match result {
                Commit::Accepted { frames, gap_frames } => {
                    Ok(WriteOutcome::Accepted { frames, gap_frames })
                }
                Commit::Rejected { frames } => Ok(WriteOutcome::Rejected { frames }),
                Commit::Full { needed_ms, free_ms } => {
                    Err(CoreError::PlaybackFull { needed_ms, free_ms })
                }
                Commit::Closed(r) => Err(CoreError::StreamClosed(r)),
            };
        }
    }

    /// Appends already-converted interleaved samples (no carry, no tag, no timestamp): the
    /// encoded-stream decode thread's push. `PlaybackFull` consumes nothing.
    pub(crate) fn write_samples(&self, samples: Box<[i16]>) -> Result<WriteOutcome, CoreError> {
        self.check_open()?;
        let ch = self.channels();
        let frames = (samples.len() / ch.max(1)) as u64;
        let mut after = After::default();
        let r = {
            let mut g = lock_checked(&self.inner);
            self.commit(&mut g, samples, 0, frames, WriteMeta::default(), &mut after)
        };
        self.finish(after);
        match r {
            Commit::Accepted { frames, gap_frames } => {
                Ok(WriteOutcome::Accepted { frames, gap_frames })
            }
            Commit::Rejected { frames } => Ok(WriteOutcome::Rejected { frames }),
            Commit::Full { needed_ms, free_ms } => {
                Err(CoreError::PlaybackFull { needed_ms, free_ms })
            }
            Commit::Closed(r) => Err(CoreError::StreamClosed(r)),
        }
    }

    /// §7.3 step 3 under L6: rejection, gap, capacity, push, start.
    fn commit(
        &self,
        g: &mut QueueInner,
        samples: Box<[i16]>,
        off: usize,
        frames: u64,
        meta: WriteMeta<'_>,
        after: &mut After,
    ) -> Commit {
        let flags = self.st.end_flags.load(Ordering::Acquire);
        if flags != 0 || g.closed.is_some() {
            after.drop_samples = Some(samples);
            return Commit::Closed(Self::close_reason(flags));
        }
        let now = self.clock.now();
        let rate = self.fmt.rate.hz();
        let dur_ms = i64::try_from(self.ms(frames)).unwrap_or(i64::MAX);
        // a. Rejection (judged against the post-clear state).
        if g.hold_until.is_some_and(|t| now >= t) {
            g.hold_until = None;
        }
        let gap_rejected = match (meta.at_ms, g.gap_reject) {
            (Some(at), Some(gr)) => gr.rejects(at),
            _ => false,
        };
        let rejected = g.hold_until.is_some()
            || meta.tag.is_some_and(|t| g.rejected.contains(t))
            || gap_rejected
            || (self.lim.when_disconnected == DisconnectPolicy::Discard && !g.link_up);
        if rejected {
            if gap_rejected && let (Some(at), Some(gr)) = (meta.at_ms, g.gap_reject.as_mut()) {
                gr.cut_end_ms = gr.cut_end_ms.max(at.saturating_add(dur_ms));
            }
            self.st.rejected.fetch_add(frames, Ordering::Relaxed);
            after.drop_samples = Some(samples);
            return Commit::Rejected { frames };
        }
        if frames == 0 {
            after.drop_samples = Some(samples);
            return Commit::Accepted {
                frames: 0,
                gap_frames: 0,
            };
        }
        // b. Gap (§7.6).
        let mut gap = 0u64;
        let mut new_anchor = g.anchor;
        if let Some(at) = meta.at_ms {
            let earliest = anchor::earliest(now, g.queued_frames, rate);
            let mut d = anchor::decide(g.anchor, at, earliest, rate, self.lim.max_gap_ms);
            if let GapDecision::Silence { frames: gf } = d
                && gf.saturating_add(frames) > self.lim.cap_frames
            {
                d = GapDecision::Reanchor;
            }
            match d {
                GapDecision::Append => {}
                GapDecision::Silence { frames: gf } => gap = gf,
                GapDecision::Reanchor => {
                    new_anchor = Some(Anchor {
                        remote_ms: at,
                        wall: earliest,
                    });
                }
            }
        }
        // c. Capacity (nothing is consumed on failure).
        let need = gap.saturating_add(frames);
        let new_segs = 1 + usize::from(gap > 0);
        if g.queued_frames.saturating_add(need) > self.lim.cap_frames
            || g.segs.len() + new_segs > self.lim.max_segs
        {
            g.space_wanted = need.min(self.lim.cap_frames);
            let free = self.lim.cap_frames.saturating_sub(g.queued_frames);
            after.drop_samples = Some(samples);
            return Commit::Full {
                needed_ms: self.ms(need),
                free_ms: self.ms(free),
            };
        }
        // d. Push.
        let queued_before = g.queued_frames;
        if gap > 0 {
            g.segs.push_back(Seg::Silence { frames: gap });
            g.queued_frames += gap;
            self.st.gap_filled.fetch_add(gap, Ordering::Relaxed);
        }
        g.write_seq += 1;
        let tag = match meta.tag {
            Some(name) => {
                let (id, evicted) = g.tags.open(name, g.write_seq);
                if evicted {
                    self.st.tags_evicted.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(e) = id.and_then(|id| g.tags.get_mut(id)) {
                    e.written += frames;
                }
                id
            }
            None => None,
        };
        g.segs.push_back(Seg::Pcm {
            data: samples,
            off: u32::try_from(off).unwrap_or(0),
            tag,
        });
        g.queued_frames += frames;
        g.queued_real += frames;
        g.anchor = new_anchor;
        if let Some(at) = meta.at_ms {
            let end = at.saturating_add(dur_ms);
            g.timed_end_ms = Some(g.timed_end_ms.map_or(end, |e| e.max(end)));
            if g.gap_reject.is_some() {
                // The first timed write after the gap ends the rejection window.
                g.gap_reject = None;
            }
        }
        self.st.written.fetch_add(frames, Ordering::Relaxed);
        self.st.queued.store(g.queued_frames, Ordering::Release);
        // e. Start.
        if queued_before == 0 && g.fresh_turn && self.lim.prebuffer_frames > 0 {
            g.gate = Prebuffer {
                armed: true,
                ticks_left: self.lim.prebuffer_ticks,
            };
        }
        g.fresh_turn = false;
        g.active = true;
        self.st.active.store(true, Ordering::Release);
        g.last_activity = now;
        if g.paused_idle {
            g.paused_idle = false;
            self.sync_track(g);
        }
        Commit::Accepted {
            frames,
            gap_frames: gap,
        }
    }

    /// Runs the side effects collected under L6 (after unlocking).
    fn finish(&self, after: After) {
        drop(after.drop_segs);
        drop(after.drop_samples);
        if after.stop
            && let Some(t) = self.track.get()
        {
            t.stop();
        }
        if after.ring {
            self.ring();
        }
    }

    /// Arms a space wake-up for `nbytes`; `true` ⇒ there is already enough space.
    pub fn arm_space(&self, nbytes: usize) -> bool {
        if self.st.end_flags.load(Ordering::Acquire) != 0 {
            return true;
        }
        let fb = self.fmt.frame_bytes().max(1);
        let needed = (nbytes.div_ceil(fb) as u64).min(self.lim.cap_frames);
        let mut g = lock_checked(&self.inner);
        let wanted = g.space_wanted.max(needed).min(self.lim.cap_frames);
        let free = self.lim.cap_frames.saturating_sub(g.queued_frames);
        if free >= wanted && g.segs.len() + 2 <= self.lim.max_segs {
            g.space_wanted = 0;
            true
        } else {
            g.space_wanted = wanted.max(1);
            false
        }
    }

    /// Releases the prebuffer gate and arms the drain wake-up; `true` ⇒ already drained (or
    /// closed: the caller then checks `closed`).
    pub fn arm_drain(&self) -> bool {
        let mut g = lock_checked(&self.inner);
        g.gate.armed = false;
        if self.st.end_flags.load(Ordering::Acquire) != 0 || g.queued_frames == 0 {
            return true;
        }
        g.drain_waiting = true;
        false
    }

    /// Arms the Active → Idle wake-up; `true` ⇒ already idle (or closed).
    pub fn arm_idle(&self) -> bool {
        let mut g = lock_checked(&self.inner);
        if !g.active || self.st.end_flags.load(Ordering::Acquire) != 0 {
            return true;
        }
        g.idle_waiting = true;
        false
    }

    /// Arms the reader-gone wake-up (`aclose(drain=True)`); `true` ⇒ already ended. A closed
    /// connection counts as ended: its tracks go with the Driver at teardown, after the
    /// housekeeper (which would ring this wake-up) has stopped.
    pub fn arm_end(&self) -> bool {
        let ended = || {
            self.st.reader_gone.load(Ordering::Acquire)
                || self.st.end_flags.load(Ordering::Acquire) & END_CONN_CLOSED != 0
        };
        if ended() {
            return true;
        }
        let mut g = lock_checked(&self.inner);
        g.end_waiting = true;
        ended()
    }

    /// Wall time of the pull with count `p` (index `p − 1`), from the pull epoch (§7.5).
    fn pull_wall(&self, p: u64) -> Instant {
        let ep = self.st.epoch_pulls.load(Ordering::Acquire);
        let eus = self.st.epoch_us.load(Ordering::Acquire);
        let epoch = self
            .base
            .checked_add(Duration::from_micros(eus))
            .unwrap_or(self.base);
        let tick = u64::from(TICK_MS);
        if ep >= p {
            let back = Duration::from_millis((ep - p).saturating_mul(tick));
            epoch.checked_sub(back).unwrap_or(epoch)
        } else {
            let fwd = Duration::from_millis((p - ep).saturating_mul(tick));
            epoch.checked_add(fwd).unwrap_or(epoch)
        }
    }

    /// Milliseconds of wall time since the pull with count `p`.
    fn since_pull_ms(&self, p: u64, now: Instant) -> u64 {
        let wall = self.pull_wall(p);
        u64::try_from(now.saturating_duration_since(wall).as_millis()).unwrap_or(u64::MAX)
    }

    /// `played − clamp(output_latency_ms − since(last_pull), 0, played)` in ms.
    fn heard_ms(&self, played_ms: u64, last_pull: Option<u64>, now: Instant) -> u64 {
        let Some(p) = last_pull else {
            return played_ms;
        };
        let since = self.since_pull_ms(p, now);
        let hold = u64::from(self.lim.output_latency_ms)
            .saturating_sub(since)
            .min(played_ms);
        played_ms - hold
    }

    fn item_of(&self, e: &super::tags::TagEntry, now: Instant) -> ItemProgressData {
        let written_ms = self.ms(e.written);
        let played_ms = self.ms(e.played.min(e.written));
        ItemProgressData {
            tag: e.name.clone(),
            written_ms,
            played_ms,
            heard_ms: self.heard_ms(played_ms, e.last_pull, now),
            complete: e.ended && e.played >= e.written,
        }
    }

    /// Barge-in (§7.7): discards everything queued now (L6 only, never L6w).
    pub fn clear(&self, opts: ClearOpts) -> Result<ClearSummary, CoreError> {
        let hold_ms = check("hold_ms", opts.hold_ms, HOLD_MS)?;
        if let Some(n) = opts.reject_until_gap_ms {
            check("reject_until_gap_ms", n, REJECT_GAP_MS)?;
        }
        let now = self.clock.now();
        let lat = u64::from(self.lim.output_latency_ms);
        let mut after = After::default();
        let (discarded, generation, items) = {
            let mut g = lock_checked(&self.inner);
            let mut items = Vec::new();
            let mut open_names: SmallVec<[Arc<str>; 8]> = SmallVec::new();
            for e in g.tags.iter() {
                let recent = e
                    .last_pull
                    .is_some_and(|p| self.since_pull_ms(p, now) < lat);
                if e.open || recent {
                    items.push(self.item_of(e, now));
                }
                if e.open {
                    open_names.push(e.name.clone());
                }
            }
            let discarded = g.queued_real;
            self.st.discarded.fetch_add(discarded, Ordering::Relaxed);
            after.drop_segs = std::mem::take(&mut g.segs);
            after.ring = g.space_wanted > 0;
            g.queued_frames = 0;
            g.queued_real = 0;
            g.space_wanted = 0;
            g.anchor = None;
            g.gate.armed = false;
            g.generation += 1;
            self.st.generation.store(g.generation, Ordering::Release);
            self.st.queued.store(0, Ordering::Release);
            if opts.reject_late {
                for name in open_names {
                    g.rejected.insert(name);
                }
                for it in &items {
                    g.rejected.insert(it.tag.clone());
                }
            }
            g.tags.close_all();
            if let (Some(n), Some(end)) = (opts.reject_until_gap_ms, g.timed_end_ms) {
                g.gap_reject = Some(GapReject {
                    cut_end_ms: end,
                    gap_ms: n,
                });
            }
            if hold_ms > 0 {
                g.hold_until = now.checked_add(Duration::from_millis(u64::from(hold_ms)));
            }
            g.fresh_turn = true;
            g.last_activity = now;
            (discarded, g.generation, items)
        };
        self.st.clears.fetch_add(1, Ordering::Relaxed);
        self.finish(after);
        let played = self.st.played_real.load(Ordering::Acquire);
        let played_ms = self.ms(played);
        let last = self.st.last_real_pull.load(Ordering::Acquire);
        Ok(ClearSummary {
            discarded_ms: self.ms(discarded),
            played_ms,
            heard_ms: self.heard_ms(played_ms, (last > 0).then_some(last), now),
            position_ms: self.ms(self.st.position.load(Ordering::Acquire)),
            generation,
            items,
        })
    }

    /// Closes the tag, sets `ended`, releases the prebuffer gate.
    pub fn end_item(&self, tag: &str) {
        let mut g = lock_checked(&self.inner);
        if let Some(id) = g.tags.find(tag)
            && let Some(e) = g.tags.get_mut(id)
        {
            e.open = false;
            e.ended = true;
        }
        g.gate.armed = false;
    }

    /// `pause()` / `resume()` (`paused_user`, plus `track.pause()` / `track.play()`).
    pub fn pause(&self, paused: bool) {
        let mut g = lock_checked(&self.inner);
        g.paused_user = paused;
        self.sync_track(&mut g);
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
        let now = self.clock.now();
        let g = lock_checked(&self.inner);
        let id = g.tags.find(tag)?;
        g.tags.get(id).map(|e| self.item_of(e, now))
    }

    pub fn stats(&self) -> PlaybackStatsData {
        let now = self.clock.now();
        let st = &self.st;
        let played = st.played_real.load(Ordering::Acquire);
        let played_ms = self.ms(played);
        let last = st.last_real_pull.load(Ordering::Acquire);
        PlaybackStatsData {
            written_ms: self.ms(st.written.load(Ordering::Acquire)),
            played_ms,
            heard_ms: self.heard_ms(played_ms, (last > 0).then_some(last), now),
            buffered_ms: self.ms(st.queued.load(Ordering::Acquire)),
            position_ms: self.ms(st.position.load(Ordering::Acquire)),
            underrun_ms: self.ms(st.underrun.load(Ordering::Acquire)),
            gap_filled_ms: self.ms(st.gap_filled.load(Ordering::Acquire)),
            rejected_ms: self.ms(st.rejected.load(Ordering::Acquire)),
            discarded_ms: self.ms(st.discarded.load(Ordering::Acquire)),
            contended_ticks: st.contended.load(Ordering::Acquire),
            clears: st.clears.load(Ordering::Acquire),
            tags_evicted: st.tags_evicted.load(Ordering::Acquire),
            idle: !st.active.load(Ordering::Acquire),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.st.end_flags.load(Ordering::Acquire) != 0
    }

    /// The owner (`native.Playback`) dropped its handle: abort, unless a close was already
    /// requested (a `close(drain=True)` in progress completes; its track ends on its own).
    pub fn abandon(&self) {
        if self.st.end_flags.load(Ordering::Acquire) == 0 {
            LiveSource::close(self, CloseMode::Abort);
        }
    }

    pub fn wake_id(&self) -> WakeId {
        self.wake.as_ref().map_or(WAKE_CONN, WakeToken::id)
    }

    /// Attaches songbird's track (or a `FakeTrackControl`), once, and brings it to the state the
    /// queue wants (a write that raced ahead of the attach resumes it now).
    #[allow(dead_code)] // called by W2's ConnShared::open_* / play (phase-2 integration)
    pub(crate) fn attach_track(&self, t: Box<dyn TrackControl>) {
        if self.track.set(t).is_err() {
            return;
        }
        let mut after = After::default();
        {
            let mut g = lock_checked(&self.inner);
            self.sync_track(&mut g);
            // Same rule as `close()`: a drain-closed track that is not playing (paused, idle or
            // never written) can never drain, so it is stopped.
            let stop = match g.closed {
                Some(CloseMode::Abort) => true,
                Some(CloseMode::Drain) => !g.track_playing,
                None => false,
            };
            if stop && !g.stop_sent {
                g.stop_sent = true;
                after.stop = true;
            }
        }
        self.finish(after);
    }

    /// The encoded decode thread failed: let the decoded audio drain, then the decoder reports an
    /// error so the track ends with `reason="error"` (§7.9). Resumes a paused track so the
    /// decoder actually runs.
    pub(crate) fn fail(&self) {
        self.st
            .end_flags
            .fetch_or(END_FAILED | END_DRAIN, Ordering::AcqRel);
        let mut after = After::default();
        {
            let mut g = lock_checked(&self.inner);
            if g.closed.is_none() {
                g.closed = Some(CloseMode::Drain);
            }
            g.gate.armed = false;
            g.paused_idle = false;
            g.paused_user = false;
            self.sync_track(&mut g);
            after.ring = std::mem::take(&mut g.space_wanted) > 0
                || std::mem::take(&mut g.drain_waiting)
                || std::mem::take(&mut g.idle_waiting);
        }
        self.finish(after);
    }

    /// Runs `f` while L6 is held (tests: mixer contention).
    #[cfg(test)]
    pub(crate) fn with_inner_locked<R>(&self, f: impl FnOnce() -> R) -> R {
        let _g = self.inner.lock();
        f()
    }

    /// Frames currently queued (lock-free mirror).
    pub(crate) fn queued_frames(&self) -> u64 {
        self.st.queued.load(Ordering::Acquire)
    }

    /// Free frames (lock-free mirror; for the encoded decode thread).
    pub(crate) fn free_frames(&self) -> u64 {
        self.lim.cap_frames.saturating_sub(self.queued_frames())
    }

    /// Whether the reader/decoder holding this queue was dropped.
    pub(crate) fn reader_gone(&self) -> bool {
        self.st.reader_gone.load(Ordering::Acquire)
    }

    /// One housekeeping pass (§7.5); events are appended to `events` and emitted by the caller.
    ///
    /// The housekeeper-only state is taken out of its (uncontended) mutex and put back afterwards,
    /// so that mutex is never held while L6 is taken: L6w → L6 stays the only nesting (§5.3).
    pub(crate) fn housekeep_into(
        &self,
        now: Instant,
        events: &mut SmallVec<[EventData; 2]>,
    ) -> HouseStatus {
        let (mut h, mut spare) = {
            let mut house = lock_checked(&self.house);
            (house.state, std::mem::take(&mut house.spare))
        };
        let status = self.housekeep_with(now, events, &mut h, &mut spare);
        // Consumed segments are freed here, on the RX thread, outside L6.
        spare.clear();
        let mut house = lock_checked(&self.house);
        house.state = h;
        house.spare = spare;
        status
    }

    fn housekeep_with(
        &self,
        now: Instant,
        events: &mut SmallVec<[EventData; 2]>,
        h: &mut HouseState,
        spare: &mut Vec<Box<[i16]>>,
    ) -> HouseStatus {
        if self.st.reader_gone.load(Ordering::Acquire) {
            if !h.ended_reported {
                h.ended_reported = true;
                let prev = self.st.end_flags.fetch_or(END_ABORT, Ordering::AcqRel);
                let reason = if prev & (END_POISON | END_FAILED) != 0 {
                    if prev & END_POISON != 0 {
                        ::log::error!(
                            "playback {}: a panic was caught on the mixer thread; the playback \
                             ended (please report this bug)",
                            self.id
                        );
                    }
                    "internal_error"
                } else if prev & END_CONN_CLOSED != 0 {
                    "connection_closed"
                } else if prev & END_ABORT != 0 {
                    "closed"
                } else if prev & END_DRAIN != 0 {
                    "drained"
                } else {
                    "stopped"
                };
                let mut after = After {
                    ring: true,
                    ..After::default()
                };
                {
                    let mut g = lock_checked(&self.inner);
                    self.abort_locked(&mut g, &mut after);
                    g.end_waiting = false;
                    std::mem::swap(&mut g.dead, spare);
                }
                self.finish(after);
                if self.emit_events {
                    events.push(EventData::PlaybackEnded {
                        playback_id: self.id,
                        reason,
                    });
                }
            }
            return HouseStatus::Finished;
        }
        let pulls = self.st.pulls.load(Ordering::Acquire);
        if pulls != h.seen_pulls {
            h.seen_pulls = pulls;
            let us = u64::try_from(now.saturating_duration_since(self.base).as_micros())
                .unwrap_or(u64::MAX);
            self.st.epoch_us.store(us, Ordering::Release);
            self.st.epoch_pulls.store(pulls, Ordering::Release);
        }
        let real_total = self.st.played_real.load(Ordering::Acquire);
        let real_advanced = real_total != h.seen_real;
        h.seen_real = real_total;
        let mut ring = false;
        {
            let mut g = lock_checked(&self.inner);
            if !g.dead.is_empty() {
                std::mem::swap(&mut g.dead, spare);
            }
            // Space.
            let free = self.lim.cap_frames.saturating_sub(g.queued_frames);
            if g.space_wanted > 0 && free >= g.space_wanted && g.segs.len() + 2 <= self.lim.max_segs
            {
                g.space_wanted = 0;
                ring = true;
            }
            // Drain.
            if g.drain_waiting && g.queued_frames == 0 {
                g.drain_waiting = false;
                ring = true;
            }
            // Started.
            if real_advanced && !h.announced {
                h.announced = true;
                if self.emit_events {
                    events.push(EventData::PlaybackStarted {
                        playback_id: self.id,
                    });
                }
            }
            // Idle.
            if g.active && g.queued_frames == 0 {
                let last_real = self.st.last_real_pull.load(Ordering::Acquire);
                let mut last = g.last_activity;
                if last_real > 0 {
                    last = last.max(self.pull_wall(last_real));
                }
                if now.saturating_duration_since(last)
                    >= Duration::from_millis(u64::from(self.lim.idle_ms))
                {
                    g.active = false;
                    self.st.active.store(false, Ordering::Release);
                    g.fresh_turn = true;
                    g.tags.close_all();
                    if h.announced {
                        h.announced = false;
                        if self.emit_events {
                            events.push(EventData::PlaybackIdle {
                                playback_id: self.id,
                                played_ms: self.ms(real_total),
                            });
                        }
                    }
                    if g.idle_waiting {
                        g.idle_waiting = false;
                        ring = true;
                    }
                    if self.lim.auto_pause && g.closed.is_none() {
                        g.paused_idle = true;
                        self.sync_track(&mut g);
                    }
                }
            }
        }
        if ring {
            self.ring();
        }
        HouseStatus::Live
    }

    /// Frees everything queued under L6 (close(Abort), reader gone, link-down discard).
    fn abort_locked(&self, g: &mut QueueInner, after: &mut After) {
        self.st
            .discarded
            .fetch_add(g.queued_real, Ordering::Relaxed);
        let old = std::mem::take(&mut g.segs);
        after.drop_segs.extend(old);
        g.queued_frames = 0;
        g.queued_real = 0;
        g.gate.armed = false;
        self.st.queued.store(0, Ordering::Release);
        if std::mem::take(&mut g.space_wanted) > 0
            || std::mem::take(&mut g.drain_waiting)
            || std::mem::take(&mut g.idle_waiting)
        {
            after.ring = true;
        }
    }

    /// Mixer pull (§7.4); see [`PcmProvider::pull`].
    fn pull_impl(&self, out: &mut [i16]) -> PullReport {
        let ch = self.channels();
        let n = self.fmt.frames_per_tick();
        let out = match out.get_mut(..n * ch) {
            Some(o) => o,
            None => out,
        };
        out.fill(0);
        let idx = self.st.pulls.load(Ordering::Relaxed);
        let mut consumed = 0u64;
        let mut real = 0u64;
        if self.send_muted.load(Ordering::Relaxed) {
            // Self-muted: hold the position, consume nothing (§7.8).
        } else if let Some(mut g) = self.inner.try_lock() {
            if g.paused_user {
                // Silence, consume nothing.
            } else if g.gate.armed
                && g.queued_frames < self.lim.prebuffer_frames
                && g.gate.ticks_left > 0
            {
                g.gate.ticks_left -= 1;
            } else {
                g.gate.armed = false;
                let (c, r) = pop_into(&mut g, out, ch, n as u64, idx + 1);
                consumed = c;
                real = r;
                if g.active {
                    self.st
                        .underrun
                        .fetch_add(n as u64 - consumed, Ordering::Relaxed);
                }
            }
            self.st.queued.store(g.queued_frames, Ordering::Release);
        } else {
            self.st.contended.fetch_add(1, Ordering::Relaxed);
        }
        self.st.pulls.store(idx + 1, Ordering::Release);
        self.st.position.fetch_add(n as u64, Ordering::Relaxed);
        if consumed > 0 {
            self.st.space_epoch.fetch_add(1, Ordering::Relaxed);
        }
        if real > 0 {
            self.st.played_real.fetch_add(real, Ordering::Release);
            self.st.last_real_pull.store(idx + 1, Ordering::Release);
        }
        PullReport {
            consumed: u32::try_from(consumed).unwrap_or(u32::MAX),
            real: u32::try_from(real).unwrap_or(u32::MAX),
        }
    }
}

/// Outcome of the L6 commit.
#[derive(Debug)]
enum Commit {
    Accepted { frames: u64, gap_frames: u64 },
    Rejected { frames: u64 },
    Full { needed_ms: u64, free_ms: u64 },
    Closed(CloseReason),
}

/// Pops up to `n` frames into `out` (already zeroed): silence segments are skipped, PCM is copied;
/// per-tag `played` and `last_pull` are updated. Consumed PCM boxes go to the graveyard. No
/// allocation.
fn pop_into(g: &mut QueueInner, out: &mut [i16], ch: usize, n: u64, pull: u64) -> (u64, u64) {
    let mut need = n;
    let mut pos = 0usize;
    let mut consumed = 0u64;
    let mut real = 0u64;
    while need > 0 {
        let Some(front) = g.segs.front_mut() else {
            break;
        };
        let exhausted = match front {
            Seg::Silence { frames } => {
                let take = (*frames).min(need);
                *frames -= take;
                need -= take;
                consumed += take;
                pos += take as usize * ch;
                *frames == 0
            }
            Seg::Pcm { data, off, tag } => {
                let o = *off as usize;
                let avail = (data.len().saturating_sub(o) / ch.max(1)) as u64;
                let take = avail.min(need);
                let len = take as usize * ch;
                if let (Some(dst), Some(src)) = (out.get_mut(pos..pos + len), data.get(o..o + len))
                {
                    dst.copy_from_slice(src);
                }
                *off = u32::try_from(o + len).unwrap_or(u32::MAX);
                pos += len;
                need -= take;
                consumed += take;
                real += take;
                if let Some(id) = *tag
                    && let Some(e) = g.tags.get_mut(id)
                {
                    e.played += take;
                    e.last_pull = Some(pull);
                }
                take == avail
            }
        };
        if exhausted
            && let Some(Seg::Pcm { data, .. }) = g.segs.pop_front()
            && g.dead.len() < g.dead.capacity()
        {
            g.dead.push(data);
        }
    }
    g.queued_frames = g.queued_frames.saturating_sub(consumed);
    g.queued_real = g.queued_real.saturating_sub(real);
    (consumed, real)
}

impl PcmProvider for PcmQueue {
    fn format(&self) -> Format {
        self.fmt
    }

    fn pull(&self, out: &mut [i16]) -> PullReport {
        self.pull_impl(out)
    }

    fn should_end(&self) -> bool {
        let flags = self.st.end_flags.load(Ordering::Acquire);
        if flags & END_NOW != 0 {
            return true;
        }
        flags & END_DRAIN != 0
            && flags & END_FAILED == 0
            && self.st.queued.load(Ordering::Acquire) == 0
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

    fn on_poison(&self) {
        self.st.end_flags.fetch_or(END_POISON, Ordering::AcqRel);
    }

    fn failed(&self) -> bool {
        self.st.end_flags.load(Ordering::Acquire) & END_FAILED != 0
            && self.st.queued.load(Ordering::Acquire) == 0
    }
}

impl LiveSource for PcmQueue {
    fn id(&self) -> u32 {
        self.id
    }

    fn housekeep(&self, now: Instant, events: &EventHub) -> HouseStatus {
        let mut evs: SmallVec<[EventData; 2]> = SmallVec::new();
        let status = self.housekeep_into(now, &mut evs);
        for ev in &evs {
            events.emit(ev);
        }
        status
    }

    fn on_link(&self, up: bool) {
        let mut after = After::default();
        {
            let mut g = lock_checked(&self.inner);
            g.link_up = up;
            if !up && self.lim.when_disconnected == DisconnectPolicy::Discard {
                self.abort_locked(&mut g, &mut after);
                g.anchor = None;
                // Like clear(): the next accepted write starts a turn (prebuffer gate re-armed).
                g.fresh_turn = true;
                g.generation += 1;
                self.st.generation.store(g.generation, Ordering::Release);
            }
        }
        self.finish(after);
    }

    fn close(&self, mode: CloseMode) {
        let bit = match mode {
            CloseMode::Drain => END_DRAIN,
            CloseMode::Abort => END_ABORT,
        };
        // Set before taking L6, so a writer that is parsing aborts at its next check.
        self.st.end_flags.fetch_or(bit, Ordering::AcqRel);
        let mut after = After::default();
        {
            let mut g = lock_checked(&self.inner);
            if g.closed == Some(CloseMode::Abort) {
                return;
            }
            g.closed = Some(mode);
            g.gate.armed = false;
            // A paused (idle or user) track is never pulled, so it cannot drain: stop it. A
            // playing one returns EOF once empty.
            let nothing_to_drain = !g.track_playing;
            if mode == CloseMode::Abort {
                self.abort_locked(&mut g, &mut after);
            } else if std::mem::take(&mut g.space_wanted) > 0 || std::mem::take(&mut g.idle_waiting)
            {
                after.ring = true;
            }
            // Without an attached track the stop is left to `attach_track` (it would be lost).
            if (mode == CloseMode::Abort || nothing_to_drain)
                && !g.stop_sent
                && self.track.get().is_some()
            {
                g.stop_sent = true;
                after.stop = true;
            }
        }
        self.finish(after);
    }

    fn conn_closed(&self) {
        self.st
            .end_flags
            .fetch_or(END_CONN_CLOSED | END_ABORT, Ordering::AcqRel);
        LiveSource::close(self, CloseMode::Abort);
        // `arm_end` now reports the stream as ended; wake a waiter armed before this.
        let ring = std::mem::take(&mut lock_checked(&self.inner).end_waiting);
        if ring {
            self.ring();
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests;
