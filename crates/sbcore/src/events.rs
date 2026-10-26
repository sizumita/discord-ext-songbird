//! Voice events and bounded per-subscriber event queues (W2, §8.11, §12.3).
//!
//! [`EventHub::emit`] pushes into every [`EventQueue`] whose [`KindMask`] matches. Queues are
//! bounded (`EVENT_QUEUE`, drop-oldest) and report drops with a synthetic `Lagged` event.
//! `Disconnected { will_retry: false }` is always the last event before the queues close
//! ([`EventHub::close_all`], T6).
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;

use crate::error::{CloseReason, CoreError};
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
    /// L5h.
    queues: Mutex<Vec<Arc<EventQueue>>>,
    /// Set by `close_all`; later `emit`s are dropped.
    closed: AtomicBool,
}

impl EventHub {
    pub fn new() -> Arc<Self> {
        Arc::new(EventHub::default())
    }

    /// Pushes `ev` into every matching queue. NET/RX/Python threads only; a no-op once closed.
    pub fn emit(&self, ev: &EventData) {
        // phase 0: events are not delivered yet.
    }

    /// Registers a new bounded queue (`cap` within `EVENT_QUEUE`, at most
    /// `MAX_EVENT_STREAMS_PER_CONN` per connection).
    pub fn subscribe(
        &self,
        cap: usize,
        kinds: KindMask,
        token: WakeToken,
    ) -> Result<Arc<EventQueue>, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// One L5h section: mark closed, append `last` (if any) to every queue, close every queue (T6).
    pub fn close_all(&self, last: Option<&EventData>) {
        self.closed.store(true, Ordering::Release);
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
}

impl EventQueue {
    /// Pops the next event, or arms the wake-up in the same critical section if empty.
    pub fn try_pop_or_arm(&self) -> Pop<EventData> {
        match self.inner.lock().closed {
            Some(r) => Pop::Closed(r),
            None => Pop::Empty,
        }
    }

    /// Pops without arming (usable from any thread).
    pub fn try_pop(&self) -> Pop<EventData> {
        match self.inner.lock().closed {
            Some(r) => Pop::Closed(r),
            None => Pop::Empty,
        }
    }

    /// Closes the queue; queued events stay readable until drained. Idempotent.
    pub fn close(&self) {
        let mut g = self.inner.lock();
        if g.closed.is_none() {
            g.closed = Some(CloseReason::Closed);
        }
    }

    /// Total events dropped by the overflow policy.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }

    pub fn wake_id(&self) -> WakeId {
        self.token.id()
    }
}
