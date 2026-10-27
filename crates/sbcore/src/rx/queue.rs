//! Subscriptions and their bounded chunk queues (W3, §8.9, §8.10).
//!
//! `SubQueue` is `Mutex<VecDeque<Delivered>>` (L4) + an armed flag + atomics, bounded by
//! `max_chunks = ⌈max_queue_ms / chunk_ms⌉` and grown lazily from `min(max_chunks, 16)`.
//! A chunk's `dropped_ms` is always the audio dropped immediately before it.
//!
//! L4 is a leaf: nothing else is locked while it is held, chunks freed by a policy are dropped
//! **after** unlocking, and the wake-up (`WakeToken::notify`) also happens after unlocking.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use super::bus::MixFilter;
use super::chunk::ChunkData;
use super::router::{RouterQueue, RouterSpec};
use crate::conn::worker::{BudgetGuard, WorkerCmd, WorkerShared};
use crate::error::{CloseReason, CoreError};
use crate::events::Pop;
use crate::format::Format;
use crate::limits::{CHUNK_MS, MAX_QUEUE_MS_MAX, check, mul_checked};
use crate::runtime::{LiveCounter, live};
use crate::wake::{WakeId, WakeToken};

/// Monotonic per-connection subscription id (never reused).
pub type SubId = u64;

/// Per-chunk metadata overhead charged against the receive budget (§8.9).
pub const CHUNK_META_BUDGET_BYTES: usize = 160;

/// Initial `VecDeque` capacity bound (§10: capacities start at `min(bound, 16)`).
const INITIAL_CAPACITY: usize = 16;

/// What happens when a subscription's queue is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overflow {
    DropOldest,
    DropNewest,
    Error,
}

/// Where a subscription's audio comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RxSource {
    User(u64),
    Mixed(MixFilter),
}

/// A subscription request (`listen_user` / `listen_mixed`).
#[derive(Clone, Debug)]
pub struct SubSpec {
    pub source: RxSource,
    pub format: Format,
    pub chunk_ms: u32,
    pub max_queue_ms: u32,
    pub overflow: Overflow,
}

/// `CHUNK_MS`, `chunk_ms ≤ max_queue_ms ≤ MAX_QUEUE_MS_MAX`, G.711 ⇒ 8 kHz mono.
pub(crate) fn validate_stream(
    fmt: Format,
    chunk_ms: u32,
    max_queue_ms: u32,
) -> Result<(), CoreError> {
    check("chunk_ms", chunk_ms, CHUNK_MS)?;
    check("max_queue_ms", max_queue_ms, chunk_ms..=MAX_QUEUE_MS_MAX)?;
    if !(1..=2).contains(&fmt.channels) {
        return Err(CoreError::invalid("channels must be 1 or 2"));
    }
    if fmt.encoding.is_g711() && (fmt.rate.hz() != 8_000 || fmt.channels != 1) {
        return Err(CoreError::invalid("mulaw/alaw require 8000 Hz mono"));
    }
    Ok(())
}

/// `⌈max_queue_ms / chunk_ms⌉ × (chunk_bytes + 160)`; validates first, `checked_mul` after.
pub(crate) fn stream_budget(
    fmt: Format,
    chunk_ms: u32,
    max_queue_ms: u32,
) -> Result<usize, CoreError> {
    validate_stream(fmt, chunk_ms, max_queue_ms)?;
    let max_chunks = u64::from(max_queue_ms.div_ceil(chunk_ms));
    let per_chunk =
        (fmt.bytes_for_ms(chunk_ms)? as u64).saturating_add(CHUNK_META_BUDGET_BYTES as u64);
    let total = mul_checked("receive_budget", max_chunks, per_chunk)?;
    usize::try_from(total).map_err(|_| CoreError::Alloc)
}

impl SubSpec {
    /// `CHUNK_MS`, `chunk_ms ≤ max_queue_ms ≤ MAX_QUEUE_MS_MAX`, G.711 ⇒ 8 kHz mono.
    pub fn validate(&self) -> Result<(), CoreError> {
        validate_stream(self.format, self.chunk_ms, self.max_queue_ms)
    }

    /// `max_chunks × (chunk_bytes + 160)`, charged against `max_receive_mb`.
    pub fn budget_bytes(&self) -> Result<usize, CoreError> {
        stream_budget(self.format, self.chunk_ms, self.max_queue_ms)
    }

