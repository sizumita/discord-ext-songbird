//! The sequential track queue (W2, §7.10). Only the current entry is handed to songbird; pending
//! entries (≤ `MAX_QUEUE_ENTRIES`) hold their source spec, options and flags. Not gapless: the
//! housekeeper detects the end by polling and then starts the next entry.
//!
//! Locking: the queue state is one L10 leaf lock. Advancing pops under it, releases it, and only
//! then builds the input and calls `play` under L1.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;

use super::flags::{
    DoneGuard, PendingChange, PlayOpts, TrackEndReason, TrackFlags, TrackRef, loop_state,
};
use super::registry::TrackRegistry;
use crate::conn::LogLimiter;
use crate::conn::shared::ConnShared;
use crate::conn::worker::WorkerShared;
use crate::error::{CloseReason, CoreError};
use crate::limits::{MAX_QUEUE_ENTRIES, VOLUME, check};
use crate::sync::lock_checked;
use crate::tx::encoded::EncodedShared;
use crate::tx::source::SourceSpec;

/// Entries that may fail to start in one `advance` before yielding to the next tick.
const ADVANCE_BUDGET: usize = 64;

static START_FAIL_LOG: LogLimiter = LogLimiter::per_minute();

/// One pending entry.
#[derive(Debug)]
pub(crate) struct QueueEntry {
    src: SourceSpec,
    opts: PlayOpts,
    track: TrackRef,
    done: DoneGuard,
}

/// Changes to the current entry made while it was being started (applied right after).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Late {
    paused: Option<bool>,
    volume: Option<f32>,
    loops: Option<Option<u32>>,
}

impl Late {
    fn record(&mut self, c: PendingChange) {
        match c {
            PendingChange::Paused(p) => self.paused = Some(p),
            PendingChange::Volume(v) => self.volume = Some(v),
            PendingChange::Loops(l) => self.loops = Some(l),
        }
    }

    fn apply(self, t: &TrackRef) {
        if let Some(p) = self.paused {
            let _ = if p { t.pause() } else { t.resume() };
        }
        if let Some(v) = self.volume {
            let _ = t.set_volume(v);
        }
        if let Some(l) = self.loops {
            let _ = t.set_loops(l);
        }
    }
}

/// The playing entry.
struct Current {
    track: TrackRef,
    /// Popped but not yet handed to songbird.
    starting: bool,
    late: Late,
    /// A lazy encoded entry's stream, until Python takes it (`queue_take_stream`).
    stream: Option<Arc<EncodedShared>>,
}

#[derive(Default)]
struct QState {
    items: VecDeque<QueueEntry>,
    current: Option<Current>,
}

/// Sequential queue. Index 0 is the first **pending** entry; the current track is `current()`.
#[derive(Default)]
pub struct TrackQueue {
    /// L10.
    state: Mutex<QState>,
    paused: AtomicBool,
    reg: OnceLock<Weak<TrackRegistry>>,
    waker: OnceLock<Weak<WorkerShared>>,
}

impl std::fmt::Debug for TrackQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackQueue")
            .field("pending", &self.len())
            .field("paused", &self.paused.load(Ordering::Relaxed))
            .finish()
    }
}

impl TrackQueue {
    pub fn new() -> Self {
        TrackQueue::default()
    }

    /// Connects the queue to its registry (TrackEnded reports for removed pending entries) and to
    /// the worker (advancing). Called once by `ConnShared::new`.
    pub(crate) fn attach(&self, reg: Weak<TrackRegistry>, waker: Weak<WorkerShared>) {
        let _ = self.reg.set(reg);
        let _ = self.waker.set(waker);
    }

    fn wake(&self) {
        if let Some(w) = self.waker.get().and_then(Weak::upgrade) {
            w.wake();
        }
    }

    /// Reports resolved pending entries (`TrackEnded`) through the registry's poll list.
    fn report(&self, resolved: &[Arc<TrackFlags>]) {
        if resolved.is_empty() {
            return;
        }
        if let Some(reg) = self.reg.get().and_then(Weak::upgrade) {
            for f in resolved {
                reg.watch(f);
            }
        }
        self.wake();
    }

