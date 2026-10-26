//! The single per-connection songbird event handler (W2, §8.2).
//!
//! Registered once per event type at Driver creation and never added or removed afterwards, except
//! by teardown and watchdog re-installation (§8.8). `act` is O(#SSRC), never awaits, never unwinds
//! (`catch_unwind`), and always returns `None`.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use songbird::events::{Event, EventContext, EventHandler};
use tokio::sync::mpsc::error::TrySendError;

use super::LogLimiter;
use super::gateway::{DiscKind, DiscReason, GwInput};
use super::shared::ConnShared;
use crate::limits::RX_CTRL_OVERFLOW_CAP;
use crate::runtime::{LiveCounter, live};
use crate::rx::raw::{RawTick, RxInput, RxSender};
use crate::tracks::registry::TrackRegistry;

/// One ERROR log per minute for caught dispatcher panics (§5.7).
static PANIC_LOG: LogLimiter = LogLimiter::per_minute();

/// Counters reported through `ConnShared::stats` (§11.3).
#[derive(Debug, Default)]
pub(crate) struct DispatchCounters {
    pub(crate) panics: AtomicU64,
    pub(crate) ticks_dropped: AtomicU64,
    pub(crate) ctrl_dropped: AtomicU64,
    /// Milliseconds since `DispatchInner::base` of the last heartbeat (`Periodic(1 s)`) plus one;
    /// 0 = none yet. Compared with the clock by the ConnWorker (§8.8).
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
    /// Reference point of `ctr.heartbeat`.
    base: Instant,
    pub(crate) ctr: DispatchCounters,
    _live: LiveCounter,
}

