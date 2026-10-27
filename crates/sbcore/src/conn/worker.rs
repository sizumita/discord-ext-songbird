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
use super::admission::{Admission, Reservation};
use super::gateway::GwInput;
use super::shared::ConnShared;
use crate::clock::Clock;
use crate::error::{CloseReason, CoreError};
use crate::events::EventData;
use crate::limits::{HEARTBEAT_MISS, MAX_SUBS_PER_CONN, TICK_CHANNEL_CAP, TICK_MS, check};
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
    /// Race-free `MAX_SUBS_PER_CONN` admissions: `ConnShared::subscribe` (Python threads) and the
    /// streams routers create (the worker) share it.
    subs_adm: Admission,
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
    /// Makes the next worker step panic (panic-containment tests).
    #[cfg(test)]
    pub(crate) panic_next_step: std::sync::atomic::AtomicBool,
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
            subs_adm: Admission::new(),
            budget_used: AtomicUsize::new(0),
            budget_max,
            timeline_ms: AtomicU64::new(0),
            rx_counts: Mutex::new(RxCounts::default()),
            panics: AtomicU64::new(0),
            heartbeat_misses: AtomicU64::new(0),
            #[cfg(test)]
            panic_next_step: std::sync::atomic::AtomicBool::new(false),
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

    /// One of `MAX_SUBS_PER_CONN` subscription queues (explicit subscriptions and router-created
    /// streams alike). Hold the reservation until the queue is registered (`register_queue`).
    pub(crate) fn reserve_sub(&self) -> Result<Reservation<'_>, CoreError> {
        self.subs_adm
            .reserve("subscriptions", MAX_SUBS_PER_CONN, || self.open_queues())
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

    /// After a caught panic: close every subscription and rebuild the pipeline without consumers.
    /// The connection state (SSRC → user map, bot flags, pending leaves, speaking spurts, timeline,
    /// link, counters) moves into the new pipeline: Discord announces a user's SSRC once per
    /// session, so dropping the map would lose every present user's audio for good (§8.5).
    fn recover(&mut self, conn: &Arc<ConnShared>) {
        self.shared.close_queues(CloseReason::InternalError);
        let fresh = conn.new_pipeline();
        let mut old = std::mem::replace(&mut self.pipeline, fresh);
        let carry = catch_unwind(AssertUnwindSafe(|| {
            old.close_all(CloseReason::InternalError);
            old.take_carry()
        }));
        if let Ok(c) = carry {
            self.pipeline.restore_carry(c);
        }
        let _ = catch_unwind(AssertUnwindSafe(move || drop(old)));
    }

    fn step_inner(&mut self, conn: &Arc<ConnShared>, now: Instant) -> Option<Instant> {
        #[cfg(test)]
        if self.shared.panic_next_step.swap(false, Ordering::AcqRel) {
            panic!("injected worker panic");
        }
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

        // 2. Inputs, in the order Dispatch produced them: the stash and the channel, then the
        //    control overflow. While the overflow holds anything, Dispatch adds nothing to the
        //    channel (control inputs queue behind the overflow, ticks are dropped), so the whole
        //    channel is older than the overflow, and the overflow is drained only once the channel
        //    is empty.
        if let Some(input) = self.stash.take() {
            self.pipeline.on_input(input, now);
        }
        let mut taken = 0usize;
        let mut channel_empty = self.rx_closed;
        while taken < INPUT_BUDGET && !self.rx_closed {
            match self.rx_in.try_recv() {
                Ok(input) => {
                    self.pipeline.on_input(input, now);
                    taken += 1;
                }
                Err(TryRecvError::Empty) => {
                    channel_empty = true;
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    self.rx_closed = true;
                    channel_empty = true;
                }
            }
        }
        if channel_empty {
            loop {
                let item = self.overflow.lock().pop_front();
                match item {
                    Some(input) => self.pipeline.on_input(input, now),
                    None => break,
                }
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
            // The receive-timeline position at which the stall began (last real tick or link-up).
            let since_ms = conn.timeline_ms_at(since);
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

    /// §13.1: a heartbeat gap > 3 s (receive off) triggers a rate-limited `ReinstallDispatch`.
    #[test]
    fn heartbeat_gap_reinstalls_the_dispatcher_rate_limited() {
        use crate::clock::Clock;
        use crate::conn::settings::{ConnParams, RawVoiceSettings, VoiceSettings};
        use crate::wake::Doorbell;
        use std::num::NonZeroU64;
        use std::os::unix::net::UnixStream;

        let _serial = crate::conn::TEST_LOCK.lock();
        let (clock, m) = Clock::manual();
        let (a, _peer) = UnixStream::pair().unwrap();
        let p = ConnParams {
            guild_id: NonZeroU64::new(1).unwrap(),
            user_id: NonZeroU64::new(2).unwrap(),
            reconnect: true,
            settings: VoiceSettings::validate(RawVoiceSettings {
                receive: false,
                ..RawVoiceSettings::default()
            })
            .unwrap(),
            clock,
        };
        let (conn, mut w) =
            ConnShared::new_unspawned(p, Doorbell::from_unix_stream(a).unwrap()).unwrap();
        let reinstalls = || {
            conn.stats()
                .iter()
                .find(|(k, _)| *k == "dispatch_reinstalls")
                .map(|(_, v)| *v)
        };
        conn.request_join(NonZeroU64::new(10).unwrap(), false, false);
        // Within 3 s of the last heartbeat (or of the Driver's creation): nothing.
        let due = w.watch_heartbeat(&conn, m.now()).expect("watchdog armed");
        assert!(due > m.now());
        assert_eq!(reinstalls(), Some(0));
        // The (manual) clock runs 10 s ahead of every real heartbeat: handlers are lost.
        m.advance(Duration::from_secs(10));
        let next = w.watch_heartbeat(&conn, m.now());
        assert_eq!(next, Some(m.now() + WATCHDOG_REPEAT));
        assert_eq!(reinstalls(), Some(1));
        assert_eq!(w.shared.heartbeat_misses(), 1);
        // Repeated every second while the gap lasts; the reinstall itself at most every 5 s.
        m.advance(Duration::from_secs(1));
        w.watch_heartbeat(&conn, m.now());
        assert_eq!(reinstalls(), Some(1));
        assert_eq!(w.shared.heartbeat_misses(), 2);
        m.advance(Duration::from_secs(4));
        w.watch_heartbeat(&conn, m.now());
        assert_eq!(reinstalls(), Some(2));
        conn.close(CloseReason::Cleanup);
        assert!(
            w.watch_heartbeat(&conn, m.now()).is_none(),
            "no watchdog after close"
        );
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
        // `since_ms` is the timeline position where the stall began (the connection was created
        // at the same manual instant as `since`).
        assert!(matches!(
            stalls[..],
            [EventData::ReceiveStalled { since_ms: 0 }]
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
            Pop::Item(EventData::ReceiveStalled { since_ms: 5_000 })
        ));
        conn.close(CloseReason::Cleanup);
    }

    /// §5.7: a panic in a worker step closes every subscription with `internal_error`, counts in
    /// `stats()["panics"]` and rebuilds the receive pipeline empty; receiving continues.
    #[test]
    fn a_worker_panic_closes_the_streams_and_rebuilds_the_pipeline() {
        use crate::format::PCM16_24K;
        use crate::rx::{Overflow, RouterSpec, RxSource, SubSpec};

        let _serial = crate::conn::TEST_LOCK.lock();
        let (conn, mut w, m, _peer) = detached();
        let spec = SubSpec {
            source: RxSource::User(7),
            format: PCM16_24K,
            chunk_ms: 20,
            max_queue_ms: 200,
            overflow: Overflow::DropOldest,
        };
        let sub = conn.subscribe(spec.clone()).unwrap();
        let router = conn
            .subscribe_router(RouterSpec {
                format: PCM16_24K,
                chunk_ms: 20,
                max_queue_ms: 200,
                overflow: Overflow::DropOldest,
                include: None,
                ignore_bots: false,
                close_on_leave: true,
            })
            .unwrap();
        w.step(m.now());
        let stat = |k: &str| conn.stats().into_iter().find(|(n, _)| *n == k).unwrap().1;
        assert_eq!((stat("subscriptions"), stat("routers")), (1, 1));
        w.shared.panic_next_step.store(true, Ordering::Release);
        m.advance(TICK);
        assert_eq!(w.step(m.now()), Some(m.now() + TICK));
        assert_eq!(stat("panics"), 1);
        assert_eq!(sub.queue.close_reason(), Some(CloseReason::InternalError));
        assert!(matches!(
            router.queue.try_pop_or_arm(),
            crate::events::Pop::Closed(_)
        ));
        // The rebuilt pipeline serves new subscriptions.
        let again = conn.subscribe(spec).unwrap();
        m.advance(TICK);
        w.step(m.now());
        assert_eq!((stat("subscriptions"), stat("routers")), (1, 0));
        assert!(!again.queue.is_closed());
        conn.close(CloseReason::Cleanup);
        assert_eq!(
            again.queue.close_reason(),
            Some(CloseReason::ConnectionClosed)
        );
    }

    use crate::rx::raw::{RawTick, RawVoice};
    use crate::rx::{MixFilter, Overflow, RxSource, SubSpec};

    /// One tick at `at`: an RTP packet of `opus` per `(ssrc, seq)`.
    fn tick_of(at: Instant, voices: &[(u32, u16)], opus: &[u8]) -> RxInput {
        RxInput::Tick(RawTick {
            arrived: at,
            voices: voices
                .iter()
                .map(|&(ssrc, seq)| {
                    let (pkt, payload_offset, payload_end) =
                        crate::rx::test_util::RtpBuild::new(seq, ssrc).build(opus, 16);
                    RawVoice::Packet {
                        ssrc,
                        packet: bytes::Bytes::from(pkt),
                        payload_offset,
                        payload_end,
                    }
                })
                .collect(),
            silent: smallvec::SmallVec::new(),
        })
    }

    fn silent_tick(at: Instant) -> RxInput {
        tick_of(at, &[], &[])
    }

    fn drain_events(events: &crate::events::EventQueue) -> Vec<EventData> {
        std::iter::from_fn(|| match events.try_pop() {
            Pop::Item(e) => Some(e),
            _ => None,
        })
        .collect()
    }

    /// The tick channel is full when the driver reconnects: `LinkDown` takes the last slot and
    /// `LinkUp` waits in the overflow. The worker applies them in that order (the overflow after
    /// the channel), so the link is up and a later receive stall is still detected.
    #[test]
    fn a_link_up_in_the_overflow_is_applied_after_the_older_link_down() {
        let _serial = crate::conn::TEST_LOCK.lock();
        let (conn, mut w, m, _peer) = detached();
        let events = conn.subscribe_events(64, KindMask::ALL).unwrap();
        let tx = conn.rx_sender().unwrap();
        for _ in 0..TICK_CHANNEL_CAP - 1 {
            tx.try_send(silent_tick(m.now())).unwrap();
        }
        let d = conn.dispatch().inner();
        d.send_ctrl(RxInput::LinkDown); // the last slot
        d.send_ctrl(RxInput::LinkUp { ssrc: Some(5) }); // the overflow
        w.step(m.now());
        assert!(w.overflow.lock().is_empty());
        // No tick for longer than `stall_timeout` while the link is up: a stall.
        for _ in 0..150 {
            m.advance(TICK);
            w.step(m.now());
        }
        assert!(
            drain_events(&events)
                .iter()
                .any(|e| matches!(e, EventData::ReceiveStalled { .. })),
            "the stall watchdog is armed"
        );
        conn.close(CloseReason::Cleanup);
    }

    /// A user leaves and comes back (new SSRC) while the channel is full: the `Map` waits in the
    /// overflow behind the queued `UserLeft`, so the leave's prune never deletes the new mapping.
    #[test]
    fn a_map_in_the_overflow_is_applied_after_the_older_user_left() {
        const A: u64 = 7;
        let _serial = crate::conn::TEST_LOCK.lock();
        let (conn, mut w, m, _peer) = detached();
        let events = conn.subscribe_events(256, KindMask::ALL).unwrap();
        let tx = conn.rx_sender().unwrap();
        let d = conn.dispatch().inner();
        d.send_ctrl(RxInput::Map {
            ssrc: 11,
            user_id: A,
        });
        w.step(m.now());
        for _ in 0..TICK_CHANNEL_CAP - 2 {
            tx.try_send(silent_tick(m.now())).unwrap();
        }
        d.send_ctrl(RxInput::UserLeft { user_id: A });
        tx.try_send(silent_tick(m.now())).unwrap(); // full
        d.send_ctrl(RxInput::Map {
            ssrc: 21,
            user_id: A,
        });
        w.step(m.now());
        for _ in 0..75 {
            m.advance(TICK); // past the 1 s prune
            w.step(m.now());
        }
        drain_events(&events);
        let opus =
            crate::rx::test_util::opus_frames(&crate::rx::test_util::sine48(440.0, 0.3, 960 * 5));
        for (i, p) in opus.iter().enumerate() {
            m.advance(TICK);
            tx.try_send(tick_of(m.now(), &[(21, 100 + i as u16)], p))
                .unwrap();
            w.step(m.now());
        }
        assert!(
            drain_events(&events)
                .iter()
                .any(|e| matches!(e, EventData::SpeakingStarted { user_id: A, .. })),
            "the returning user's new SSRC is still mapped"
        );
        conn.close(CloseReason::Cleanup);
    }

    /// §5.7 + §8.5: the pipeline rebuilt after a caught panic keeps the SSRC → user map, the bot
    /// flags and the timeline (Discord announces a user's SSRC once per session).
    #[test]
    fn a_worker_panic_keeps_the_ssrc_map_bot_flags_and_timeline() {
        const A: u64 = 7;
        const BOT: u64 = 9;
        let _serial = crate::conn::TEST_LOCK.lock();
        let (conn, mut w, m, _peer) = detached();
        let tx = conn.rx_sender().unwrap();
        let d = conn.dispatch().inner();
        d.send_ctrl(RxInput::Map {
            ssrc: 1,
            user_id: A,
        });
        d.send_ctrl(RxInput::Map {
            ssrc: 2,
            user_id: BOT,
        });
        conn.set_users_bot(&[(BOT, true)]);
        let early = conn
            .subscribe(SubSpec {
                source: RxSource::User(A),
                format: crate::format::PCM16_24K,
                chunk_ms: 20,
                max_queue_ms: 2_000,
                overflow: Overflow::DropOldest,
            })
            .unwrap();
        for _ in 0..50 {
            m.advance(TICK);
            w.step(m.now());
        }
        let timeline = conn.timeline_ms();
        assert!(timeline >= 900);
        w.shared.panic_next_step.store(true, Ordering::Release);
        m.advance(TICK);
        w.step(m.now());
        assert_eq!(early.queue.close_reason(), Some(CloseReason::InternalError));
        assert!(
            conn.timeline_ms() >= timeline,
            "the timeline never goes back"
        );
        let spec = |source| SubSpec {
            source,
            format: crate::format::PCM16_24K,
            chunk_ms: 20,
            max_queue_ms: 2_000,
            overflow: Overflow::DropOldest,
        };
        let ua = conn.subscribe(spec(RxSource::User(A))).unwrap();
        let mix = conn
            .subscribe(spec(RxSource::Mixed(MixFilter::everyone())))
            .unwrap();
        let opus =
            crate::rx::test_util::opus_frames(&crate::rx::test_util::sine48(440.0, 0.3, 960 * 10));
        for (i, p) in opus.iter().enumerate() {
            m.advance(TICK);
            let seq = 100 + i as u16;
            tx.try_send(tick_of(m.now(), &[(1, seq), (2, seq)], p))
                .unwrap();
            w.step(m.now());
        }
        let pop_all = |s: &crate::rx::Subscription| {
            std::iter::from_fn(|| match s.queue.try_pop() {
                Pop::Item(d) => Some(d),
                _ => None,
            })
            .collect::<Vec<_>>()
        };
        assert!(
            pop_all(&ua).iter().any(|d| d.chunk.meta.speaking),
            "A's SSRC is still mapped"
        );
        let mixed = pop_all(&mix);
        assert!(mixed.iter().any(|d| d.chunk.meta.speakers.contains(&A)));
        assert!(
            mixed.iter().all(|d| !d.chunk.meta.speakers.contains(&BOT)),
            "the bot is still excluded"
        );
        conn.close(CloseReason::Cleanup);
    }

    /// A moderator moves the bot: the `Connect` to the new channel ends every presence of the old
    /// one in the receive pipeline (no `ClientDisconnect` arrives for those users), so its
    /// `listen_users` streams close and give back their slots and budget; a reconnect to the same
    /// channel keeps everything.
    #[test]
    fn moving_to_another_channel_closes_the_old_channels_router_streams() {
        use crate::rx::RouterSpec;
        const A: u64 = 7;
        let _serial = crate::conn::TEST_LOCK.lock();
        let (conn, mut w, m, _peer) = detached();
        conn.request_join(NonZeroU64::new(10).unwrap(), false, false);
        conn.on_voice_state("s", Some(10));
        conn.on_voice_server("t", Some("e.discord.media"));
        assert_eq!(conn.status().state, "connected");
        let router = conn
            .subscribe_router(RouterSpec {
                format: crate::format::PCM16_48K,
                chunk_ms: 20,
                max_queue_ms: 200,
                overflow: Overflow::DropOldest,
                include: None,
                ignore_bots: false,
                close_on_leave: true,
            })
            .unwrap();
        let tx = conn.rx_sender().unwrap();
        conn.dispatch().inner().send_ctrl(RxInput::Map {
            ssrc: 1,
            user_id: A,
        });
        let opus =
            crate::rx::test_util::opus_frames(&crate::rx::test_util::sine48(440.0, 0.3, 960 * 3));
        for (i, p) in opus.iter().enumerate() {
            m.advance(TICK);
            tx.try_send(tick_of(m.now(), &[(1, 100 + i as u16)], p))
                .unwrap();
            w.step(m.now());
        }
        let stream = match router.queue.try_pop_or_arm() {
            Pop::Item(s) => s,
            other => panic!("{other:?}"),
        };
        let stat = |k: &str| conn.stats().into_iter().find(|(n, _)| *n == k).unwrap().1;
        assert_eq!(stat("subscriptions"), 1);
        assert!(stat("receive_budget_bytes") > 0);
        // Same channel, new session: a reconnect keeps the presence.
        conn.on_voice_state("s2", Some(10));
        m.advance(TICK);
        w.step(m.now());
        assert!(!stream.queue.is_closed());
        // A moderator move to channel 11.
        conn.on_voice_state("s2", Some(11));
        conn.on_voice_server("t2", Some("e2.discord.media"));
        assert_eq!(
            (conn.status().state, conn.status().channel_id),
            ("connected", Some(11))
        );
        m.advance(TICK);
        w.step(m.now());
        assert_eq!(stream.queue.close_reason(), Some(CloseReason::UserLeft));
        drop(stream);
        m.advance(TICK);
        w.step(m.now());
        assert_eq!(
            (stat("subscriptions"), stat("receive_budget_bytes")),
            (0, 0)
        );
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