    /// Adds a pending entry (`LimitExceeded` beyond `MAX_QUEUE_ENTRIES`). Nothing is handed to
    /// songbird (and a lazy `Encoded` entry allocates nothing) until the entry becomes current.
    pub fn enqueue(
        &self,
        src: SourceSpec,
        opts: PlayOpts,
        reg: &TrackRegistry,
    ) -> Result<TrackRef, CoreError> {
        check("volume", opts.volume, VOLUME)?;
        loop_state(opts.loops)?;
        let track = {
            let mut st = lock_checked(&self.state);
            // Checked under the queue lock: T8's `close_all` drains under it after T0 set
            // `closing`, so it either already ran (and `closing` is visible here) or will resolve
            // this entry. Checked before the lock, an entry could be added after T8 and never end.
            if reg.is_closing() {
                return Err(CoreError::ConnectionClosed(CloseReason::ConnectionClosed));
            }
            check(
                "queue_entries",
                st.items.len().saturating_add(1),
                1..=MAX_QUEUE_ENTRIES,
            )?;
            let (track, done) = reg.register(src.kind());
            st.items.push_back(QueueEntry {
                src,
                opts,
                track: track.clone(),
                done,
            });
            track
        };
        self.wake();
        Ok(track)
    }

    /// The playing entry.
    pub fn current(&self) -> Option<TrackRef> {
        lock_checked(&self.state)
            .current
            .as_ref()
            .map(|c| c.track.clone())
    }

    /// Pending entries (index 0 = next up).
    pub fn snapshot(&self) -> Vec<TrackRef> {
        lock_checked(&self.state)
            .items
            .iter()
            .map(|e| e.track.clone())
            .collect()
    }

    /// Number of pending entries.
    pub fn len(&self) -> usize {
        lock_checked(&self.state).items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the queue is paused (advancing blocked).
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Stops the current track (resolved `stopped` at once, so the queue advances even while the
    /// mixer is parked), then advances.
    pub fn skip(&self) {
        let cur = lock_checked(&self.state)
            .current
            .as_ref()
            .map(|c| c.track.clone());
        if let Some(t) = cur {
            let _ = t.stop();
            t.flags.resolve(TrackEndReason::Stopped, None);
            self.report(std::slice::from_ref(&t.flags));
        }
        self.wake();
    }

    /// `clear()` then `skip()`.
    pub fn stop(&self) {
        self.clear();
        self.skip();
    }

    /// The current track to control directly, or `None` when there is none or it is being
    /// started right now (then `c` is recorded and applied as soon as it has started).
    fn current_or_record(&self, c: PendingChange) -> Option<TrackRef> {
        let mut st = lock_checked(&self.state);
        let cur = st.current.as_mut()?;
        if cur.starting {
            cur.late.record(c);
            return None;
        }
        Some(cur.track.clone())
    }

    /// Pauses the current track and blocks advancing.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        if let Some(t) = self.current_or_record(PendingChange::Paused(true)) {
            let _ = t.pause();
        }
    }

