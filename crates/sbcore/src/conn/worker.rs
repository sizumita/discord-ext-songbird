//! The per-connection ConnWorker (W2, §3.1, §4.7 T6, §8.8): one tokio task on the RX runtime that
//! runs the receive pipeline, the send housekeeper and the connection timers.
//!
//! [`ConnWorker::step`] is one synchronous iteration (commands, queued inputs, timers) and returns
//! the next wake-up; [`ConnWorker::run`] wraps it in `select!{shutdown, rx_in.recv, notify,
//! sleep_until(next)}`. Detached test connections call `step` directly with a manual clock.

use std::collections::VecDeque;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::error::TryRecvError;

use super::LogLimiter;
use super::gateway::GwInput;
use super::shared::ConnShared;
use crate::clock::Clock;
use crate::error::{CloseReason, CoreError};
use crate::events::EventData;
use crate::limits::{HEARTBEAT_MISS, TICK_CHANNEL_CAP, TICK_MS, check};
use crate::runtime::{LiveCounter, live};
use crate::rx::pipeline::{RxCounts, RxPipeline};
use crate::rx::queue::{RxCmd, SubQueue};
use crate::rx::raw::RxInput;
use crate::rx::router::RouterQueue;
use crate::sync::{Shutdown, lock_checked};
use crate::tx::LiveSource;
use crate::tx::housekeep::Housekeeper;

/// One tick.
const TICK: Duration = Duration::from_millis(TICK_MS as u64);
/// Repeated `RxStalled` / `HandlersLost` inputs while the condition lasts (the state machine
/// rate-limits the reinstall itself; this only bounds L2 traffic).
const WATCHDOG_REPEAT: Duration = Duration::from_secs(1);
/// Inputs taken from the tick channel per step before yielding.
const INPUT_BUDGET: usize = TICK_CHANNEL_CAP * 2;

/// One ERROR log per minute for caught worker panics (§5.7).
static PANIC_LOG: LogLimiter = LogLimiter::per_minute();

/// Commands from Python threads to the worker (applied at the next tick/lane boundary).
pub enum WorkerCmd {
    Rx(RxCmd),
    AddSource(Weak<dyn LiveSource>),
    /// From the `RxLink` action. A `Link` with `seq ≤` the last applied seq is ignored (§4.6).
    Link {
        up: bool,
        ssrc: Option<u32>,
        seq: u64,
    },
}

/// A reservation of the per-connection receive budget (§8.9). Drop releases it.
#[derive(Debug)]
pub struct BudgetGuard {
    worker: Weak<WorkerShared>,
    bytes: usize,
}

impl BudgetGuard {
    /// Reserved bytes.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        if let Some(w) = self.worker.upgrade() {
            w.budget_used.fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

/// State shared between Python threads and the worker task.
pub struct WorkerShared {
    /// L3.
    cmds: Mutex<Vec<WorkerCmd>>,
    notify: Notify,
    shutdown: Arc<Shutdown>,
    /// L10: every subscription queue, so T6 can close them synchronously.
    known_queues: Mutex<Vec<Weak<SubQueue>>>,
    /// L10.
    known_routers: Mutex<Vec<Weak<RouterQueue>>>,
    budget_used: AtomicUsize,
    budget_max: usize,
    /// Published by the worker after every step.
    timeline_ms: AtomicU64,
    /// L10: published by the worker after every step.
    rx_counts: Mutex<RxCounts>,
    /// Panics caught in a worker step (§5.7).
    panics: AtomicU64,
    /// Heartbeat gaps longer than `HEARTBEAT_MISS` (§8.8).
    heartbeat_misses: AtomicU64,
}

impl WorkerShared {
    /// `budget_max` = `max_receive_mb` in bytes.
    pub fn new(budget_max: usize) -> Arc<Self> {
        Arc::new(WorkerShared {
            cmds: Mutex::new(Vec::new()),
            notify: Notify::new(),
            shutdown: Shutdown::new(),
            known_queues: Mutex::new(Vec::new()),
            known_routers: Mutex::new(Vec::new()),
            budget_used: AtomicUsize::new(0),
            budget_max,
            timeline_ms: AtomicU64::new(0),
            rx_counts: Mutex::new(RxCounts::default()),
            panics: AtomicU64::new(0),
            heartbeat_misses: AtomicU64::new(0),
        })
    }

    /// Queues a command and wakes the worker. Dropped once the worker shut down (nothing would
    /// consume it; the command's queues are closed by T6).
    pub fn push(&self, c: WorkerCmd) {
        if self.shutdown.is_set() {
            return;
        }
        lock_checked(&self.cmds).push(c);
        self.notify.notify_one();
    }

