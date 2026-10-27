//! The `listen_users` router (W3, §8.9): on a user's first voiced tick that passes the filters, the
//! worker creates a per-user subscription and pushes it here (cap `ROUTER_QUEUE_CAP`, drop-oldest
//! closes the dropped stream).
//!
//! The router lock is a leaf: subscriptions it evicts or discards are dropped (closed) only after
//! unlocking, and `close(true)` closes the streams it created after unlocking too.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use super::queue::{
    Overflow, RxSource, SubQueue, SubSpec, Subscription, stream_budget, validate_stream,
};
use crate::error::{CloseReason, CoreError};
use crate::events::Pop;
use crate::format::Format;
use crate::limits::ROUTER_QUEUE_CAP;
use crate::wake::{WakeId, WakeToken};

/// A router request.
#[derive(Clone, Debug)]
pub struct RouterSpec {
    pub format: Format,
    pub chunk_ms: u32,
    pub max_queue_ms: u32,
    pub overflow: Overflow,
    /// `None` = everyone.
    pub include: Option<BTreeSet<u64>>,
    pub ignore_bots: bool,
    /// Close a user's stream with `"user_left"` on `ClientDisconnect`.
    pub close_on_leave: bool,
}

impl RouterSpec {
    /// The same stream rules as a subscription (`SubSpec::validate`).
    pub fn validate(&self) -> Result<(), CoreError> {
        validate_stream(self.format, self.chunk_ms, self.max_queue_ms)
    }

    /// Budget of **one** stream the router creates (each is reserved when it is created).
    pub fn stream_budget_bytes(&self) -> Result<usize, CoreError> {
        stream_budget(self.format, self.chunk_ms, self.max_queue_ms)
    }

    /// Whether `user` gets a stream (include filter and bots).
    pub fn selects(&self, user: u64, bots: &HashSet<u64>) -> bool {
        self.include.as_ref().is_none_or(|inc| inc.contains(&user))
            && !(self.ignore_bots && bots.contains(&user))
    }

    /// The per-user subscription the router creates for `user`.
    pub fn sub_spec(&self, user: u64) -> SubSpec {
        SubSpec {
            source: RxSource::User(user),
            format: self.format,
            chunk_ms: self.chunk_ms,
            max_queue_ms: self.max_queue_ms,
            overflow: self.overflow,
        }
    }
}

/// Mutable state of a [`RouterQueue`].
#[derive(Debug, Default)]
struct RouterInner {
    items: VecDeque<Subscription>,
    armed: bool,
    closed: bool,
    /// Every stream this router created (for `close(close_streams=true)`); pruned on insert.
    created: Vec<Weak<SubQueue>>,
}

/// Queue of newly created per-user subscriptions.
#[derive(Debug)]
pub struct RouterQueue {
    inner: Mutex<RouterInner>,
    token: WakeToken,
    dropped: AtomicU64,
    closed_flag: AtomicBool,
}

impl RouterQueue {
    pub fn new(token: WakeToken) -> Arc<Self> {
        Arc::new(RouterQueue {
            inner: Mutex::new(RouterInner {
                items: VecDeque::with_capacity(16),
                ..RouterInner::default()
            }),
            token,
            dropped: AtomicU64::new(0),
            closed_flag: AtomicBool::new(false),
        })
    }

    /// Pops the next stream, or arms the wake-up if empty.
    pub fn try_pop_or_arm(&self) -> Pop<Subscription> {
        let mut g = self.inner.lock();
        match g.items.pop_front() {
            Some(s) => Pop::Item(s),
            None if g.closed => Pop::Closed(CloseReason::Closed),
            None => {
                g.armed = true;
                Pop::Empty
            }
        }
    }

    /// Adds a stream the worker created. Beyond `ROUTER_QUEUE_CAP` the oldest pending stream is
    /// dropped (closed) and counted; after `close` the new stream is closed at once.
    pub(crate) fn push(&self, sub: Subscription) {
        let (notify, discard) = {
            let mut g = self.inner.lock();
            if g.closed {
                (false, Some(sub))
            } else {
                g.created.retain(|w| w.strong_count() > 0);
                g.created.push(Arc::downgrade(&sub.queue));
                let evicted = if g.items.len() >= ROUTER_QUEUE_CAP {
                    self.dropped.fetch_add(1, Ordering::AcqRel);
                    g.items.pop_front()
                } else {
                    None
                };
                g.items.push_back(sub);
                (std::mem::take(&mut g.armed), evicted)
            }
        };
        drop(discard); // Subscription::drop closes it (outside the router lock)
        if notify {
            self.token.notify();
        }
    }

    /// Closes the router; `close_streams` also closes every stream it created. Pending streams
    /// nobody received are always closed.
    ///
    /// The first close decides the fate of the streams already received: after
    /// `close(close_streams=false)` the router forgets them, so a later `close(true)` (e.g. the
    /// `RouterHandle` drop) does not close streams the caller chose to keep.
    pub fn close(&self, close_streams: bool) {
        let (notify, pending, created) = {
            let mut g = self.inner.lock();
            g.closed = true;
            // Taken in both cases: with `close_streams == false` the weak handles are dropped.
            let created = std::mem::take(&mut g.created);
            let created = if close_streams { created } else { Vec::new() };
            (
                std::mem::take(&mut g.armed),
                std::mem::take(&mut g.items),
                created,
            )
        };
        self.closed_flag.store(true, Ordering::Release);
        drop(pending);
        for q in created.iter().filter_map(Weak::upgrade) {
            q.close(CloseReason::Closed);
        }
        if notify {
            self.token.notify();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed_flag.load(Ordering::Acquire)
    }

    /// Streams discarded because the router queue overflowed (or could not be created).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }

    /// Counts a stream the worker could not create (budget, subscription cap, wake ids).
    pub(crate) fn count_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::AcqRel);
    }

    /// A wake token from this router's hub, for a stream it creates.
    pub(crate) fn alloc_token(&self) -> Result<WakeToken, CoreError> {
        let hub = self
            .token
            .hub()
            .ok_or(CoreError::StreamClosed(CloseReason::ConnectionClosed))?;
        Ok(hub.alloc()?)
    }

    pub fn wake_id(&self) -> WakeId {
        self.token.id()
    }

    /// Streams waiting to be received.
    pub fn pending(&self) -> usize {
        self.inner.lock().items.len()
    }
}