    /// `⌈max_queue_ms / chunk_ms⌉` (at least 1).
    pub fn max_chunks(&self) -> usize {
        (self.max_queue_ms.div_ceil(self.chunk_ms.max(1)) as usize).max(1)
    }
}

/// One delivered chunk on the subscription's stream clock.
#[derive(Clone, Debug)]
pub struct Delivered {
    pub chunk: Arc<ChunkData>,
    /// `start_ms / chunk_ms` (jumps over dropped chunks).
    pub seq: u64,
    /// Stream clock: `chunk.timeline_ms − origin`.
    pub start_ms: u64,
    /// Audio dropped by the overflow policy immediately before this chunk.
    pub dropped_ms: u64,
}

/// `native.StreamStats` data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubStats {
    pub queued_chunks: u64,
    pub queued_ms: u64,
    pub delivered: u64,
    pub dropped_chunks: u64,
    pub dropped_ms: u64,
    pub overflowed: bool,
}

/// Mutable state of a [`SubQueue`] (under L4).
#[derive(Debug, Default)]
struct SubInner {
    items: VecDeque<Delivered>,
    armed: bool,
    closed: Option<CloseReason>,
    /// `Closed(Overflow)` was reported once as `StreamOverflow`.
    overflow_reported: bool,
    /// `drop_newest`: dropped audio attached to the next accepted chunk.
    pending_drop_ms: u64,
    /// `timeline_ms` of the stream clock origin (`start_ms == 0`).
    origin: Option<u64>,
    /// The receive-budget reservation; released once the queue is closed and empty.
    budget: Option<BudgetGuard>,
}

impl SubInner {
    /// Releases the budget once nothing can occupy the queue any more.
    fn take_budget_if_done(&mut self) -> Option<BudgetGuard> {
        if self.closed.is_some() && self.items.is_empty() {
            self.budget.take()
        } else {
            None
        }
    }

    /// The `Closed` result once drained: `Overflow` exactly once, then `Closed`.
    fn closed_pop<T>(&mut self, r: CloseReason) -> Pop<T> {
        if r == CloseReason::Overflow {
            if self.overflow_reported {
                Pop::Closed(CloseReason::Closed)
            } else {
                self.overflow_reported = true;
                Pop::Closed(CloseReason::Overflow)
            }
        } else {
            Pop::Closed(r)
        }
    }
}

/// One subscriber's bounded queue.
#[derive(Debug)]
pub struct SubQueue {
    /// L4.
    inner: Mutex<SubInner>,
    token: WakeToken,
    format: Format,
    user_id: Option<u64>,
    chunk_ms: u32,
    max_chunks: usize,
    overflow: Overflow,
    /// Mirror of `inner.closed.is_some()` for the worker's per-tick scan (no lock).
    closed_flag: AtomicBool,
    queued: AtomicU64,
    delivered: AtomicU64,
    dropped_chunks: AtomicU64,
    dropped_ms: AtomicU64,
    _live: LiveCounter,
}

/// What a `push` has to do after unlocking.
struct AfterPush {
    notify: bool,
    freed: Option<Arc<ChunkData>>,
}

impl SubQueue {
    pub fn new(spec: &SubSpec, token: WakeToken) -> Arc<Self> {
        let user_id = match spec.source {
            RxSource::User(u) => Some(u),
            RxSource::Mixed(_) => None,
        };
        let max_chunks = spec.max_chunks();
        Arc::new(SubQueue {
            inner: Mutex::new(SubInner {
                items: VecDeque::with_capacity(max_chunks.min(INITIAL_CAPACITY)),
                ..SubInner::default()
            }),
            token,
            format: spec.format,
            user_id,
            chunk_ms: spec.chunk_ms,
            max_chunks,
            overflow: spec.overflow,
            closed_flag: AtomicBool::new(false),
            queued: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
            dropped_chunks: AtomicU64::new(0),
            dropped_ms: AtomicU64::new(0),
            _live: LiveCounter::new(&live::SUB_QUEUE),
        })
    }

    /// Stores the receive-budget reservation; it is released when the queue is closed and empty.
    pub(crate) fn attach_budget(&self, b: BudgetGuard) {
        let stale = {
            let mut g = self.inner.lock();
            if g.closed.is_some() && g.items.is_empty() {
                Some(b)
            } else {
                g.budget.replace(b)
            }
        };
        drop(stale);
    }

