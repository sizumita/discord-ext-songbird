//! Voice events and bounded per-subscriber event queues (W2, §8.11, §12.3).
//!
//! [`EventHub::emit`] pushes into every [`EventQueue`] whose [`KindMask`] matches. Queues are
//! bounded (`EVENT_QUEUE`, drop-oldest) and report drops with a synthetic `Lagged` event.
//! `Disconnected { will_retry: false }` is always the last event before the queues close
//! ([`EventHub::close_all`], T6).
//!
//! Locking: `EventHub.queues` (L5h) → `EventQueue.inner` (L5q) is the only nesting. Wake-ups are
//! rung after both locks are released (§5.3).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use smallvec::SmallVec;

use crate::error::{CloseReason, CoreError};
use crate::limits::{EVENT_QUEUE, MAX_EVENT_STREAMS_PER_CONN, check};
use crate::runtime::{LiveCounter, live};
use crate::tracks::flags::TrackEndReason;
use crate::wake::{WakeId, WakeToken};

/// One voice event. The variant order defines the [`KindMask`] bit of each kind (bit 0 =
/// `Connected`, …, bit 12 = `Lagged`).
#[derive(Clone, Debug)]
pub enum EventData {
    Connected {
        channel_id: u64,
        ssrc: u32,
        timeline_ms: u64,
    },
    Reconnected {
        channel_id: u64,
        ssrc: u32,
        gap_ms: u64,
    },
    Disconnected {
        reason: &'static str,
        code: Option<u16>,
        will_retry: bool,
    },
    UserMapped {
        user_id: u64,
        ssrc: u32,
    },
    UserLeft {
        user_id: u64,
    },
    SpeakingStarted {
        user_id: u64,
        timeline_ms: u64,
        captured_at: Instant,
    },
    SpeakingStopped {
        user_id: u64,
        timeline_ms: u64,
        duration_ms: u64,
    },
    PlaybackStarted {
        playback_id: u32,
    },
    PlaybackIdle {
        playback_id: u32,
        played_ms: u64,
    },
    PlaybackEnded {
        playback_id: u32,
        reason: &'static str,
    },
    TrackEnded {
        track_id: u64,
        reason: TrackEndReason,
    },
    ReceiveStalled {
        since_ms: u64,
    },
    Lagged {
        dropped: u64,
    },
}

/// Number of event kinds (bits used in a [`KindMask`]).
pub const EVENT_KINDS: u32 = 13;

/// Bit set of event kinds a subscription wants (bit `i` = the `i`-th [`EventData`] variant).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindMask(pub u16);

impl KindMask {
    /// Every kind.
    pub const ALL: Self = KindMask((1u16 << EVENT_KINDS) - 1);
    /// The `Lagged` bit.
    pub const LAGGED: u16 = 1 << 12;

    /// The single bit of `ev`'s kind.
    pub fn of(ev: &EventData) -> u16 {
        let index: u32 = match ev {
            EventData::Connected { .. } => 0,
            EventData::Reconnected { .. } => 1,
            EventData::Disconnected { .. } => 2,
            EventData::UserMapped { .. } => 3,
            EventData::UserLeft { .. } => 4,
            EventData::SpeakingStarted { .. } => 5,
            EventData::SpeakingStopped { .. } => 6,
            EventData::PlaybackStarted { .. } => 7,
            EventData::PlaybackIdle { .. } => 8,
            EventData::PlaybackEnded { .. } => 9,
            EventData::TrackEnded { .. } => 10,
            EventData::ReceiveStalled { .. } => 11,
            EventData::Lagged { .. } => 12,
        };
        1u16 << index
    }

    /// Whether `ev`'s kind is selected.
    pub fn matches(self, ev: &EventData) -> bool {
        self.0 & KindMask::of(ev) != 0
    }
}

/// Result of a non-blocking pop from a consumer queue.
#[derive(Debug)]
pub enum Pop<T> {
    Item(T),
    Empty,
    Closed(CloseReason),
}

/// Registry of event queues (L5h → L5q).
#[derive(Debug, Default)]
pub struct EventHub {
    /// L5h. Closed queues are pruned on every `emit` and `subscribe`.
    queues: Mutex<Vec<Arc<EventQueue>>>,
    /// Set by `close_all` (under L5h); later `emit`s and `subscribe`s are refused.
    closed: AtomicBool,
}

/// Queues to ring after the hub lock is released (≤ `MAX_EVENT_STREAMS_PER_CONN`, no heap).
type RingList = SmallVec<[Arc<EventQueue>; MAX_EVENT_STREAMS_PER_CONN]>;

impl EventHub {
    pub fn new() -> Arc<Self> {
        Arc::new(EventHub::default())
    }

