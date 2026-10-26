//! Subscriptions and their bounded chunk queues (W3, §8.9, §8.10).
//!
//! `SubQueue` is `Mutex<VecDeque<Delivered>>` (L4) + an armed flag + atomics, bounded by
//! `max_chunks = ⌈max_queue_ms / chunk_ms⌉` and grown lazily from `min(max_chunks, 16)`.
//! A chunk's `dropped_ms` is always the audio dropped immediately before it.
#![allow(dead_code, unused_variables, clippy::ptr_arg)] // phase-0 skeleton

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use super::bus::MixFilter;
use super::chunk::ChunkData;
use super::router::{RouterQueue, RouterSpec};
use crate::conn::worker::{BudgetGuard, WorkerCmd, WorkerShared};
use crate::error::{CloseReason, CoreError};
use crate::events::Pop;
use crate::format::Format;
use crate::runtime::{LiveCounter, live};
use crate::wake::{WakeId, WakeToken};

/// Monotonic per-connection subscription id (never reused).
pub type SubId = u64;

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

impl SubSpec {
    /// `CHUNK_MS`, `chunk_ms ≤ max_queue_ms ≤ MAX_QUEUE_MS_MAX`, G.711 ⇒ 8 kHz mono.
    pub fn validate(&self) -> Result<(), CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// `max_chunks × (chunk_bytes + 160)`, charged against `max_receive_mb`.
    pub fn budget_bytes(&self) -> Result<usize, CoreError> {
        Err(CoreError::Internal("unimplemented"))
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
    delivered: AtomicU64,
    dropped_chunks: AtomicU64,
    dropped_ms: AtomicU64,
    _live: LiveCounter,
}

impl SubQueue {
    pub fn new(spec: &SubSpec, token: WakeToken) -> Arc<Self> {
        let user_id = match spec.source {
            RxSource::User(u) => Some(u),
            RxSource::Mixed(_) => None,
        };
        Arc::new(SubQueue {
            inner: Mutex::new(SubInner::default()),
            token,
            format: spec.format,
            user_id,
            chunk_ms: spec.chunk_ms,
            max_chunks: spec.max_queue_ms.div_ceil(spec.chunk_ms.max(1)) as usize,
            overflow: spec.overflow,
            delivered: AtomicU64::new(0),
            dropped_chunks: AtomicU64::new(0),
            dropped_ms: AtomicU64::new(0),
            _live: LiveCounter::new(&live::SUB_QUEUE),
        })
    }

    /// Applies the overflow policy; rings after unlocking if armed. ConnWorker only.
    pub(crate) fn push(&self, chunk: Arc<ChunkData>) {
        // phase 0: chunks are dropped.
    }

    /// Pops the next chunk, or arms the wake-up in the same critical section if empty. Queued
    /// items, then `Closed(Overflow)` exactly once (the binding raises `StreamOverflow`), then
    /// `Closed(..)` (§8.9).
    pub fn try_pop_or_arm(&self) -> Pop<Delivered> {
        match self.inner.lock().closed {
            Some(r) => Pop::Closed(r),
            None => Pop::Empty,
        }
    }

    /// Pops without arming (usable from any thread).
    pub fn try_pop(&self) -> Pop<Delivered> {
        match self.inner.lock().closed {
            Some(r) => Pop::Closed(r),
            None => Pop::Empty,
        }
    }

    /// Moves up to `max` chunks into `out`; returns the close reason once closed and drained.
    pub fn drain(&self, max: usize, out: &mut Vec<Delivered>) -> Option<CloseReason> {
        self.inner.lock().closed
    }

    /// Drops everything queued; returns the milliseconds dropped. `reset_clock`: the next chunk has
    /// `start_ms == 0` and `seq == 0`.
    pub fn skip_to_live(&self, reset_clock: bool) -> u64 {
        0
    }

    /// `timeline_ms` of the clock origin; `None` until the first chunk after a reset.
    pub fn origin_timeline_ms(&self) -> Option<u64> {
        self.inner.lock().origin
    }

    /// Closes the queue (first reason wins); queued chunks stay readable until drained.
    pub fn close(&self, r: CloseReason) {
        let mut g = self.inner.lock();
        if g.closed.is_none() {
            g.closed = Some(r);
        }
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().closed.is_some()
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
    budget: BudgetGuard,
}

impl Subscription {
    pub(crate) fn new(
        id: SubId,
        queue: Arc<SubQueue>,
        worker: Weak<WorkerShared>,
        budget: BudgetGuard,
    ) -> Self {
        Subscription {
            id,
            queue,
            worker,
            budget,
        }
    }

    /// Mixed streams only: moves the subscription to the lane of `f` at its first complete chunk.
    pub fn set_filter(&self, f: MixFilter) -> Result<(), CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Closes the queue and unregisters at the next tick. Idempotent.
    pub fn close(&self) {
        self.queue.close(CloseReason::Closed);
        if let Some(w) = self.worker.upgrade() {
            w.push(WorkerCmd::Rx(RxCmd::RemoveSub(self.id)));
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.close();
    }
}
