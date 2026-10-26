//! The single per-connection songbird event handler (W2, §8.2).
//!
//! Registered once per event type at Driver creation and never added or removed afterwards, except
//! by teardown and watchdog re-installation (§8.8). `act` is O(#SSRC), never awaits, never unwinds
//! (`catch_unwind`), and always returns `None`.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use parking_lot::Mutex;
use songbird::events::{Event, EventContext, EventHandler};

use super::shared::ConnShared;
use crate::runtime::{LiveCounter, live};
use crate::rx::raw::{RxInput, RxSender};
use crate::tracks::registry::TrackRegistry;

/// Counters reported through `ConnShared::stats` (§11.3).
#[derive(Debug, Default)]
pub(crate) struct DispatchCounters {
    pub(crate) panics: AtomicU64,
    pub(crate) ticks_dropped: AtomicU64,
    pub(crate) ctrl_dropped: AtomicU64,
    /// Milliseconds timestamp of the last heartbeat (`Periodic(1 s)`), compared by the ConnWorker.
    pub(crate) heartbeat: AtomicU64,
}

/// State shared by [`Dispatch`] and [`Heartbeat`]. Holds only weak references upward (I4).
pub(crate) struct DispatchInner {
    conn: Weak<ConnShared>,
    rx: Option<RxSender>,
    tracks: Weak<TrackRegistry>,
    /// Control inputs that did not fit the tick channel (cap `RX_CTRL_OVERFLOW_CAP`); drained first
    /// by the worker.
    ctrl_overflow: Arc<Mutex<VecDeque<RxInput>>>,
    pub(crate) ctr: DispatchCounters,
    _live: LiveCounter,
}

impl DispatchInner {
    /// Handles one event (§8.2 table). O(#SSRC); never awaits.
    fn on_event(&self, ctx: &EventContext<'_>) {
        // phase 0: events are ignored.
    }
}

/// The songbird global event handler (§8.2).
#[derive(Clone)]
pub(crate) struct Dispatch(Arc<DispatchInner>);

/// The `Periodic(1 s)` handler: stores `ctr.heartbeat` only (§8.8).
#[derive(Clone)]
pub(crate) struct Heartbeat(Arc<DispatchInner>);

impl Dispatch {
    pub(crate) fn new(
        conn: Weak<ConnShared>,
        rx: Option<RxSender>,
        tracks: Weak<TrackRegistry>,
        ctrl_overflow: Arc<Mutex<VecDeque<RxInput>>>,
    ) -> Self {
        Dispatch(Arc::new(DispatchInner {
            conn,
            rx,
            tracks,
            ctrl_overflow,
            ctr: DispatchCounters::default(),
            _live: LiveCounter::new(&live::DISPATCH),
        }))
    }

    /// The heartbeat handler sharing this dispatcher's state.
    pub(crate) fn heartbeat(&self) -> Heartbeat {
        Heartbeat(self.0.clone())
    }

    pub(crate) fn counters(&self) -> &DispatchCounters {
        &self.0.ctr
    }
}

#[async_trait]
impl EventHandler for Dispatch {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if catch_unwind(AssertUnwindSafe(|| self.0.on_event(ctx))).is_err() {
            self.0.ctr.panics.fetch_add(1, Ordering::Relaxed);
        }
        // Never Some(Event::Cancel): a no-op for untimed handlers.
        None
    }
}

#[async_trait]
impl EventHandler for Heartbeat {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        // phase 0: the heartbeat timestamp is not recorded yet.
        None
    }
}