    /// Pushes `ev` into every matching queue. NET/RX/Python threads only; a no-op once closed.
    /// Allocation-free (events hold no heap data; the ring list is inline).
    pub fn emit(&self, ev: &EventData) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let mut ring: RingList = SmallVec::new();
        {
            let mut queues = self.queues.lock(); // L5h
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            queues.retain(|q| !q.is_closed());
            for q in queues.iter() {
                if q.kinds.matches(ev) && q.push(ev.clone()) {
                    ring.push(q.clone());
                }
            }
        } // L5h released
        for q in ring {
            q.token.notify();
        }
    }

    /// Registers a new bounded queue (`cap` within `EVENT_QUEUE`, at most
    /// `MAX_EVENT_STREAMS_PER_CONN` per connection).
    pub fn subscribe(
        &self,
        cap: usize,
        kinds: KindMask,
        token: WakeToken,
    ) -> Result<Arc<EventQueue>, CoreError> {
        let cap = check("max_queue", cap, EVENT_QUEUE)?;
        let mut queues = self.queues.lock(); // L5h
        if self.closed.load(Ordering::Acquire) {
            return Err(CoreError::ConnectionClosed(CloseReason::ConnectionClosed));
        }
        queues.retain(|q| !q.is_closed());
        check(
            "event_streams",
            queues.len().saturating_add(1),
            1..=MAX_EVENT_STREAMS_PER_CONN,
        )?;
        let q = Arc::new(EventQueue::new(cap, kinds, token));
        queues.push(q.clone());
        Ok(q)
    }

    /// One L5h section: mark closed, append `last` (if any) to every queue, close every queue (T6).
    /// `last` is appended even if the queue's mask does not select its kind, and even if the queue
    /// is full (the oldest event is dropped), so it is always the final event.
    pub fn close_all(&self, last: Option<&EventData>) {
        let mut ring: RingList = SmallVec::new();
        let taken: Vec<Arc<EventQueue>> = {
            let mut queues = self.queues.lock(); // L5h
            self.closed.store(true, Ordering::Release);
            for q in queues.iter() {
                if q.close_with_last(last.cloned(), CloseReason::ConnectionClosed) {
                    ring.push(q.clone());
                }
            }
            std::mem::take(&mut *queues)
        };
        for q in ring {
            q.token.notify();
        }
        drop(taken);
    }

    /// Whether `close_all` ran.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Registered queues that are still open (diagnostics and tests).
    pub fn open_queues(&self) -> usize {
        self.queues.lock().iter().filter(|q| !q.is_closed()).count()
    }
}

/// Mutable state of an [`EventQueue`] (under L5q).
#[derive(Debug)]
struct EventInner {
    items: VecDeque<EventData>,
    armed: bool,
    closed: Option<CloseReason>,
    /// Drops not yet reported by a `Lagged` event.
    lagged: u64,
}

/// One subscriber's bounded (drop-oldest) event queue.
#[derive(Debug)]
pub struct EventQueue {
    /// L5q.
    inner: Mutex<EventInner>,
    token: WakeToken,
    kinds: KindMask,
    cap: usize,
    dropped: AtomicU64,
    /// Mirror of `inner.closed.is_some()` for lock-free pruning.
    closed_flag: AtomicBool,
    _live: LiveCounter,
}

impl EventQueue {
    fn new(cap: usize, kinds: KindMask, token: WakeToken) -> Self {
        EventQueue {
            inner: Mutex::new(EventInner {
                // Lazily grown: never `with_capacity(user_value)` (§10).
                items: VecDeque::with_capacity(cap.min(16)),
                armed: false,
                closed: None,
                lagged: 0,
            }),
            token,
            kinds,
            cap,
            dropped: AtomicU64::new(0),
            closed_flag: AtomicBool::new(false),
            _live: LiveCounter::new(&live::EVENT_QUEUE),
        }
    }

    /// Appends one event (drop-oldest). Returns whether the consumer was armed (ring after unlock).
    fn push(&self, ev: EventData) -> bool {
        let mut g = self.inner.lock(); // L5q
        if g.closed.is_some() {
            return false;
        }
        if g.items.len() >= self.cap {
            g.items.pop_front();
            g.lagged = g.lagged.saturating_add(1);
            self.dropped.fetch_add(1, Ordering::AcqRel);
        }
        g.items.push_back(ev);
        std::mem::replace(&mut g.armed, false)
    }

    /// T6: appends `last` (dropping the oldest event if full) and closes with `reason`. Returns
    /// whether the consumer was armed.
    fn close_with_last(&self, last: Option<EventData>, reason: CloseReason) -> bool {
        let mut g = self.inner.lock(); // L5q
        if g.closed.is_some() {
            return false;
        }
        if let Some(ev) = last {
            if g.items.len() >= self.cap {
                g.items.pop_front();
                g.lagged = g.lagged.saturating_add(1);
                self.dropped.fetch_add(1, Ordering::AcqRel);
            }
            g.items.push_back(ev);
        }
        g.closed = Some(reason);
        self.closed_flag.store(true, Ordering::Release);
        // Always ring: the consumer must observe the close even if it was not armed.
        g.armed = false;
        true
    }