    /// Applies the overflow policy; rings after unlocking if armed. ConnWorker only.
    pub(crate) fn push(&self, chunk: Arc<ChunkData>) {
        let dur = chunk.duration_ms();
        let timeline = chunk.meta.timeline_ms;
        let after = {
            let mut g = self.inner.lock();
            let after = self.push_locked(&mut g, chunk, dur, timeline);
            self.sync_queued(&g);
            after
        };
        drop(after.freed);
        if after.notify {
            self.token.notify();
        }
    }

    fn push_locked(
        &self,
        g: &mut SubInner,
        chunk: Arc<ChunkData>,
        dur: u64,
        timeline: u64,
    ) -> AfterPush {
        let mut after = AfterPush {
            notify: false,
            freed: None,
        };
        if g.closed.is_some() {
            after.freed = Some(chunk);
            return after;
        }
        if g.items.len() >= self.max_chunks {
            match self.overflow {
                Overflow::DropOldest => {
                    if let Some(old) = g.items.pop_front() {
                        let lost = old.dropped_ms.saturating_add(dur_of(&old));
                        self.count_drop(dur_of(&old));
                        if let Some(front) = g.items.front_mut() {
                            front.dropped_ms = front.dropped_ms.saturating_add(lost);
                        } else {
                            g.pending_drop_ms = g.pending_drop_ms.saturating_add(lost);
                        }
                        after.freed = Some(old.chunk);
                    }
                }
                Overflow::DropNewest => {
                    g.pending_drop_ms = g.pending_drop_ms.saturating_add(dur);
                    self.count_drop(dur);
                    after.freed = Some(chunk);
                    return after;
                }
                Overflow::Error => {
                    self.count_drop(dur);
                    g.closed = Some(CloseReason::Overflow);
                    self.closed_flag.store(true, Ordering::Release);
                    after.notify = std::mem::take(&mut g.armed);
                    after.freed = Some(chunk);
                    return after;
                }
            }
        }
        let origin = *g.origin.get_or_insert(timeline);
        let start_ms = timeline.saturating_sub(origin);
        let item = Delivered {
            chunk,
            seq: start_ms / u64::from(self.chunk_ms.max(1)),
            start_ms,
            dropped_ms: std::mem::take(&mut g.pending_drop_ms),
        };
        g.items.push_back(item);
        after.notify = std::mem::take(&mut g.armed);
        after
    }

    /// Mirrors the queue length into `queued` (under L4, so the lock-free snapshot is never stale
    /// after concurrent pushes and pops).
    fn sync_queued(&self, g: &SubInner) {
        self.queued.store(g.items.len() as u64, Ordering::Release);
    }

    fn count_drop(&self, ms: u64) {
        self.dropped_chunks.fetch_add(1, Ordering::AcqRel);
        self.dropped_ms.fetch_add(ms, Ordering::AcqRel);
    }

    /// Pops the next chunk, or arms the wake-up in the same critical section if empty. Queued
    /// items, then `Closed(Overflow)` exactly once (the binding raises `StreamOverflow`), then
    /// `Closed(..)` (§8.9).
    pub fn try_pop_or_arm(&self) -> Pop<Delivered> {
        self.pop(true)
    }

    /// Pops without arming (usable from any thread).
    pub fn try_pop(&self) -> Pop<Delivered> {
        self.pop(false)
    }

    fn pop(&self, arm: bool) -> Pop<Delivered> {
        let (res, budget) = {
            let mut g = self.inner.lock();
            let res = match g.items.pop_front() {
                Some(d) => Pop::Item(d),
                None => match g.closed {
                    Some(r) => g.closed_pop(r),
                    None => {
                        if arm {
                            g.armed = true;
                        }
                        Pop::Empty
                    }
                },
            };
            self.sync_queued(&g);
            (res, g.take_budget_if_done())
        };
        if matches!(res, Pop::Item(_)) {
            self.delivered.fetch_add(1, Ordering::AcqRel);
        }
        drop(budget);
        res
    }

    /// Moves up to `max` chunks into `out`. Returns the close reason only when nothing was moved
    /// and the queue is closed and drained (`Overflow` once, then `Closed`, as for pops).
    pub fn drain(&self, max: usize, out: &mut Vec<Delivered>) -> Option<CloseReason> {
        let (res, budget, moved) = {
            let mut g = self.inner.lock();
            let n = max.min(g.items.len());
            let before = out.len();
            if out.try_reserve(n).is_ok() {
                out.extend(g.items.drain(..n));
            }
            let moved = out.len() - before;
            let res = match (moved, g.closed) {
                (0, Some(r)) if g.items.is_empty() => match g.closed_pop::<()>(r) {
                    Pop::Closed(r) => Some(r),
                    _ => None,
                },
                _ => None,
            };
            self.sync_queued(&g);
            (res, g.take_budget_if_done(), moved)
        };
        self.delivered.fetch_add(moved as u64, Ordering::AcqRel);
        drop(budget);
        res
    }