    /// Wakes the worker without a command (e.g. a queue entry to advance).
    pub fn wake(&self) {
        self.notify.notify_one();
    }

    /// Reserves `bytes` of the receive budget, or `LimitExceeded`.
    pub fn reserve_budget(self: &Arc<Self>, bytes: usize) -> Result<BudgetGuard, CoreError> {
        let mut cur = self.budget_used.load(Ordering::Acquire);
        loop {
            let total = cur.saturating_add(bytes);
            check("max_receive_mb", total, 0..=self.budget_max)?;
            match self.budget_used.compare_exchange_weak(
                cur,
                total,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(BudgetGuard {
                        worker: Arc::downgrade(self),
                        bytes,
                    });
                }
                Err(actual) => cur = actual,
            }
        }
    }

    /// Remembers a subscription queue so T6 can close it synchronously (dead entries are pruned).
    pub fn register_queue(&self, q: &Arc<SubQueue>) {
        // L10 is a leaf: prune by liveness only (`is_closed` would take the queue's L4).
        let mut v = lock_checked(&self.known_queues);
        v.retain(|w| w.strong_count() > 0);
        v.push(Arc::downgrade(q));
    }

    /// Remembers a router queue so T6 can close it synchronously (dead entries are pruned).
    pub fn register_router(&self, q: &Arc<RouterQueue>) {
        let mut v = lock_checked(&self.known_routers);
        v.retain(|w| w.strong_count() > 0);
        v.push(Arc::downgrade(q));
    }

    /// Open subscription queues (for `MAX_SUBS_PER_CONN`).
    pub fn open_queues(&self) -> usize {
        let alive: Vec<Arc<SubQueue>> = lock_checked(&self.known_queues)
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        // Checked after the registry lock is released.
        alive.iter().filter(|q| !q.is_closed()).count()
    }

    /// Live router queues.
    pub fn open_routers(&self) -> usize {
        lock_checked(&self.known_routers)
            .iter()
            .filter(|w| w.strong_count() > 0)
            .count()
    }

    /// T6, synchronous on the caller thread: closes every registered subscription and router queue.
    /// The queues are collected under the lock and closed after it is released.
    pub fn close_queues(&self, r: CloseReason) {
        let queues: Vec<Arc<SubQueue>> = std::mem::take(&mut *lock_checked(&self.known_queues))
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        let routers: Vec<Arc<RouterQueue>> =
            std::mem::take(&mut *lock_checked(&self.known_routers))
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
        for q in &queues {
            q.close(r);
        }
        for q in &routers {
            q.close(true);
        }
    }

    /// Sets the shutdown flag and wakes the worker; it exits at its next poll. Pending commands are
    /// dropped (outside the lock).
    pub fn shutdown(&self) {
        self.shutdown.trigger();
        self.notify.notify_one();
        let dropped = std::mem::take(&mut *lock_checked(&self.cmds));
        drop(dropped);
    }

    /// Bytes of the receive budget currently reserved.
    pub fn budget_used(&self) -> usize {
        self.budget_used.load(Ordering::Acquire)
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.is_set()
    }

    /// The receive timeline position published by the worker.
    pub fn timeline_ms(&self) -> u64 {
        self.timeline_ms.load(Ordering::Acquire)
    }

    /// The receive counters published by the worker.
    pub fn rx_counts(&self) -> RxCounts {
        *lock_checked(&self.rx_counts)
    }

    pub(crate) fn panics(&self) -> u64 {
        self.panics.load(Ordering::Relaxed)
    }

    pub(crate) fn heartbeat_misses(&self) -> u64 {
        self.heartbeat_misses.load(Ordering::Relaxed)
    }

    fn publish(&self, timeline_ms: u64, counts: RxCounts) {
        self.timeline_ms.store(timeline_ms, Ordering::Release);
        *lock_checked(&self.rx_counts) = counts;
    }
}

/// The worker task's state (owned by the task; nothing else holds it).
pub struct ConnWorker {
    conn: Weak<ConnShared>,
    shared: Arc<WorkerShared>,
    pipeline: RxPipeline,
    house: Housekeeper,
    rx_in: Receiver<RxInput>,
    overflow: Arc<Mutex<VecDeque<RxInput>>>,
    clock: Clock,
    last_link_seq: u64,
    /// Reused command buffer (swapped with `WorkerShared.cmds` under L3).
    cmd_buf: Vec<WorkerCmd>,
    /// An input received by `run`'s `select!`, processed first by the next `step`.
    stash: Option<RxInput>,
    rx_closed: bool,
    /// `stalled_since` of the stall already reported with `ReceiveStalled`.
    stall_reported: Option<Instant>,
    stall_last_apply: Option<Instant>,
    hb_last_apply: Option<Instant>,
    _live: LiveCounter,
}