    fn pop_inner(&self, arm: bool) -> Pop<EventData> {
        let mut g = self.inner.lock(); // L5q
        if g.lagged > 0 && !g.items.is_empty() && self.kinds.0 & KindMask::LAGGED != 0 {
            let dropped = std::mem::take(&mut g.lagged);
            return Pop::Item(EventData::Lagged { dropped });
        }
        if let Some(ev) = g.items.pop_front() {
            if g.items.is_empty() {
                g.lagged = 0;
            }
            return Pop::Item(ev);
        }
        g.lagged = 0;
        match g.closed {
            Some(r) => Pop::Closed(r),
            None => {
                if arm {
                    g.armed = true;
                }
                Pop::Empty
            }
        }
    }

    /// Pops the next event, or arms the wake-up in the same critical section if empty. After drops,
    /// a synthetic `Lagged { dropped }` precedes the next delivered event (if the mask selects
    /// `Lagged`).
    pub fn try_pop_or_arm(&self) -> Pop<EventData> {
        self.pop_inner(true)
    }

    /// Pops without arming (usable from any thread).
    pub fn try_pop(&self) -> Pop<EventData> {
        self.pop_inner(false)
    }

    /// Closes the queue at the subscriber's request (`EventStream.close()`, `Drop`) and frees the
    /// queued events now. Idempotent. The hub prunes it on its next `emit`/`subscribe`.
    pub fn close(&self) {
        let freed = {
            let mut g = self.inner.lock();
            if g.closed.is_none() {
                g.closed = Some(CloseReason::Closed);
            }
            self.closed_flag.store(true, Ordering::Release);
            g.lagged = 0;
            std::mem::take(&mut g.items)
        };
        drop(freed);
    }

    /// Whether the queue is closed (queued events may still be readable).
    pub fn is_closed(&self) -> bool {
        self.closed_flag.load(Ordering::Acquire)
    }

    /// Total events dropped by the overflow policy.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }

    pub fn wake_id(&self) -> WakeId {
        self.token.id()
    }