    /// Drops everything queued; returns the milliseconds dropped (not counted as `dropped_ms`).
    /// `reset_clock`: the next chunk has `start_ms == 0` and `seq == 0`. The pending drop counter is
    /// cleared too.
    pub fn skip_to_live(&self, reset_clock: bool) -> u64 {
        let (freed, budget) = {
            let mut g = self.inner.lock();
            let freed = std::mem::take(&mut g.items);
            g.pending_drop_ms = 0;
            if reset_clock {
                g.origin = None;
            }
            self.sync_queued(&g);
            (freed, g.take_budget_if_done())
        };
        let ms = freed.iter().map(dur_of).sum();
        drop(freed);
        drop(budget);
        ms
    }

    /// `timeline_ms` of the clock origin; `None` until the first chunk after a reset.
    pub fn origin_timeline_ms(&self) -> Option<u64> {
        self.inner.lock().origin
    }

    /// Closes the queue (first reason wins); queued chunks stay readable until drained. Wakes an
    /// armed consumer.
    pub fn close(&self, r: CloseReason) {
        self.close_impl(r, false);
    }

    /// Closes the queue and frees every queued chunk (explicit `close()` / drop of the Python
    /// object: nobody will read them). First close wins: on a queue that is already closed (by
    /// T6, `UserLeft`, an overflow) it does nothing, so the chunks that close left readable stay
    /// readable (`SongbirdClient.cleanup()` closes every child stream after the connection closed,
    /// §6.5, and queued chunks must still drain, §4.9).
    pub fn close_and_clear(&self, r: CloseReason) {
        self.close_impl(r, true);
    }

    fn close_impl(&self, r: CloseReason, clear: bool) {
        let (notify, freed, budget) = {
            let mut g = self.inner.lock();
            let first = g.closed.is_none();
            if first {
                g.closed = Some(r);
            }
            let freed = if clear && first {
                std::mem::take(&mut g.items)
            } else {
                VecDeque::new()
            };
            // Under L4: `is_closed()` and `close_reason()` never disagree once this returns.
            self.closed_flag.store(true, Ordering::Release);
            self.sync_queued(&g);
            (std::mem::take(&mut g.armed), freed, g.take_budget_if_done())
        };
        drop(freed);
        drop(budget);
        if notify {
            self.token.notify();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed_flag.load(Ordering::Acquire)
    }

    /// The close reason, once closed.
    pub fn close_reason(&self) -> Option<CloseReason> {
        self.inner.lock().closed
    }

    pub fn stats(&self) -> SubStats {
        let (queued, overflowed) = {
            let g = self.inner.lock();
            (
                g.items.len() as u64,
                g.closed == Some(CloseReason::Overflow),
            )
        };
        SubStats {
            queued_chunks: queued,
            // Every chunk of a subscription lasts exactly `chunk_ms` (O(1) under L4).
            queued_ms: queued.saturating_mul(u64::from(self.chunk_ms)),
            delivered: self.delivered.load(Ordering::Acquire),
            dropped_chunks: self.dropped_chunks.load(Ordering::Acquire),
            dropped_ms: self.dropped_ms.load(Ordering::Acquire),
            overflowed,
        }
    }

    pub fn wake_id(&self) -> WakeId {
        self.token.id()
    }

    /// The stream format.
    pub fn format(&self) -> Format {
        self.format
    }

    /// The user of a per-user stream; `None` for mixed streams.
    pub fn user_id(&self) -> Option<u64> {
        self.user_id
    }

    pub fn chunk_ms(&self) -> u32 {
        self.chunk_ms
    }

    /// The queue bound in chunks.
    pub fn max_chunks(&self) -> usize {
        self.max_chunks
    }

    /// Chunks currently queued (lock-free snapshot).
    pub fn queued_chunks(&self) -> u64 {
        self.queued.load(Ordering::Acquire)
    }

    /// Bytes currently queued (payload + `CHUNK_META_BUDGET_BYTES` per chunk), as charged against
    /// the receive budget. O(queued chunks) under L4: diagnostics and tests only.
    pub fn queued_bytes(&self) -> usize {
        let g = self.inner.lock();
        g.items
            .iter()
            .map(|d| d.chunk.data.len() + CHUNK_META_BUDGET_BYTES)
            .sum()
    }

    /// Current `VecDeque` capacity (tests: lazy growth).
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.inner.lock().items.capacity()
    }
}