/// The earlier of two optional instants.
fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

impl ConnWorker {
    pub(crate) fn new(
        conn: Weak<ConnShared>,
        shared: Arc<WorkerShared>,
        pipeline: RxPipeline,
        rx_in: Receiver<RxInput>,
        overflow: Arc<Mutex<VecDeque<RxInput>>>,
        clock: Clock,
    ) -> Self {
        ConnWorker {
            conn,
            shared,
            pipeline,
            house: Housekeeper::new(),
            rx_in,
            overflow,
            clock,
            last_link_seq: 0,
            cmd_buf: Vec::new(),
            stash: None,
            rx_closed: false,
            stall_reported: None,
            stall_last_apply: None,
            hb_last_apply: None,
            _live: LiveCounter::new(&live::CONN_WORKER),
        }
    }

    /// One iteration: commands, queued inputs, timers. Returns the next wake-up time (`None`:
    /// nothing is armed; the worker sleeps until a command, an input or shutdown).
    ///
    /// The body runs inside `catch_unwind`: on a panic every subscription closes with
    /// `"internal_error"` and the receive pipeline is rebuilt empty (§5.7).
    pub fn step(&mut self, now: Instant) -> Option<Instant> {
        if self.shared.is_shutdown() {
            return None;
        }
        let conn = self.conn.upgrade()?;
        match catch_unwind(AssertUnwindSafe(|| self.step_inner(&conn, now))) {
            Ok(next) => next,
            Err(_) => {
                self.shared.panics.fetch_add(1, Ordering::Relaxed);
                if PANIC_LOG.allow() {
                    ::log::error!(
                        "panic in the connection worker; receive streams closed with \
                         'internal_error'. Please report this bug"
                    );
                }
                self.recover(&conn);
                now.checked_add(TICK)
            }
        }
    }