/// What `native.RouterSubscription` holds. Drop ⇒ `close(true)`.
#[derive(Debug)]
pub struct RouterHandle {
    pub id: u64,
    pub queue: Arc<RouterQueue>,
}

impl Drop for RouterHandle {
    fn drop(&mut self) {
        // §4.10: no lock in a forked child (see `Subscription`'s `Drop`).
        if crate::runtime::check_not_forked().is_err() {
            return;
        }
        self.queue.close(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::PCM16_48K;
    use crate::rx::test_util::{hub, ready, token};

    fn spec() -> RouterSpec {
        RouterSpec {
            format: PCM16_48K,
            chunk_ms: 20,
            max_queue_ms: 200,
            overflow: Overflow::DropOldest,
            include: None,
            ignore_bots: true,
            close_on_leave: true,
        }
    }

    fn sub(r: &RouterQueue, user: u64, id: u64) -> (Subscription, Arc<SubQueue>) {
        let q = SubQueue::new(&spec().sub_spec(user), r.alloc_token().unwrap());
        (Subscription::detached(id, q.clone()), q)
    }

    #[test]
    fn spec_rules() {
        let mut s = spec();
        s.validate().unwrap();
        assert_eq!(s.stream_budget_bytes().unwrap(), 10 * (1_920 + 160));
        let bots: HashSet<u64> = [2].into_iter().collect();
        assert!(s.selects(1, &bots) && !s.selects(2, &bots));
        s.include = Some([2, 3].into_iter().collect());
        s.ignore_bots = false;
        assert!(!s.selects(1, &bots) && s.selects(2, &bots));
        assert_eq!(s.sub_spec(3).source, RxSource::User(3));
        s.chunk_ms = 5;
        assert!(s.validate().is_err());
    }

    #[test]
    fn cap_drops_oldest_and_closes_it() {
        let (h, _r) = hub();
        let r = RouterQueue::new(token(&h));
        let mut queues = Vec::new();
        for u in 0..(ROUTER_QUEUE_CAP as u64 + 2) {
            let (s, q) = sub(&r, u, u);
            r.push(s);
            queues.push(q);
        }
        assert_eq!(r.dropped(), 2);
        assert_eq!(r.pending(), ROUTER_QUEUE_CAP);
        assert!(queues[0].is_closed() && queues[1].is_closed());
        assert!(!queues[2].is_closed());
        match r.try_pop_or_arm() {
            Pop::Item(s) => assert_eq!(s.queue.user_id(), Some(2)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn arm_wake_and_close_streams() {
        let (h, _r) = hub();
        let r = RouterQueue::new(token(&h));
        assert!(matches!(r.try_pop_or_arm(), Pop::Empty));
        let (s, q1) = sub(&r, 1, 1);
        r.push(s);
        assert_eq!(ready(&h), [r.wake_id().0]);
        let received = match r.try_pop_or_arm() {
            Pop::Item(s) => s,
            other => panic!("{other:?}"),
        };
        let (s, q2) = sub(&r, 2, 2);
        r.push(s);
        // close(close_streams=false): the received stream stays open, the pending one closes.
        r.close(false);
        assert!(!q1.is_closed());
        assert!(q2.is_closed());
        assert!(matches!(
            r.try_pop_or_arm(),
            Pop::Closed(CloseReason::Closed)
        ));
        drop(received);
        assert!(q1.is_closed());
        // After close, new streams are closed immediately.
        let (s, q3) = sub(&r, 3, 3);
        r.push(s);
        assert!(q3.is_closed());

        let r2 = RouterQueue::new(token(&h));
        let (s, q4) = sub(&r2, 4, 4);
        r2.push(s);
        let held = match r2.try_pop_or_arm() {
            Pop::Item(s) => s,
            other => panic!("{other:?}"),
        };
        let handle = RouterHandle { id: 1, queue: r2 };
        drop(handle); // close(true): also the received stream
        assert!(q4.is_closed());
        drop(held);
    }

    /// Regression: `UserStreams.close(close_streams=False)` keeps the received streams open, also
    /// once the router object is dropped (its `Drop` calls `close(true)`) or closed again.
    #[test]
    fn close_without_streams_survives_a_later_close_and_the_drop() {
        let (h, _r) = hub();
        let r = RouterQueue::new(token(&h));
        let (s, q) = sub(&r, 1, 1);
        r.push(s);
        let kept = match r.try_pop_or_arm() {
            Pop::Item(s) => s,
            other => panic!("{other:?}"),
        };
        let (s, pending) = sub(&r, 2, 2);
        r.push(s);
        let handle = RouterHandle { id: 1, queue: r };
        handle.queue.close(false);
        assert!(pending.is_closed(), "never-received streams always close");
        assert!(!q.is_closed());
        handle.queue.close(true);
        assert!(!q.is_closed(), "the first close decides");
        drop(handle);
        assert!(!q.is_closed(), "the drop must not close kept streams");
        drop(kept);
        assert!(q.is_closed());
    }
}