impl DispatchInner {
    /// Handles one event (§8.2 table). O(#SSRC); never awaits.
    fn on_event(&self, ctx: &EventContext<'_>) {
        match ctx {
            EventContext::VoiceTick(t) => {
                if let Some(rx) = &self.rx {
                    let tick = RawTick::from_voice_tick(t, Instant::now());
                    if let Err(TrySendError::Full(_)) = rx.try_send(RxInput::Tick(tick)) {
                        // The pipeline treats the hole like any other gap.
                        self.ctr.ticks_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            EventContext::SpeakingStateUpdate(s) => {
                if let Some(uid) = s.user_id {
                    self.send_ctrl(RxInput::Map {
                        ssrc: s.ssrc,
                        user_id: uid.0,
                    });
                }
            }
            EventContext::ClientDisconnect(c) => {
                self.send_ctrl(RxInput::UserLeft {
                    user_id: c.user_id.0,
                });
            }
            EventContext::DriverConnect(d) => self.driver_connect(d.ssrc, false),
            EventContext::DriverReconnect(d) => self.driver_connect(d.ssrc, true),
            EventContext::DriverDisconnect(d) => self.driver_disconnect(
                DiscKind::from_songbird(d.kind),
                d.reason.map(DiscReason::from_songbird),
            ),
            EventContext::Track(list) => {
                if let Some(reg) = self.tracks.upgrade() {
                    reg.on_track_event(list);
                }
            }
            _ => {}
        }
    }

    /// `DriverConnect` / `DriverReconnect`: the state machine, then the receive link.
    pub(crate) fn driver_connect(&self, ssrc: u32, reconnect: bool) {
        if let Some(conn) = self.conn.upgrade() {
            conn.apply(if reconnect {
                GwInput::DriverReconnect { ssrc }
            } else {
                GwInput::DriverConnect { ssrc }
            });
        }
        self.send_ctrl(RxInput::LinkUp { ssrc: Some(ssrc) });
    }

    /// `DriverDisconnect`: the state machine, then the receive link.
    pub(crate) fn driver_disconnect(&self, kind: DiscKind, reason: Option<DiscReason>) {
        if let Some(conn) = self.conn.upgrade() {
            conn.apply(GwInput::DriverDisconnect { kind, reason });
        }
        self.send_ctrl(RxInput::LinkDown);
    }

    /// Control inputs are never silently lost: when the channel is full they go to the 64-entry
    /// overflow that the worker drains first.
    fn send_ctrl(&self, input: RxInput) {
        let Some(rx) = &self.rx else {
            return;
        };
        match rx.try_send(input) {
            Ok(()) | Err(TrySendError::Closed(_)) => {}
            Err(TrySendError::Full(input)) => {
                let mut q = self.ctrl_overflow.lock();
                if q.len() < RX_CTRL_OVERFLOW_CAP {
                    q.push_back(input);
                } else {
                    self.ctr.ctrl_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    fn beat(&self) {
        let ms = u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX - 1);
        self.ctr
            .heartbeat
            .store(ms.saturating_add(1), Ordering::Release);
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
            base: Instant::now(),
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

    /// When the last heartbeat arrived (the dispatcher's creation if none has yet).
    pub(crate) fn last_heartbeat(&self) -> Instant {
        let ms = self.0.ctr.heartbeat.load(Ordering::Acquire);
        self.0
            .base
            .checked_add(Duration::from_millis(ms.saturating_sub(1)))
            .unwrap_or(self.0.base)
    }

    /// The same paths `act` takes, without a songbird context (test hooks, unit tests).
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn inner(&self) -> &DispatchInner {
        &self.0
    }
}

#[async_trait]
impl EventHandler for Dispatch {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if catch_unwind(AssertUnwindSafe(|| self.0.on_event(ctx))).is_err() {
            self.0.ctr.panics.fetch_add(1, Ordering::Relaxed);
            if PANIC_LOG.allow() {
                ::log::error!(
                    "panic in the songbird event dispatcher (event dropped); please report this bug"
                );
            }
        }
        // Never Some(Event::Cancel): a no-op for untimed handlers.
        None
    }
}

#[async_trait]
impl EventHandler for Heartbeat {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        self.0.beat();
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inner_with_channel(
        cap: usize,
    ) -> (
        Dispatch,
        tokio::sync::mpsc::Receiver<RxInput>,
        Arc<Mutex<VecDeque<RxInput>>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(cap);
        let overflow = Arc::new(Mutex::new(VecDeque::new()));
        let d = Dispatch::new(Weak::new(), Some(tx), Weak::new(), overflow.clone());
        (d, rx, overflow)
    }

    #[test]
    fn control_inputs_overflow_into_the_bounded_side_queue() {
        let _serial = crate::conn::TEST_LOCK.lock();
        let (d, mut rx, overflow) = inner_with_channel(1);
        d.inner().send_ctrl(RxInput::LinkDown);
        for i in 0..(RX_CTRL_OVERFLOW_CAP as u64 + 5) {
            d.inner().send_ctrl(RxInput::UserLeft { user_id: i });
        }
        assert!(matches!(rx.try_recv(), Ok(RxInput::LinkDown)));
        assert_eq!(overflow.lock().len(), RX_CTRL_OVERFLOW_CAP);
        assert_eq!(d.counters().ctrl_dropped.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn driver_events_without_a_connection_still_update_the_link() {
        let _serial = crate::conn::TEST_LOCK.lock();
        let (d, mut rx, _overflow) = inner_with_channel(4);
        d.inner().driver_connect(5, false);
        d.inner()
            .driver_disconnect(DiscKind::Runtime, Some(DiscReason::Io));
        assert!(matches!(
            rx.try_recv(),
            Ok(RxInput::LinkUp { ssrc: Some(5) })
        ));
        assert!(matches!(rx.try_recv(), Ok(RxInput::LinkDown)));
    }

    #[test]
    fn heartbeat_timestamps_advance() {
        let _serial = crate::conn::TEST_LOCK.lock();
        let (d, _rx, _overflow) = inner_with_channel(1);
        let created = d.last_heartbeat();
        std::thread::sleep(Duration::from_millis(5));
        d.heartbeat().0.beat();
        assert!(d.last_heartbeat() > created);
        assert!(d.last_heartbeat() <= Instant::now());
    }

    #[test]
    fn live_counter_tracks_dispatchers() {
        let _serial = crate::conn::TEST_LOCK.lock();
        let base = live::DISPATCH.load(Ordering::Acquire);
        let (d, _rx, _overflow) = inner_with_channel(1);
        let hb = d.heartbeat();
        assert_eq!(live::DISPATCH.load(Ordering::Acquire), base + 1);
        drop(d);
        assert_eq!(
            live::DISPATCH.load(Ordering::Acquire),
            base + 1,
            "the heartbeat shares it"
        );
        drop(hb);
        assert_eq!(live::DISPATCH.load(Ordering::Acquire), base);
    }
}