    /// After a caught panic: close every subscription and rebuild the pipeline empty.
    fn recover(&mut self, conn: &Arc<ConnShared>) {
        self.shared.close_queues(CloseReason::InternalError);
        let fresh = conn.new_pipeline();
        let mut old = std::mem::replace(&mut self.pipeline, fresh);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            old.close_all(CloseReason::InternalError)
        }));
        let _ = catch_unwind(AssertUnwindSafe(move || drop(old)));
    }

    fn step_inner(&mut self, conn: &Arc<ConnShared>, now: Instant) -> Option<Instant> {
        // 1. Commands (subscribe/filter/source/link), applied at this tick boundary.
        std::mem::swap(&mut *lock_checked(&self.shared.cmds), &mut self.cmd_buf);
        for cmd in self.cmd_buf.drain(..) {
            match cmd {
                WorkerCmd::Rx(c) => self.pipeline.apply(c),
                WorkerCmd::AddSource(w) => self.house.add(w),
                WorkerCmd::Link { up, ssrc: _, seq } => {
                    if seq > self.last_link_seq {
                        self.last_link_seq = seq;
                        self.house.link(up);
                    }
                }
            }
        }

        // 2. Inputs: the control overflow first (so mappings are never lost), then the channel.
        loop {
            let item = self.overflow.lock().pop_front();
            match item {
                Some(input) => self.pipeline.on_input(input, now),
                None => break,
            }
        }
        if let Some(input) = self.stash.take() {
            self.pipeline.on_input(input, now);
        }
        let mut taken = 0usize;
        while taken < INPUT_BUDGET && !self.rx_closed {
            match self.rx_in.try_recv() {
                Ok(input) => {
                    self.pipeline.on_input(input, now);
                    taken += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => self.rx_closed = true,
            }
        }

        // 3. Receive timers: silence synthesis, pruning, stall watchdog.
        let rt = self.pipeline.on_timer(now);
        self.shared
            .publish(self.pipeline.timeline_ms(), self.pipeline.counts());
        let mut next = rt.next_wake;
        next = earliest(next, self.watch_stall(conn, rt.stalled_since, now));

        // 4. Handler liveness (heartbeat), while the Driver exists.
        next = earliest(next, self.watch_heartbeat(conn, now));

        // 5. Send housekeeping (live sources), then tracks and the queue.
        next = earliest(next, self.house.tick(now, conn.events()));
        conn.poll_tracks(now);
        if conn.tracks_need_poll() {
            next = earliest(next, now.checked_add(TICK));
        }

        // 6. Gateway deadlines (move fallback, move grace, rejoin due/timeout).
        if let Some(d) = conn.next_deadline()
            && now >= d
        {
            conn.apply(GwInput::Deadline);
        }
        next = earliest(next, conn.next_deadline());

        if taken >= INPUT_BUDGET {
            next = Some(now);
        }
        next
    }

    /// Emits `ReceiveStalled` once per stall and feeds `RxStalled` to the state machine (at most
    /// once per second while the stall lasts).
    fn watch_stall(
        &mut self,
        conn: &Arc<ConnShared>,
        stalled_since: Option<Instant>,
        now: Instant,
    ) -> Option<Instant> {
        let Some(since) = stalled_since else {
            self.stall_reported = None;
            self.stall_last_apply = None;
            return None;
        };
        if self.stall_reported != Some(since) {
            self.stall_reported = Some(since);
            self.stall_last_apply = None;
            let since_ms =
                u64::try_from(now.saturating_duration_since(since).as_millis()).unwrap_or(u64::MAX);
            conn.events().emit(&EventData::ReceiveStalled { since_ms });
        }
        if self
            .stall_last_apply
            .is_none_or(|t| now.saturating_duration_since(t) >= WATCHDOG_REPEAT)
        {
            self.stall_last_apply = Some(now);
            conn.apply(GwInput::RxStalled { since });
        }
        self.stall_last_apply
            .and_then(|t| t.checked_add(WATCHDOG_REPEAT))
    }

    /// `HandlersLost` when no heartbeat arrived for `HEARTBEAT_MISS` while the Driver exists.
    fn watch_heartbeat(&mut self, conn: &Arc<ConnShared>, now: Instant) -> Option<Instant> {
        let last = conn.last_heartbeat()?;
        let due = last.checked_add(HEARTBEAT_MISS)?;
        if now < due {
            self.hb_last_apply = None;
            return Some(due);
        }
        if self
            .hb_last_apply
            .is_none_or(|t| now.saturating_duration_since(t) >= WATCHDOG_REPEAT)
        {
            self.hb_last_apply = Some(now);
            self.shared.heartbeat_misses.fetch_add(1, Ordering::Relaxed);
            conn.apply(GwInput::HandlersLost);
        }
        self.hb_last_apply
            .and_then(|t| t.checked_add(WATCHDOG_REPEAT))
    }

    /// Best-effort cleanup on exit (decoders, resamplers, lanes, live sources). Everything
    /// observable was already closed by T5/T6.
    fn exit(&mut self) {
        let pipeline = &mut self.pipeline;
        let _ = catch_unwind(AssertUnwindSafe(|| {
            pipeline.close_all(CloseReason::ConnectionClosed)
        }));
        self.house.close_all();
    }

    /// The task body: `select!{shutdown, rx_in.recv, notify, sleep_until(next)}` around `step`.
    // Contract signature (§12.3): the explicit `+ Send` bound is part of the API.
    #[allow(clippy::manual_async_fn)]
    pub fn run(mut self) -> impl Future<Output = ()> + Send {
        async move {
            let shared = self.shared.clone();
            loop {
                if shared.is_shutdown() || self.conn.strong_count() == 0 {
                    break;
                }
                let now = self.clock.now();
                let next = self.step(now);
                if shared.is_shutdown() {
                    break;
                }
                let sleep_to = next.map(tokio::time::Instant::from_std);
                let rx_open = !self.rx_closed;
                tokio::select! {
                    biased;
                    _ = shared.shutdown.wait() => break,
                    input = self.rx_in.recv(), if rx_open => match input {
                        Some(i) => self.stash = Some(i),
                        None => self.rx_closed = true,
                    },
                    _ = shared.notify.notified() => {}
                    _ = async {
                        match sleep_to {
                            Some(t) => tokio::time::sleep_until(t).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {}
                }
            }
            self.exit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_reserved_and_released() {
        let w = WorkerShared::new(1000);
        let a = w.reserve_budget(600).unwrap();
        assert_eq!(a.bytes(), 600);
        assert!(matches!(w.reserve_budget(401), Err(CoreError::Limit(_))));
        let b = w.reserve_budget(400).unwrap();
        assert_eq!(w.budget_used(), 1000);
        drop(a);
        assert_eq!(w.budget_used(), 400);
        drop(b);
        assert_eq!(w.budget_used(), 0);
        // A guard outliving its worker does not panic.
        let c = w.reserve_budget(10).unwrap();
        drop(w);
        drop(c);
    }

    #[test]
    fn push_after_shutdown_is_dropped() {
        let w = WorkerShared::new(0);
        w.push(WorkerCmd::Link {
            up: true,
            ssrc: None,
            seq: 1,
        });
        assert_eq!(w.cmds.lock().len(), 1);
        w.shutdown();
        assert!(w.is_shutdown());
        assert!(w.cmds.lock().is_empty());
        w.push(WorkerCmd::Link {
            up: false,
            ssrc: None,
            seq: 2,
        });
        assert!(w.cmds.lock().is_empty());
    }

    #[test]
    fn earliest_picks_the_minimum() {
        let t = Instant::now();
        let later = t + Duration::from_secs(1);
        assert_eq!(earliest(Some(t), Some(later)), Some(t));
        assert_eq!(earliest(None, Some(later)), Some(later));
        assert_eq!(earliest(Some(t), None), Some(t));
        assert_eq!(earliest(None, None), None);
    }
}

#[cfg(all(test, feature = "test-hooks"))]
mod hooked_tests {
    use std::num::NonZeroU64;
    use std::os::unix::net::UnixStream;

    use super::*;
    use crate::conn::settings::{ConnParams, RawVoiceSettings, VoiceSettings};
    use crate::events::{EventData, KindMask, Pop};
    use crate::wake::Doorbell;

    type Detached = (
        Arc<ConnShared>,
        ConnWorker,
        Arc<crate::clock::ManualClock>,
        UnixStream,
    );

    fn detached() -> Detached {
        let (clock, m) = Clock::manual();
        let (a, b) = UnixStream::pair().unwrap();
        let p = ConnParams {
            guild_id: NonZeroU64::new(1).unwrap(),
            user_id: NonZeroU64::new(2).unwrap(),
            reconnect: true,
            settings: VoiceSettings::validate(RawVoiceSettings::default()).unwrap(),
            clock,
        };
        let (c, w) = ConnShared::new_detached(p, Doorbell::from_unix_stream(a).unwrap()).unwrap();
        (c, w, m, b)
    }

    #[test]
    fn stall_is_reported_once_and_feeds_the_state_machine() {
        let _serial = crate::conn::TEST_LOCK.lock();
        let (conn, mut w, m, _peer) = detached();
        let events = conn.subscribe_events(8, KindMask::ALL).unwrap();
        conn.request_join(NonZeroU64::new(10).unwrap(), false, false);
        conn.on_voice_state("s", Some(10));
        conn.on_voice_server("t", Some("e.discord.media"));
        assert_eq!(conn.status().state, "connected");
        while let Pop::Item(_) = events.try_pop() {}

        let since = m.now();
        m.advance(Duration::from_secs(2));
        let next = w.watch_stall(&conn, Some(since), m.now());
        assert_eq!(next, Some(m.now() + WATCHDOG_REPEAT));
        m.advance(Duration::from_millis(500));
        w.watch_stall(&conn, Some(since), m.now());
        let stalls: Vec<EventData> = std::iter::from_fn(|| match events.try_pop() {
            Pop::Item(e) => Some(e),
            _ => None,
        })
        .collect();
        assert!(matches!(
            stalls[..],
            [EventData::ReceiveStalled { since_ms: 2_000 }]
        ));
        let reinstalls = || {
            conn.stats()
                .iter()
                .find(|(k, _)| *k == "dispatch_reinstalls")
                .map(|(_, v)| *v)
        };
        assert_eq!(reinstalls(), Some(1), "rate-limited by the state machine");
        // Stalled for 5 s: auto-recover schedules a rejoin.
        m.advance(Duration::from_millis(2_500));
        w.watch_stall(&conn, Some(since), m.now());
        assert_eq!(conn.status().state, "reconnecting");
        // The stall ends: the next one is reported again.
        assert!(w.watch_stall(&conn, None, m.now()).is_none());
        w.watch_stall(&conn, Some(m.now()), m.now());
        assert!(matches!(
            events.try_pop(),
            Pop::Item(EventData::ReceiveStalled { since_ms: 0 })
        ));
        conn.close(CloseReason::Cleanup);
    }

    #[test]
    fn no_heartbeat_watchdog_without_a_driver() {
        let _serial = crate::conn::TEST_LOCK.lock();
        let (conn, mut w, m, _peer) = detached();
        m.advance(Duration::from_secs(60));
        assert!(w.watch_heartbeat(&conn, m.now()).is_none());
        assert_eq!(w.shared.heartbeat_misses(), 0);
        conn.close(CloseReason::Cleanup);
    }
}