    /// Resumes the current track and allows advancing.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        if let Some(t) = self.current_or_record(PendingChange::Paused(false)) {
            let _ = t.resume();
        }
        self.wake();
    }

    /// Pending entries end with `stopped`; the current track keeps playing.
    pub fn clear(&self) {
        let drained: Vec<QueueEntry> = lock_checked(&self.state).items.drain(..).collect();
        let flags: Vec<Arc<TrackFlags>> = drained
            .iter()
            .filter(|e| e.track.flags.resolve(TrackEndReason::Stopped, None))
            .map(|e| e.track.flags.clone())
            .collect();
        drop(drained);
        self.report(&flags);
    }

    /// Removes pending entry `index` and marks it `stopped`.
    pub fn dequeue(&self, index: usize) -> Option<TrackRef> {
        let entry = lock_checked(&self.state).items.remove(index)?;
        let track = entry.track.clone();
        drop(entry);
        track.flags.resolve(TrackEndReason::Stopped, None);
        self.report(std::slice::from_ref(&track.flags));
        Some(track)
    }

    /// Removes the pending entry of `t` and marks it `stopped` (`TrackHandle.stop()` on a pending
    /// entry); `false` if `t` is not pending. The current entry while it is being started counts as
    /// pending: it is marked `stopped` now and `advance` stops it right after it started.
    pub fn remove(&self, t: &TrackRef) -> bool {
        let entry = {
            let mut st = lock_checked(&self.state);
            match st.items.iter().position(|e| e.track.same(t)) {
                Some(i) => st.items.remove(i),
                None => {
                    let starting = st
                        .current
                        .as_ref()
                        .is_some_and(|c| c.starting && c.track.same(t));
                    drop(st);
                    if !starting {
                        return false;
                    }
                    None
                }
            }
        };
        drop(entry);
        t.flags.resolve(TrackEndReason::Stopped, None);
        self.report(std::slice::from_ref(&t.flags));
        true
    }

    /// Pending entries: pause/resume/volume/loops update the stored `PlayOpts` (`Ok(false)` = not
    /// pending; use the handle). A change to an entry that is being started right now is applied
    /// as soon as it has started.
    pub fn update_pending(&self, t: &TrackRef, c: PendingChange) -> Result<bool, CoreError> {
        match c {
            PendingChange::Volume(v) => {
                check("volume", v, VOLUME)?;
            }
            PendingChange::Loops(l) => {
                if t.flags.kind.is_live() {
                    return Err(CoreError::Track("live sources cannot loop".into()));
                }
                loop_state(l)?;
            }
            PendingChange::Paused(_) => {}
        }
        let mut st = lock_checked(&self.state);
        if let Some(e) = st.items.iter_mut().find(|e| e.track.same(t)) {
            c.apply_to(&mut e.opts);
            return Ok(true);
        }
        if let Some(cur) = st.current.as_mut()
            && cur.starting
            && cur.track.same(t)
        {
            cur.late.record(c);
            return Ok(true);
        }
        Ok(false)
    }

    /// The stored options of a pending entry (tests and diagnostics).
    pub fn pending_opts(&self, t: &TrackRef) -> Option<PlayOpts> {
        lock_checked(&self.state)
            .items
            .iter()
            .find(|e| e.track.same(t))
            .map(|e| e.opts)
    }

    /// The encoded stream of the current lazy entry `t`, once (`queue_take_stream`).
    pub(crate) fn take_stream(&self, t: &TrackRef) -> Option<Arc<EncodedShared>> {
        let mut st = lock_checked(&self.state);
        match st.current.as_mut() {
            Some(c) if c.track.same(t) => c.stream.take(),
            _ => None,
        }
    }

    /// Whether the housekeeper should call `advance` (the current entry ended or none is playing,
    /// and entries are pending).
    pub(crate) fn needs_advance(&self) -> bool {
        if self.is_paused() {
            return false;
        }
        let st = lock_checked(&self.state);
        let free = st
            .current
            .as_ref()
            .is_none_or(|c| !c.starting && c.track.flags.ended().is_some());
        free && (!st.items.is_empty() || st.current.is_some())
    }

    /// Advances when the current track resolved and the queue is not paused (pop under L10,
    /// release, then `play` under L1). The current entry counts towards
    /// `MAX_PLAYING_TRACKS_PER_CONN`: while that many `play()` tracks are playing, the next entry
    /// waits.
    pub(crate) fn advance(&self, conn: &ConnShared) {
        for _ in 0..ADVANCE_BUDGET {
            if self.is_paused() || conn.is_closed() {
                return;
            }
            // Taken before the queue lock (the registry's list is another L10 leaf) and held until
            // the entry is started, so a concurrent `play()` cannot overshoot the limit either.
            let slot = conn.reserve_playing();
            let (entry, finished) = {
                let mut st = lock_checked(&self.state);
                if let Some(cur) = &st.current
                    && (cur.starting || cur.track.flags.ended().is_none())
                {
                    return;
                }
                if st.items.is_empty() {
                    let finished = st.current.take();
                    drop(st);
                    drop(finished);
                    return;
                }
                if slot.is_err() {
                    // MAX_PLAYING_TRACKS_PER_CONN tracks are playing: the next entry waits.
                    return;
                }
                let Some(e) = st.items.pop_front() else {
                    return;
                };
                let finished = st.current.replace(Current {
                    track: e.track.clone(),
                    starting: true,
                    late: Late::default(),
                    stream: None,
                });
                (e, finished)
            };
            drop(finished);
            let QueueEntry {
                src,
                opts,
                track,
                done,
            } = entry;
            let started = conn.start_queue_entry(&src, opts, &track, done);
            drop(slot); // the started entry is visible to `TrackRegistry::playing` now
            let (late, has_stream) = {
                let mut st = lock_checked(&self.state);
                match st.current.as_mut() {
                    Some(c) if c.track.same(&track) => {
                        c.starting = false;
                        if let Ok(stream) = &started {
                            c.stream = stream.clone();
                        }
                        (std::mem::take(&mut c.late), c.stream.is_some())
                    }
                    _ => (Late::default(), false),
                }
            };
            match started {
                Ok(_) => {
                    late.apply(&track);
                    if track.flags.ended().is_some() {
                        // Skipped or stopped while it was being started.
                        let _ = track.stop();
                    }
                    if has_stream {
                        conn.notify_tracks();
                    }
                    return;
                }
                Err(e) => {
                    if START_FAIL_LOG.allow() {
                        ::log::warn!("queue entry {} failed to start: {e}", track.flags.id);
                    }
                    if conn.is_closed() {
                        // Closed while starting: T8 resolves it (or already did).
                        track.flags.resolve(TrackEndReason::Disconnected, None);
                        return;
                    }
                    let msg = e.to_string();
                    track.flags.resolve(TrackEndReason::Error, Some(&msg));
                    self.report(std::slice::from_ref(&track.flags));
                }
            }
        }
    }

    /// T8: every pending entry and the current one end with `r`; their sources are freed now.
    pub(crate) fn close_all(&self, r: TrackEndReason) {
        let (drained, current) = {
            let mut st = lock_checked(&self.state);
            let drained: Vec<QueueEntry> = st.items.drain(..).collect();
            (drained, st.current.take())
        };
        for e in &drained {
            e.track.flags.resolve(r, None);
        }
        if let Some(c) = &current {
            c.track.flags.resolve(r, None);
        }
        drop(drained);
        drop(current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::encoded::EncodedSpec;
    use crate::tx::source::EncodedFactory;
    use std::sync::atomic::AtomicBool;

    fn spec() -> SourceSpec {
        SourceSpec::Encoded(EncodedFactory {
            spec: EncodedSpec {
                max_buffer_bytes: 1 << 20,
                volume: 1.0,
            },
        })
    }

    fn setup() -> (TrackQueue, Arc<TrackRegistry>, Arc<AtomicBool>) {
        let closing = Arc::new(AtomicBool::new(false));
        let reg = Arc::new(TrackRegistry::new(closing.clone()));
        let q = TrackQueue::new();
        q.attach(Arc::downgrade(&reg), Weak::new());
        (q, reg, closing)
    }

    #[test]
    fn index_zero_is_the_first_pending_entry() {
        let (q, reg, _) = setup();
        let a = q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        let b = q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        assert_eq!(q.len(), 2);
        assert!(q.current().is_none());
        let snap = q.snapshot();
        assert!(snap[0].same(&a) && snap[1].same(&b));
        assert!(q.dequeue(2).is_none(), "out of range");
        let got = q.dequeue(1).unwrap();
        assert!(got.same(&b));
        assert_eq!(b.flags.ended(), Some(TrackEndReason::Stopped));
        assert_eq!(q.len(), 1);
        // The removal is reported as TrackEnded.
        let mut out = Vec::new();
        reg.poll_ended(0, &mut out);
        assert_eq!(out, vec![(b.flags.id, TrackEndReason::Stopped)]);
    }

    #[test]
    fn pending_changes_update_stored_options() {
        let (q, reg, _) = setup();
        let t = q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        assert!(!t.is_started());
        assert!(q.update_pending(&t, PendingChange::Paused(true)).unwrap());
        assert!(q.update_pending(&t, PendingChange::Volume(0.5)).unwrap());
        let o = q.pending_opts(&t).unwrap();
        assert!(o.paused);
        assert_eq!(o.volume, 0.5);
        assert!(matches!(
            q.update_pending(&t, PendingChange::Volume(9.0)),
            Err(CoreError::Limit(_))
        ));
        // Encoded entries are live sources: no loops.
        assert!(matches!(
            q.update_pending(&t, PendingChange::Loops(None)),
            Err(CoreError::Track(_))
        ));
        let (other, _g) = reg.register(crate::tracks::TrackKind::File);
        assert!(
            !q.update_pending(&other, PendingChange::Paused(true))
                .unwrap()
        );
    }

    #[test]
    fn remove_clear_and_stop_resolve_stopped() {
        let (q, reg, _) = setup();
        let a = q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        let b = q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        let c = q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        assert!(q.remove(&b));
        assert!(!q.remove(&b));
        q.clear();
        assert!(q.is_empty());
        for t in [&a, &b, &c] {
            assert_eq!(t.flags.ended(), Some(TrackEndReason::Stopped));
        }
        let mut out = Vec::new();
        reg.poll_ended(0, &mut out);
        assert_eq!(out.len(), 3);
        let d = q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        q.stop();
        assert_eq!(d.flags.ended(), Some(TrackEndReason::Stopped));
        assert!(q.is_empty());
    }

    #[test]
    fn the_10001st_entry_is_rejected_and_lazy_entries_spawn_nothing() {
        let (q, reg, _) = setup();
        let workers =
            crate::tx::encoded::ENCODED_WORKERS.load(std::sync::atomic::Ordering::Acquire);
        for _ in 0..MAX_QUEUE_ENTRIES {
            q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        }
        assert!(matches!(
            q.enqueue(spec(), PlayOpts::default(), &reg),
            Err(CoreError::Limit(_))
        ));
        assert_eq!(q.len(), MAX_QUEUE_ENTRIES);
        assert!(
            crate::tx::encoded::ENCODED_WORKERS.load(std::sync::atomic::Ordering::Acquire)
                <= workers,
            "a queued lazy entry holds no decode thread"
        );
        assert!(!reg.needs_poll(), "pending entries are not polled");
        q.clear();
    }

    #[test]
    fn enqueue_validates_and_refuses_after_close() {
        let (q, reg, closing) = setup();
        let bad = PlayOpts {
            volume: -1.0,
            ..PlayOpts::default()
        };
        assert!(q.enqueue(spec(), bad, &reg).is_err());
        let bad_loops = PlayOpts {
            loops: Some(u32::MAX),
            ..PlayOpts::default()
        };
        assert!(q.enqueue(spec(), bad_loops, &reg).is_err());
        closing.store(true, std::sync::atomic::Ordering::Release);
        assert!(matches!(
            q.enqueue(spec(), PlayOpts::default(), &reg),
            Err(CoreError::ConnectionClosed(_))
        ));
    }

    #[test]
    fn close_all_resolves_everything() {
        let (q, reg, _) = setup();
        let a = q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        q.close_all(TrackEndReason::Disconnected);
        assert_eq!(a.flags.ended(), Some(TrackEndReason::Disconnected));
        assert!(q.is_empty());
    }

    /// Puts the first pending entry into the "being started" window of `advance`.
    fn begin_start(q: &TrackQueue) -> TrackRef {
        let mut st = q.state.lock();
        let e = st.items.pop_front().unwrap();
        st.current = Some(Current {
            track: e.track.clone(),
            starting: true,
            late: Late::default(),
            stream: None,
        });
        e.track
    }

    #[test]
    fn stop_while_the_entry_is_being_started_is_not_lost() {
        let (q, reg, _) = setup();
        let t = q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        let cur = begin_start(&q);
        assert!(cur.same(&t));
        // `NativeTrack.stop()` sees a track that is neither pending nor started.
        assert!(q.remove(&t), "the starting entry counts as pending");
        assert_eq!(t.flags.ended(), Some(TrackEndReason::Stopped));
        let mut out = Vec::new();
        reg.poll_ended(0, &mut out);
        assert_eq!(out, vec![(t.flags.id, TrackEndReason::Stopped)]);
        // Not the current entry, or no longer starting: not removable here.
        let other = q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        q.state.lock().items.clear();
        assert!(!q.remove(&other));
        q.state.lock().current.as_mut().unwrap().starting = false;
        assert!(!q.remove(&t));
    }

    #[test]
    fn queue_pause_while_the_entry_is_being_started_applies_after_start() {
        let (q, reg, _) = setup();
        let t = q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        begin_start(&q);
        q.pause();
        assert!(q.is_paused());
        let late = q.state.lock().current.as_ref().unwrap().late;
        assert_eq!(late.paused, Some(true), "recorded for after the start");
        q.resume();
        let late = q.state.lock().current.as_ref().unwrap().late;
        assert_eq!(late.paused, Some(false));
        assert!(!t.is_started());
    }

    #[test]
    fn pause_blocks_advancing() {
        let (q, reg, _) = setup();
        q.enqueue(spec(), PlayOpts::default(), &reg).unwrap();
        assert!(q.needs_advance());
        q.pause();
        assert!(!q.needs_advance());
        q.resume();
        assert!(q.needs_advance());
        q.clear();
        assert!(!q.needs_advance());
    }
}
