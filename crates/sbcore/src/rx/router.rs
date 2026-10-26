//! The `listen_users` router (W3, §8.9): on a user's first voiced tick that passes the filters, the
//! worker creates a per-user subscription and pushes it here (cap `ROUTER_QUEUE_CAP`, drop-oldest
//! closes the dropped stream).
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use super::queue::{Overflow, Subscription};
use crate::error::CloseReason;
use crate::events::Pop;
use crate::format::Format;
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

/// Mutable state of a [`RouterQueue`].
#[derive(Debug, Default)]
struct RouterInner {
    items: VecDeque<Subscription>,
    armed: bool,
    closed: bool,
}

/// Queue of newly created per-user subscriptions.
#[derive(Debug)]
pub struct RouterQueue {
    inner: Mutex<RouterInner>,
    token: WakeToken,
    dropped: AtomicU64,
}

impl RouterQueue {
    pub fn new(token: WakeToken) -> Arc<Self> {
        Arc::new(RouterQueue {
            inner: Mutex::new(RouterInner::default()),
            token,
            dropped: AtomicU64::new(0),
        })
    }

    /// Pops the next stream, or arms the wake-up if empty.
    pub fn try_pop_or_arm(&self) -> Pop<Subscription> {
        if self.inner.lock().closed {
            Pop::Closed(CloseReason::Closed)
        } else {
            Pop::Empty
        }
    }

    /// Closes the router; `close_streams` also closes every stream it created.
    pub fn close(&self, close_streams: bool) {
        self.inner.lock().closed = true;
    }

    /// Streams discarded because the router queue overflowed.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }

    pub fn wake_id(&self) -> WakeId {
        self.token.id()
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
        self.queue.close(true);
    }
}