    /// Queued events (tests and diagnostics).
    pub fn len(&self) -> usize {
        self.inner.lock().items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake::{Doorbell, WakeHub};
    use std::os::unix::net::UnixStream;

    fn hub_with_wake() -> (Arc<EventHub>, Arc<WakeHub>, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        let wake = WakeHub::new(Doorbell::from_unix_stream(a).unwrap());
        (EventHub::new(), wake, b)
    }

    fn ev(n: u64) -> EventData {
        EventData::UserLeft { user_id: n }
    }

    fn user(p: Pop<EventData>) -> Option<u64> {
        match p {
            Pop::Item(EventData::UserLeft { user_id }) => Some(user_id),
            _ => None,
        }
    }

    #[test]
    fn kind_mask_bits() {
        assert_eq!(KindMask::ALL.0, 0x1FFF);
        assert_eq!(
            KindMask::of(&EventData::Lagged { dropped: 1 }),
            KindMask::LAGGED
        );
        let m = KindMask(KindMask::of(&ev(1)));
        assert!(m.matches(&ev(2)));
        assert!(!m.matches(&EventData::ReceiveStalled { since_ms: 0 }));
    }

    #[test]
    fn emit_filters_by_kind_and_rings_armed_consumers() {
        let (hub, wake, _peer) = hub_with_wake();
        let all = hub
            .subscribe(8, KindMask::ALL, wake.alloc().unwrap())
            .unwrap();
        let only_left = hub
            .subscribe(8, KindMask(KindMask::of(&ev(0))), wake.alloc().unwrap())
            .unwrap();
        assert!(matches!(all.try_pop_or_arm(), Pop::Empty));
        hub.emit(&EventData::ReceiveStalled { since_ms: 5 });
        hub.emit(&ev(7));
        let mut ready = Vec::new();
        wake.take_ready(&mut ready);
        assert_eq!(ready, vec![all.wake_id().0]);
        assert!(matches!(
            all.try_pop(),
            Pop::Item(EventData::ReceiveStalled { since_ms: 5 })
        ));
        assert_eq!(user(all.try_pop()), Some(7));
        assert_eq!(user(only_left.try_pop()), Some(7));
        assert!(matches!(only_left.try_pop(), Pop::Empty));
    }

    #[test]
    fn overflow_drops_oldest_and_reports_lagged_first() {
        let (hub, wake, _peer) = hub_with_wake();
        let q = hub
            .subscribe(3, KindMask::ALL, wake.alloc().unwrap())
            .unwrap();
        for i in 0..5 {
            hub.emit(&ev(i));
        }
        assert_eq!(q.dropped(), 2);
        assert!(matches!(
            q.try_pop(),
            Pop::Item(EventData::Lagged { dropped: 2 })
        ));
        assert_eq!(user(q.try_pop()), Some(2));
        assert_eq!(user(q.try_pop()), Some(3));
        assert_eq!(user(q.try_pop()), Some(4));
        assert!(matches!(q.try_pop(), Pop::Empty));
        // Lagged is not synthesised for a mask without the Lagged bit.
        let q2 = hub
            .subscribe(1, KindMask(KindMask::of(&ev(0))), wake.alloc().unwrap())
            .unwrap();
        hub.emit(&ev(10));
        hub.emit(&ev(11));
        assert_eq!(q2.dropped(), 1);
        assert_eq!(user(q2.try_pop()), Some(11));
    }

    #[test]
    fn close_all_appends_final_event_last_and_closes() {
        let (hub, wake, _peer) = hub_with_wake();
        let q = hub
            .subscribe(2, KindMask(KindMask::of(&ev(0))), wake.alloc().unwrap())
            .unwrap();
        hub.emit(&ev(1));
        hub.emit(&ev(2));
        let last = EventData::Disconnected {
            reason: "requested",
            code: None,
            will_retry: false,
        };
        hub.close_all(Some(&last));
        // Later emits are dropped; subscribe is refused.
        hub.emit(&ev(3));
        assert!(
            hub.subscribe(4, KindMask::ALL, wake.alloc().unwrap())
                .is_err()
        );
        let mut seen = Vec::new();
        loop {
            match q.try_pop() {
                Pop::Item(e) => seen.push(e),
                Pop::Empty => panic!("closed queue reported empty"),
                Pop::Closed(r) => {
                    assert_eq!(r, CloseReason::ConnectionClosed);
                    break;
                }
            }
        }
        // Full queue: the oldest was dropped to make room for the final event, which is last.
        assert!(matches!(
            seen.last(),
            Some(EventData::Disconnected {
                will_retry: false,
                ..
            })
        ));
        assert_eq!(seen.len(), 2);
        assert!(q.is_closed());
    }

    #[test]
    fn subscriber_close_frees_and_hub_prunes() {
        let (hub, wake, _peer) = hub_with_wake();
        let mut qs = Vec::new();
        for _ in 0..MAX_EVENT_STREAMS_PER_CONN {
            qs.push(
                hub.subscribe(4, KindMask::ALL, wake.alloc().unwrap())
                    .unwrap(),
            );
        }
        assert!(matches!(
            hub.subscribe(4, KindMask::ALL, wake.alloc().unwrap()),
            Err(CoreError::Limit(_))
        ));
        hub.emit(&ev(1));
        qs[0].close();
        assert!(qs[0].is_empty());
        assert!(matches!(qs[0].try_pop(), Pop::Closed(CloseReason::Closed)));
        // A closed queue no longer counts towards the limit.
        let extra = hub.subscribe(4, KindMask::ALL, wake.alloc().unwrap());
        assert!(extra.is_ok());
        hub.emit(&ev(2));
        assert_eq!(hub.open_queues(), MAX_EVENT_STREAMS_PER_CONN);
    }

    #[test]
    fn subscribe_validates_capacity() {
        let (hub, wake, _peer) = hub_with_wake();
        assert!(
            hub.subscribe(0, KindMask::ALL, wake.alloc().unwrap())
                .is_err()
        );
        assert!(
            hub.subscribe(4097, KindMask::ALL, wake.alloc().unwrap())
                .is_err()
        );
        assert!(
            hub.subscribe(4096, KindMask::ALL, wake.alloc().unwrap())
                .is_ok()
        );
    }

    #[test]
    fn emit_and_pop_from_many_threads() {
        let (hub, wake, _peer) = hub_with_wake();
        let q = hub
            .subscribe(4096, KindMask::ALL, wake.alloc().unwrap())
            .unwrap();
        let producers: Vec<_> = (0..4)
            .map(|t| {
                let hub = hub.clone();
                std::thread::spawn(move || {
                    for i in 0..500 {
                        hub.emit(&ev(t * 1000 + i));
                    }
                })
            })
            .collect();
        let mut got = 0;
        let start = Instant::now();
        while got < 2000 && start.elapsed().as_secs() < 10 {
            if let Pop::Item(_) = q.try_pop() {
                got += 1;
            }
        }
        for p in producers {
            p.join().unwrap();
        }
        while let Pop::Item(_) = q.try_pop() {
            got += 1;
        }
        assert_eq!(got, 2000);
        assert_eq!(q.dropped(), 0);
    }
}