fn dur_of(d: &Delivered) -> u64 {
    d.chunk.duration_ms()
}

/// Commands for the receive pipeline (via `WorkerCmd::Rx`).
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum RxCmd {
    AddSub {
        id: SubId,
        spec: SubSpec,
        queue: Arc<SubQueue>,
    },
    RemoveSub(SubId),
    SetFilter(SubId, MixFilter),
    AddRouter {
        id: u64,
        spec: RouterSpec,
        queue: Arc<RouterQueue>,
    },
    RemoveRouter(u64),
    SetBots(Vec<(u64, bool)>),
}

/// What `native.ReceiveSubscription` holds. Python owns it; the worker owns the only other `Arc`
/// of the queue. Drop ⇒ close.
#[derive(Debug)]
pub struct Subscription {
    pub id: SubId,
    pub queue: Arc<SubQueue>,
    worker: Weak<WorkerShared>,
    /// `RemoveSub` was pushed (close is idempotent).
    removed: AtomicBool,
}

impl Subscription {
    /// The budget reservation moves into the queue: it is released as soon as the queue is closed
    /// and empty (explicit close, drop, connection teardown, or drained after an overflow).
    pub(crate) fn new(
        id: SubId,
        queue: Arc<SubQueue>,
        worker: Weak<WorkerShared>,
        budget: BudgetGuard,
    ) -> Self {
        queue.attach_budget(budget);
        Subscription {
            id,
            queue,
            worker,
            removed: AtomicBool::new(false),
        }
    }

    /// A subscription with no worker and no budget (a pipeline driven without a `WorkerShared`,
    /// e.g. unit tests). The pipeline still unregisters it once its queue is closed.
    pub(crate) fn detached(id: SubId, queue: Arc<SubQueue>) -> Self {
        Subscription {
            id,
            queue,
            worker: Weak::new(),
            removed: AtomicBool::new(false),
        }
    }

    /// Mixed streams only: moves the subscription to the lane of `f` at its first complete chunk.
    pub fn set_filter(&self, f: MixFilter) -> Result<(), CoreError> {
        if self.queue.user_id().is_some() {
            return Err(CoreError::invalid(
                "set_include/set_exclude apply to mixed streams only",
            ));
        }
        if let Some(r) = self.queue.close_reason() {
            return Err(CoreError::StreamClosed(r));
        }
        match self.worker.upgrade() {
            Some(w) => {
                w.push(WorkerCmd::Rx(RxCmd::SetFilter(self.id, f)));
                Ok(())
            }
            None => Err(CoreError::StreamClosed(CloseReason::ConnectionClosed)),
        }
    }

    /// Closes the queue (freeing queued chunks) and unregisters at the next tick. Idempotent.
    pub fn close(&self) {
        self.queue.close_and_clear(CloseReason::Closed);
        if !self.removed.swap(true, Ordering::AcqRel)
            && let Some(w) = self.worker.upgrade()
        {
            w.push(WorkerCmd::Rx(RxCmd::RemoveSub(self.id)));
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // §4.10: in a forked child never take a lock a vanished parent thread may hold (the
        // worker's, a Python thread's): the queue is simply freed with its last reference.
        if crate::runtime::check_not_forked().is_err() {
            return;
        }
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::Level;
    use crate::format::{Encoding, PCM16_24K, PcmBuf};
    use crate::rx::chunk::ChunkMeta;
    use crate::rx::test_util::{hub, ready, token};
    use std::time::Instant;

    fn chunk(timeline_ms: u64, chunk_ms: u32) -> Arc<ChunkData> {
        let fmt = PCM16_24K;
        Arc::new(ChunkData {
            meta: ChunkMeta {
                format: fmt,
                user_id: None,
                timeline_ms,
                frames: u32::try_from(fmt.frames_for_ms(chunk_ms)).unwrap(),
                speaking: false,
                voiced_ms: 0,
                concealed_ms: 0,
                gap_ms: 0,
                speakers: Box::new([]),
                level: Level::default(),
                captured_at: Instant::now(),
            },
            data: PcmBuf::zeroed(fmt.bytes_for_ms(chunk_ms).unwrap()).unwrap(),
        })
    }

    fn spec(max_queue_ms: u32, overflow: Overflow) -> SubSpec {
        SubSpec {
            source: RxSource::Mixed(MixFilter::everyone()),
            format: PCM16_24K,
            chunk_ms: 20,
            max_queue_ms,
            overflow,
        }
    }

    fn pop_all(q: &SubQueue) -> Vec<Delivered> {
        let mut v = Vec::new();
        while let Pop::Item(d) = q.try_pop() {
            v.push(d);
        }
        v
    }

    #[test]
    fn validate_and_budget() {
        let mut s = spec(2_000, Overflow::DropOldest);
        s.validate().unwrap();
        // 100 chunks × (960 + 160).
        assert_eq!(s.budget_bytes().unwrap(), 100 * 1_120);
        for (c, m) in [(0, 2_000), (1_001, 2_000), (20, 10), (20, 60_001)] {
            s.chunk_ms = c;
            s.max_queue_ms = m;
            assert!(matches!(s.validate(), Err(CoreError::Limit(_))), "{c}/{m}");
            assert!(s.budget_bytes().is_err());
        }
        s.chunk_ms = 1_000;
        s.max_queue_ms = 60_000;
        s.format = Format::new(48_000, 2, Encoding::F32Le).unwrap();
        assert_eq!(s.budget_bytes().unwrap(), 60 * (384_000 + 160));
        s.format = Format {
            rate: crate::format::SampleRate::Hz16000,
            channels: 1,
            encoding: Encoding::MuLaw,
        };
        assert!(matches!(s.validate(), Err(CoreError::InvalidAudio(_))));
    }

    #[test]
    fn stream_clock_and_seq() {
        let (h, _r) = hub();
        let q = SubQueue::new(&spec(1_000, Overflow::DropOldest), token(&h));
        assert_eq!(q.origin_timeline_ms(), None);
        for t in [500u64, 520, 540] {
            q.push(chunk(t, 20));
        }
        assert_eq!(q.origin_timeline_ms(), Some(500));
        let v = pop_all(&q);
        assert_eq!(
            v.iter().map(|d| (d.seq, d.start_ms)).collect::<Vec<_>>(),
            [(0, 0), (1, 20), (2, 40)]
        );
        assert_eq!(q.stats().delivered, 3);
    }

    #[test]
    fn drop_oldest_moves_dropped_ms_to_the_new_front() {
        let (h, _r) = hub();
        let q = SubQueue::new(&spec(60, Overflow::DropOldest), token(&h)); // 3 chunks
        for i in 0..5u64 {
            q.push(chunk(i * 20, 20));
        }
        let s = q.stats();
        assert_eq!(
            (s.queued_chunks, s.dropped_chunks, s.dropped_ms),
            (3, 2, 40)
        );
        let v = pop_all(&q);
        assert_eq!(v[0].start_ms, 40);
        assert_eq!(v[0].seq, 2, "seq jumps over the dropped chunks");
        assert_eq!(v[0].dropped_ms, 40, "the hole right before this chunk");
        assert_eq!(v[1].dropped_ms, 0);
        // Compensating every dropped_ms keeps the stream clock exact.
        let mut sent = 0;
        for d in &v {
            sent += d.dropped_ms;
            assert_eq!(sent, d.start_ms);
            sent += d.chunk.duration_ms();
        }
    }

    #[test]
    fn drop_newest_attaches_to_the_next_accepted_chunk() {
        let (h, _r) = hub();
        let q = SubQueue::new(&spec(40, Overflow::DropNewest), token(&h)); // 2 chunks
        for i in 0..4u64 {
            q.push(chunk(i * 20, 20));
        }
        assert_eq!(q.stats().dropped_ms, 40);
        let v = pop_all(&q);
        assert_eq!(v.iter().map(|d| d.start_ms).collect::<Vec<_>>(), [0, 20]);
        q.push(chunk(80, 20));
        let v = pop_all(&q);
        assert_eq!((v[0].start_ms, v[0].dropped_ms), (80, 40));
    }

    #[test]
    fn error_policy_reports_overflow_once_after_the_queue() {
        let (h, _r) = hub();
        let q = SubQueue::new(&spec(40, Overflow::Error), token(&h));
        for i in 0..3u64 {
            q.push(chunk(i * 20, 20));
        }
        assert!(q.is_closed());
        assert!(q.stats().overflowed);
        assert!(matches!(q.try_pop_or_arm(), Pop::Item(_)));
        assert!(matches!(q.try_pop_or_arm(), Pop::Item(_)));
        assert!(matches!(
            q.try_pop_or_arm(),
            Pop::Closed(CloseReason::Overflow)
        ));
        assert!(matches!(
            q.try_pop_or_arm(),
            Pop::Closed(CloseReason::Closed)
        ));
        assert!(matches!(q.try_pop(), Pop::Closed(CloseReason::Closed)));
        assert_eq!(q.close_reason(), Some(CloseReason::Overflow));
        // Later pushes are discarded.
        q.push(chunk(100, 20));
        assert_eq!(q.stats().queued_chunks, 0);
    }

    #[test]
    fn drain_reports_close_only_when_nothing_moved() {
        let (h, _r) = hub();
        let q = SubQueue::new(&spec(40, Overflow::Error), token(&h));
        for i in 0..3u64 {
            q.push(chunk(i * 20, 20));
        }
        let mut out = Vec::new();
        assert_eq!(q.drain(50, &mut out), None);
        assert_eq!(out.len(), 2);
        out.clear();
        assert_eq!(q.drain(50, &mut out), Some(CloseReason::Overflow));
        assert_eq!(q.drain(50, &mut out), Some(CloseReason::Closed));
        assert!(out.is_empty());
    }

    #[test]
    fn arm_notify_and_close_wake() {
        let (h, _r) = hub();
        let q = SubQueue::new(&spec(1_000, Overflow::DropOldest), token(&h));
        let id = q.wake_id().0;
        // Not armed: no wake-up.
        q.push(chunk(0, 20));
        assert!(ready(&h).is_empty());
        assert!(matches!(q.try_pop_or_arm(), Pop::Item(_)));
        assert!(matches!(q.try_pop_or_arm(), Pop::Empty));
        q.push(chunk(20, 20));
        assert_eq!(ready(&h), [id]);
        q.push(chunk(40, 20)); // disarmed by the first push
        assert!(ready(&h).is_empty());
        pop_all(&q);
        assert!(matches!(q.try_pop_or_arm(), Pop::Empty));
        q.close(CloseReason::ConnectionClosed);
        assert_eq!(ready(&h), [id], "close wakes an armed consumer");
        assert!(matches!(
            q.try_pop_or_arm(),
            Pop::Closed(CloseReason::ConnectionClosed)
        ));
    }

    /// §13.1: arm/notify race, 2 threads × 10⁵: every chunk is observed and no wake-up is lost.
    #[test]
    fn arm_notify_race() {
        let (h, _r) = hub();
        let q = SubQueue::new(&spec(60_000, Overflow::DropOldest), token(&h));
        const N: u64 = 100_000;
        let producer = {
            let q = q.clone();
            std::thread::spawn(move || {
                for i in 0..N {
                    q.push(chunk(i * 20, 20));
                    if i % 64 == 0 {
                        std::thread::yield_now();
                    }
                }
            })
        };
        let mut got = 0u64;
        let mut next_start = 0u64;
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while got < N {
            match q.try_pop_or_arm() {
                Pop::Item(d) => {
                    // drop_oldest may evict under pressure; then dropped_ms covers the hole.
                    assert_eq!(d.start_ms, next_start + d.dropped_ms);
                    next_start = d.start_ms + 20;
                    got += 1 + d.dropped_ms / 20;
                }
                Pop::Empty => {
                    // Armed: the producer must ring before there is anything new to pop.
                    let mut ids = Vec::new();
                    loop {
                        h.take_ready(&mut ids);
                        if !ids.is_empty() {
                            break;
                        }
                        assert!(Instant::now() < deadline, "lost wake-up");
                        std::thread::yield_now();
                    }
                }
                Pop::Closed(_) => unreachable!(),
            }
        }
        producer.join().unwrap();
        assert_eq!(got, N);
    }

    /// The lock-free `queued_chunks()` mirror is updated under L4: once concurrent pushes and pops
    /// are done it equals the real queue length (no stale store from a racing thread).
    #[test]
    fn queued_snapshot_is_exact_after_concurrent_push_and_pop() {
        let (h, _r) = hub();
        for round in 0..300u64 {
            let q = SubQueue::new(&spec(60_000, Overflow::DropOldest), token(&h));
            const N: u64 = 64;
            let producer = {
                let q = q.clone();
                std::thread::spawn(move || {
                    for i in 0..N {
                        q.push(chunk(i * 20, 20));
                    }
                })
            };
            let mut got = 0;
            while got < N {
                if let Pop::Item(_) = q.try_pop() {
                    got += 1;
                    assert!(q.queued_chunks() <= N);
                }
            }
            producer.join().unwrap();
            assert_eq!(q.queued_chunks(), 0, "round {round}");
            assert_eq!(q.stats().queued_chunks, 0);
            q.close(CloseReason::Closed);
            assert!(q.is_closed());
            assert_eq!(q.close_reason(), Some(CloseReason::Closed));
        }
    }

    #[test]
    fn capacity_grows_lazily() {
        let (h, _r) = hub();
        let q = SubQueue::new(&spec(60_000, Overflow::DropOldest), token(&h));
        assert!(q.capacity() <= 16);
        for i in 0..16u64 {
            q.push(chunk(i * 20, 20));
        }
        assert!(q.capacity() <= 16, "no growth until needed");
        q.push(chunk(320, 20));
        assert!(q.capacity() > 16);
        let small = SubQueue::new(&spec(40, Overflow::DropOldest), token(&h));
        assert!(small.capacity() >= 2 && small.capacity() < 16);
    }

    #[test]
    fn skip_to_live_and_clock_reset() {
        let (h, _r) = hub();
        let q = SubQueue::new(&spec(60, Overflow::DropNewest), token(&h));
        for i in 0..5u64 {
            q.push(chunk(1_000 + i * 20, 20));
        }
        assert_eq!(q.skip_to_live(false), 60);
        assert_eq!(q.origin_timeline_ms(), Some(1_000));
        q.push(chunk(1_100, 20));
        let d = pop_all(&q);
        assert_eq!((d[0].start_ms, d[0].seq, d[0].dropped_ms), (100, 5, 0));
        q.push(chunk(1_120, 20));
        assert_eq!(q.skip_to_live(true), 20);
        assert_eq!(q.origin_timeline_ms(), None);
        q.push(chunk(1_200, 20));
        let d = pop_all(&q);
        assert_eq!((d[0].start_ms, d[0].seq), (0, 0));
        assert_eq!(q.origin_timeline_ms(), Some(1_200));
        assert_eq!(
            q.stats().dropped_ms,
            40,
            "skip_to_live is not reported as dropped"
        );
    }

    #[test]
    fn close_and_clear_frees_and_detached_subscription_closes_on_drop() {
        let (h, _r) = hub();
        let q = SubQueue::new(&spec(1_000, Overflow::DropOldest), token(&h));
        q.push(chunk(0, 20));
        let c = chunk(20, 20);
        q.push(c.clone());
        assert_eq!(Arc::strong_count(&c), 2);
        let sub = Subscription::detached(1, q.clone());
        assert!(sub.set_filter(MixFilter::everyone()).is_err(), "no worker");
        drop(sub);
        assert!(q.is_closed());
        assert_eq!(Arc::strong_count(&c), 1, "queued chunks freed immediately");
        assert!(matches!(q.try_pop(), Pop::Closed(CloseReason::Closed)));
        // A per-user stream rejects set_filter.
        let mut s = spec(1_000, Overflow::DropOldest);
        s.source = RxSource::User(9);
        let sub = Subscription::detached(2, SubQueue::new(&s, token(&h)));
        assert!(matches!(
            sub.set_filter(MixFilter::everyone()),
            Err(CoreError::InvalidAudio(_))
        ));
    }

    /// First close wins: a stream T6 closed keeps its chunks readable when the consumer's
    /// `close()` (or `SongbirdClient.cleanup()`) follows (§4.9, §6.5).
    #[test]
    fn closing_an_already_closed_queue_keeps_its_chunks() {
        let (h, _r) = hub();
        let q = SubQueue::new(&spec(1_000, Overflow::DropOldest), token(&h));
        q.push(chunk(0, 20));
        q.push(chunk(20, 20));
        q.close(CloseReason::ConnectionClosed);
        let sub = Subscription::detached(1, q.clone());
        sub.close();
        assert_eq!(q.close_reason(), Some(CloseReason::ConnectionClosed));
        assert!(matches!(q.try_pop(), Pop::Item(_)));
        assert!(matches!(q.try_pop(), Pop::Item(_)));
        assert!(matches!(
            q.try_pop(),
            Pop::Closed(CloseReason::ConnectionClosed)
        ));
    }
}
