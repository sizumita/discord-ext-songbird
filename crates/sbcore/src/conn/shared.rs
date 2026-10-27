//! [`ConnShared`]: the per-connection facade the binding calls (W2, §4, §12.3).
//!
//! Every method is synchronous and non-blocking (O(1) or O(input)); no lock is held across an
//! `.await`, a syscall or another blocking wait (I7). `native.Connection` holds the only long-lived
//! strong `Arc`; songbird handlers and tasks hold `Weak<ConnShared>` (I4).
//!
//! State-machine inputs go through [`ConnShared::apply`] (§4.6): one pure step under L2, the batch
//! enqueued under L2 → L2q in step order, and one executor at a time running the batches in order
//! after every lock is released.

use std::collections::VecDeque;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use smallvec::SmallVec;
use songbird::constants::DEFAULT_BITRATE;
use songbird::driver::Bitrate;
use songbird::tracks::TrackHandle;
use tokio::task::AbortHandle;

use super::LogLimiter;
use super::admission::{Admission, Reservation};
use super::dispatch::Dispatch;
use super::driver::{OwnedDriver, build_config, connect_deadline};
use super::gateway::{
    ActionBatch, ConnectFailure, GatewayParams, GatewayState, GwAction, GwInput, StatusSnapshot,
};
use super::ops::{OpRegistry, OpResult, TrackInfoData};
use super::settings::{BITRATE_BPS, ConnParams, VoiceSettings};
use super::worker::{ConnWorker, WorkerCmd, WorkerShared};
use crate::clock::Clock;
use crate::ctrl::{Control, CtrlQueue};
use crate::error::{CloseReason, CoreError};
use crate::events::{EventData, EventHub, EventQueue, KindMask};
use crate::limits::{
    MAX_LIVE_SOURCES_PER_CONN, MAX_PLAYING_TRACKS_PER_CONN, MAX_SUBS_PER_CONN, PREALLOCATED_TRACKS,
    SEEK_S, TICK_CHANNEL_CAP, TICK_MS, VOLUME, check,
};
use crate::runtime::{self, LiveCounter, live};
use crate::rx::pipeline::{RxConfig, RxPipeline};
use crate::rx::queue::{RxCmd, SubQueue, SubSpec, Subscription};
use crate::rx::raw::RxSender;
use crate::rx::router::{RouterHandle, RouterQueue, RouterSpec};
use crate::sync::lock_checked;
use crate::tracks::flags::{PlayOpts, TrackEndReason, TrackKind, TrackRef, loop_state};
use crate::tracks::queue::TrackQueue;
use crate::tracks::registry::TrackRegistry;
use crate::tx::encoded::{EncodedShared, EncodedSpec, spawn_encoded_with};
use crate::tx::opus_stream::{OpusQueue, OpusStreamSpec, opus_stream_input};
use crate::tx::queue::{PcmQueue, PlaybackSpec};
use crate::tx::reader::live_pcm_input;
use crate::tx::source::SourceSpec;
use crate::tx::{CloseMode, LiveSource, TrackControl, next_source_id};
use crate::wake::{Doorbell, WAKE_CONN, WAKE_OPS, WAKE_TRACKS, WakeHub};

pub use super::registry::{registry_snapshot, shutdown_all};

/// Rate limit for the DEBUG line of unlisted (phase, input) pairs.
static UNLISTED_LOG: LogLimiter = LogLimiter::new(10);

/// Connection-level counters (§11.3: `connects`, `rejoins`, `stale_connects_dropped`, …).
#[derive(Debug, Default)]
pub(crate) struct ConnStats {
    connects: AtomicU64,
    rejoins: AtomicU64,
    stale_connects_dropped: AtomicU64,
    gw_unlisted_inputs: AtomicU64,
    dispatch_reinstalls: AtomicU64,
}

/// What the executor did (ordering tests, §4.6).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Exec {
    Connect(u64),
    Link { seq: u64, up: bool },
}

/// One registered live source.
struct SourceEntry {
    src: Weak<dyn LiveSource>,
    /// Counted towards `MAX_LIVE_SOURCES_PER_CONN` (`open_*`) until it is released (closed with
    /// abort). A lazy `Encoded` queue entry is a live source too, but it is the queue's current
    /// entry and counts towards `MAX_PLAYING_TRACKS_PER_CONN` instead; an encoded stream's
    /// internal queue is registered uncounted next to its `EncodedShared` (T5 and `stop_all` reach
    /// it after the `EncodedShared` is gone). Whatever these user-facing caps count, songbird
    /// never holds more than `PREALLOCATED_TRACKS` (48) tracks: `resident_tracks`.
    counted: bool,
}

/// Everything one voice connection owns (§4.3).
pub struct ConnShared {
    me: Weak<ConnShared>,
    guild_id: NonZeroU64,
    user_id: NonZeroU64,
    reconnect: bool,
    settings: VoiceSettings,
    /// L1. `None` once torn down, and always for detached test connections.
    driver: Mutex<Option<OwnedDriver>>,
    detached: bool,
    /// L2.
    gw: Mutex<GatewayState>,
    /// L2q.
    pending: Mutex<VecDeque<ActionBatch>>,
    executing: AtomicBool,
    wake: Arc<WakeHub>,
    events: Arc<EventHub>,
    ctrl: Arc<CtrlQueue>,
    tracks: Arc<TrackRegistry>,
    queue: TrackQueue,
    worker: Arc<WorkerShared>,
    /// The tick-channel sender (the `Dispatch` holds a clone); the test hooks inject through it.
    #[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
    rx_tx: Option<RxSender>,
    dispatch: Dispatch,
    /// L10: live sources, pruned on insert, closed at T5 (`conn_closed`).
    sources: Mutex<Vec<SourceEntry>>,
    /// L10: connect tasks (op tasks live in `ops`). The ConnWorker exits on the shutdown flag.
    tasks: Mutex<Vec<AbortHandle>>,
    ops: OpRegistry,
    /// Users first seen by the receive pipeline, awaiting bot classification (W3 pushes).
    unclassified: Arc<Mutex<Vec<u64>>>,
    closing: Arc<AtomicBool>,
    send_muted: Arc<AtomicBool>,
    /// The creating process; teardown is skipped in any other process (§4.10).
    pid: u32,
    clock: Clock,
    /// Timeline origin (§8.8) and housekeeping tick origin.
    t0: Instant,
    next_sub_id: AtomicU64,
    stats: ConnStats,
    /// Race-free I6 admissions (free-threaded callers): `MAX_PLAYING_TRACKS_PER_CONN`,
    /// `MAX_LIVE_SOURCES_PER_CONN`, `MAX_SUBS_PER_CONN` for routers (subscription queues are
    /// admitted by `WorkerShared::reserve_sub`, which router-created streams share).
    playing_adm: Admission,
    live_adm: Admission,
    /// `PREALLOCATED_TRACKS` tracks resident in songbird (`TrackRegistry::resident`), reserved
    /// with either of the two above.
    resident_adm: Admission,
    routers_adm: Admission,
    #[cfg(feature = "test-hooks")]
    pub(super) det: super::detached::DetachedState,
    #[cfg(test)]
    exec_log: Mutex<Vec<Exec>>,
    /// Makes the next `execute` panic (executor unwind-safety test).
    #[cfg(test)]
    panic_next_execute: AtomicBool,
    _live: LiveCounter,
}

impl std::fmt::Debug for ConnShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnShared")
            .field("guild_id", &self.guild_id)
            .field("state", &self.status().state)
            .field("closing", &self.is_closed())
            .finish()
    }
}

impl ConnShared {
    /// Builds the Driver, the Dispatch and the ConnWorker (spawned on the RX runtime) and registers
    /// the connection. The doorbell is dropped (closed) on failure.
    pub fn new(p: ConnParams, bell: Doorbell) -> Result<Arc<Self>, CoreError> {
        let (conn, worker) = Self::build(p, bell, false, std::process::id())?;
        runtime::rx().spawn(worker.run());
        Ok(conn)
    }

    /// `new` with the ConnWorker returned instead of spawned (worker tests with a real Driver).
    #[cfg(test)]
    pub(crate) fn new_unspawned(
        p: ConnParams,
        bell: Doorbell,
    ) -> Result<(Arc<Self>, ConnWorker), CoreError> {
        Self::build(p, bell, false, std::process::id())
    }

    /// `new` as if created by process `pid` (fork tests: a foreign pid behaves like a forked child).
    #[cfg(test)]
    pub(crate) fn new_with_pid(
        p: ConnParams,
        bell: Doorbell,
        pid: u32,
    ) -> Result<Arc<Self>, CoreError> {
        let (conn, worker) = Self::build(p, bell, false, pid)?;
        runtime::rx().spawn(worker.run());
        Ok(conn)
    }

    /// A full connection with **no songbird Driver** and the worker returned to the caller instead
    /// of spawned (`native._testing.detached_connection`, §13.2).
    #[cfg(feature = "test-hooks")]
    pub fn new_detached(
        p: ConnParams,
        bell: Doorbell,
    ) -> Result<(Arc<Self>, ConnWorker), CoreError> {
        Self::build(p, bell, true, std::process::id())
    }

    fn build(
        p: ConnParams,
        bell: Doorbell,
        detached: bool,
        pid: u32,
    ) -> Result<(Arc<Self>, ConnWorker), CoreError> {
        runtime::check_not_forked()?;
        let ConnParams {
            guild_id,
            user_id,
            reconnect,
            settings,
            clock,
        } = p;
        let wake = WakeHub::new(bell);
        let closing = Arc::new(AtomicBool::new(false));
        let events = EventHub::new();
        let ctrl = CtrlQueue::new(wake.clone());
        let tracks = Arc::new(TrackRegistry::new(closing.clone()));
        let worker = WorkerShared::new(settings.max_receive_bytes);
        let overflow = Arc::new(Mutex::new(VecDeque::new()));
        let (tx, rx_in) = tokio::sync::mpsc::channel(TICK_CHANNEL_CAP);
        // Open with receive off too: no VoiceTick handler is registered, but the WebSocket events
        // (SpeakingStateUpdate, ClientDisconnect) still reach the pipeline as control inputs, so
        // `UserMapped`/`UserLeft` keep working (§8.5, §9.3).
        let rx_tx = Some(tx);
        let driver = (!detached).then(|| OwnedDriver::new(build_config(&settings, reconnect)));
        let gw = GatewayState::new(GatewayParams::new(
            guild_id,
            user_id,
            reconnect,
            settings.rejoin_attempts,
            settings.auto_recover,
        ));
        let t0 = clock.now();
        let conn = Arc::new_cyclic(|me: &Weak<ConnShared>| {
            let dispatch = Dispatch::new(
                me.clone(),
                rx_tx.clone(),
                Arc::downgrade(&tracks),
                overflow.clone(),
            );
            let mut driver = driver;
            if let Some(d) = driver.as_mut() {
                d.install_dispatch(&dispatch, settings.receive);
                if let Some(b) = settings.bitrate {
                    d.set_bitrate(Bitrate::Bits(b));
                }
            }
            ConnShared {
                me: me.clone(),
                guild_id,
                user_id,
                reconnect,
                settings,
                driver: Mutex::new(driver),
                detached,
                gw: Mutex::new(gw),
                pending: Mutex::new(VecDeque::new()),
                executing: AtomicBool::new(false),
                wake,
                events,
                ctrl,
                tracks,
                queue: TrackQueue::new(),
                worker,
                rx_tx,
                dispatch,
                sources: Mutex::new(Vec::new()),
                tasks: Mutex::new(Vec::new()),
                ops: OpRegistry::new(),
                unclassified: Arc::new(Mutex::new(Vec::new())),
                closing,
                send_muted: Arc::new(AtomicBool::new(false)),
                pid,
                clock,
                t0,
                next_sub_id: AtomicU64::new(1),
                stats: ConnStats::default(),
                playing_adm: Admission::new(),
                live_adm: Admission::new(),
                resident_adm: Admission::new(),
                routers_adm: Admission::new(),
                #[cfg(feature = "test-hooks")]
                det: super::detached::DetachedState::new(),
                #[cfg(test)]
                exec_log: Mutex::new(Vec::new()),
                #[cfg(test)]
                panic_next_execute: AtomicBool::new(false),
                _live: LiveCounter::new(&live::CONN_SHARED),
            }
        });
        conn.queue
            .attach(Arc::downgrade(&conn.tracks), Arc::downgrade(&conn.worker));
        let worker = ConnWorker::new(
            Arc::downgrade(&conn),
            conn.worker.clone(),
            conn.new_pipeline(),
            rx_in,
            overflow,
            conn.clock.clone(),
        );
        super::registry::register(&conn);
        Ok((conn, worker))
    }

    /// A fresh receive pipeline (creation, and rebuild after a caught worker panic), attached to
    /// the connection's `WorkerShared` so router-created streams are budget-checked, registered
    /// for T6 and counted towards `MAX_SUBS_PER_CONN`.
    pub(crate) fn new_pipeline(&self) -> RxPipeline {
        let s = &self.settings;
        let mut p = RxPipeline::new(
            RxConfig {
                enabled: s.receive,
                hangover_ms: s.hangover_ms,
                voice_threshold_dbfs: s.voice_threshold_dbfs,
                max_decoded_users: s.max_decoded_users,
                decode_state_timeout: s.decode_state_timeout,
                stall_timeout: s.stall_timeout,
                t0: self.t0,
            },
            self.events.clone(),
            self.ctrl.clone(),
            self.unclassified.clone(),
        );
        p.attach_worker(Arc::downgrade(&self.worker));
        p
    }

    // ── gateway ──────────────────────────────────────────────────────────────────────────────────

    /// Idle → Joining (`Join`); mirrors `self_mute` into the Driver.
    pub fn request_join(&self, channel_id: NonZeroU64, self_mute: bool, self_deaf: bool) {
        self.apply(GwInput::Join {
            channel: channel_id,
            self_mute,
            self_deaf,
        });
    }

    /// `Move` (from `move_to`).
    pub fn request_move(&self, channel_id: NonZeroU64) {
        self.apply(GwInput::Move {
            channel: channel_id,
        });
    }

    /// `BeginRejoin` (from `reconnect()`).
    pub fn begin_rejoin(&self) {
        self.stats.rejoins.fetch_add(1, Ordering::Relaxed);
        self.apply(GwInput::BeginRejoin);
    }

    /// `Leave`: the following `VoiceState(None)` closes with `Requested`.
    pub fn request_leave(&self) {
        self.apply(GwInput::Leave);
    }

    /// Forwards discord.py's `on_voice_state_update` for our own user.
    pub fn on_voice_state(&self, session_id: &str, channel_id: Option<u64>) {
        // Secrets: the session id is logged only as a length.
        ::log::debug!(
            "voice state update: channel={channel_id:?} session_len={}",
            session_id.len()
        );
        self.apply(GwInput::VoiceState {
            session_id: session_id.into(),
            channel: channel_id,
        });
    }

    /// Forwards discord.py's `on_voice_server_update` (`endpoint=None`: server reallocating).
    pub fn on_voice_server(&self, token: &str, endpoint: Option<&str>) {
        // Secrets: the token is never logged; the endpoint only as a host.
        ::log::debug!(
            "voice server update: host={:?}",
            endpoint.and_then(super::gateway::normalize_endpoint)
        );
        self.apply(GwInput::VoiceServer {
            token: token.into(),
            endpoint: endpoint.map(Into::into),
        });
    }

    /// `SetFlags` plus the `send_muted` mirror read by every live source (§7.8).
    pub fn set_self_flags(&self, self_mute: bool, self_deaf: bool) {
        self.apply(GwInput::SetFlags {
            self_mute,
            self_deaf,
        });
    }

    /// A copy of the public status.
    pub fn status(&self) -> StatusSnapshot {
        lock_checked(&self.gw).status()
    }

    /// Receive timeline position (20 ms ticks since the connection was created).
    pub fn timeline_ms(&self) -> u64 {
        self.worker.timeline_ms()
    }

    /// The timeline position (whole 20 ms ticks since the origin `t0`) of the instant `t`.
    pub(crate) fn timeline_ms_at(&self, t: Instant) -> u64 {
        let ms =
            u64::try_from(t.saturating_duration_since(self.t0).as_millis()).unwrap_or(u64::MAX);
        ms - ms % u64::from(TICK_MS)
    }

    // ── signalling to Python ─────────────────────────────────────────────────────────────────────

    /// Ready wake ids (Python only, after `sock_recv` returned).
    pub fn take_ready(&self, out: &mut Vec<u8>) {
        self.wake.take_ready(out);
    }

    /// Pending control slots (at most `max`; the others stay pending).
    pub fn take_controls(&self, max: usize, out: &mut Vec<Control>) {
        self.ctrl.take_up_to(max, out);
    }

    /// Users first seen since the last call, awaiting bot classification (`ClassifyUsers`).
    pub fn take_unclassified_users(&self) -> Vec<u64> {
        std::mem::take(&mut *lock_checked(&self.unclassified))
    }

    /// Records which users are bots (`ignore_bots` filters).
    pub fn set_users_bot(&self, users: &[(u64, bool)]) {
        if users.is_empty() || self.is_closed() {
            return;
        }
        self.worker
            .push(WorkerCmd::Rx(RxCmd::SetBots(users.to_vec())));
    }

    // ── receive ──────────────────────────────────────────────────────────────────────────────────

    /// A per-user or mixed subscription (`ReceiveDisabled` with `receive=False`).
    pub fn subscribe(&self, spec: SubSpec) -> Result<Subscription, CoreError> {
        self.ensure_open()?;
        if !self.settings.receive {
            return Err(CoreError::ReceiveDisabled);
        }
        spec.validate()?;
        // Held until the queue is registered (visible to `open_queues`).
        let _slot = self.worker.reserve_sub()?;
        let budget = self.worker.reserve_budget(spec.budget_bytes()?)?;
        let token = self.wake.alloc()?;
        let queue = SubQueue::new(&spec, token);
        self.worker.register_queue(&queue);
        if self.is_closed() {
            // T6 may already have swept the registered queues.
            queue.close(CloseReason::ConnectionClosed);
            return Err(self.closed_err());
        }
        let id = self.next_sub_id.fetch_add(1, Ordering::Relaxed);
        self.worker.push(WorkerCmd::Rx(RxCmd::AddSub {
            id,
            spec,
            queue: queue.clone(),
        }));
        Ok(Subscription::new(
            id,
            queue,
            Arc::downgrade(&self.worker),
            budget,
        ))
    }

    /// A `listen_users` router.
    pub fn subscribe_router(&self, spec: RouterSpec) -> Result<RouterHandle, CoreError> {
        self.ensure_open()?;
        if !self.settings.receive {
            return Err(CoreError::ReceiveDisabled);
        }
        // The streams it creates use these parameters: validate them like a subscription, and
        // refuse a router whose single stream could never fit the receive budget.
        spec.validate()?;
        check(
            "max_receive_mb",
            spec.stream_budget_bytes()?,
            0..=self.settings.max_receive_bytes,
        )?;
        // Held until the router is registered (visible to `open_routers`).
        let _slot = self
            .routers_adm
            .reserve("routers", MAX_SUBS_PER_CONN, || self.worker.open_routers())?;
        let token = self.wake.alloc()?;
        let queue = RouterQueue::new(token);
        self.worker.register_router(&queue);
        if self.is_closed() {
            queue.close(true);
            return Err(self.closed_err());
        }
        let id = self.next_sub_id.fetch_add(1, Ordering::Relaxed);
        self.worker.push(WorkerCmd::Rx(RxCmd::AddRouter {
            id,
            spec,
            queue: queue.clone(),
        }));
        Ok(RouterHandle { id, queue })
    }

    /// An event stream.
    pub fn subscribe_events(
        &self,
        cap: usize,
        kinds: KindMask,
    ) -> Result<Arc<EventQueue>, CoreError> {
        self.ensure_open()?;
        let token = self.wake.alloc()?;
        match self.events.subscribe(cap, kinds, token) {
            Err(CoreError::ConnectionClosed(_)) => Err(self.closed_err()),
            r => r,
        }
    }

    // ── send ─────────────────────────────────────────────────────────────────────────────────────

    /// A realtime PCM playback (starts paused until the first accepted write, §4.9).
    pub fn open_playback(
        &self,
        spec: PlaybackSpec,
    ) -> Result<(Arc<PcmQueue>, TrackRef), CoreError> {
        self.ensure_open()?;
        spec.validate()?;
        let _slot = self.reserve_live()?;
        let token = self.wake.alloc()?;
        let id = next_source_id();
        let q = PcmQueue::new(
            id,
            &spec,
            token,
            self.clock.clone(),
            self.send_muted.clone(),
        )?;
        let (track, done) = self.tracks.register(TrackKind::Playback);
        let input = live_pcm_input(q.clone(), done, false)?;
        // Volume 1.0: the decoder applies the playback's volume (5 ms ramp).
        let ctl = match self.start_live(&track, input, Some(id), 1.0) {
            Ok(ctl) => ctl,
            Err(e) => {
                q.conn_closed();
                return Err(e);
            }
        };
        q.attach_track(ctl);
        let weak: Weak<dyn LiveSource> = Arc::downgrade(&q) as Weak<dyn LiveSource>;
        track.set_live(weak.clone());
        self.add_source(weak, true);
        Ok((q, track))
    }

    /// A push-based encoded stream with its decode thread.
    pub fn open_encoded(
        &self,
        spec: EncodedSpec,
    ) -> Result<(Arc<EncodedShared>, TrackRef), CoreError> {
        self.ensure_open()?;
        check("volume", spec.volume, VOLUME)?;
        let _slot = self.reserve_live()?;
        let token = self.wake.alloc()?;
        let (track, done) = self.tracks.register(TrackKind::EncodedStream);
        let (shared, input) = spawn_encoded_with(
            spec,
            token,
            done,
            self.send_muted.clone(),
            self.clock.clone(),
        )?;
        // Volume 1.0: the decoder applies the stream's volume. The first decoded frame resumes
        // the paused track through this control.
        let ctl = match self.start_live(&track, input, Some(shared.id()), 1.0) {
            Ok(ctl) => ctl,
            Err(e) => {
                // Ends the decode thread (it waits on the ring otherwise).
                shared.conn_closed();
                return Err(e);
            }
        };
        shared.attach_track(ctl);
        // Pause, resume and volume go to the internal queue, which the reader keeps alive: the
        // `EncodedShared` is freed once its decode thread and the Python sink are gone.
        track.set_live(shared.live_queue());
        self.add_source(Arc::downgrade(&shared) as Weak<dyn LiveSource>, true);
        self.add_source(shared.live_queue(), false);
        Ok((shared, track))
    }

    /// A push-based Opus packet stream.
    pub fn open_opus_stream(
        &self,
        spec: OpusStreamSpec,
    ) -> Result<(Arc<OpusQueue>, TrackRef), CoreError> {
        self.ensure_open()?;
        check("volume", spec.volume, VOLUME)?;
        let _slot = self.reserve_live()?;
        let token = self.wake.alloc()?;
        let q = OpusQueue::new(&spec, token, self.send_muted.clone())?;
        let (track, done) = self.tracks.register(TrackKind::OpusStream);
        let input = opus_stream_input(q.clone(), done)?;
        // songbird decodes Opus streams, so the track carries the stream's volume. The first
        // `send` resumes the paused track through this control.
        let ctl = match self.start_live(&track, input, Some(q.id()), q.volume()) {
            Ok(ctl) => ctl,
            Err(e) => {
                q.conn_closed();
                return Err(e);
            }
        };
        q.attach_track(ctl);
        let weak: Weak<dyn LiveSource> = Arc::downgrade(&q) as Weak<dyn LiveSource>;
        // The user pause lives in the queue: a direct songbird pause would be undone by the next
        // `send` (or the idle pause's resume).
        track.set_live(weak.clone());
        self.add_source(weak, true);
        Ok((q, track))
    }

    /// Plays a memory source now (`MAX_PLAYING_TRACKS_PER_CONN`).
    pub fn play(&self, src: &SourceSpec, opts: PlayOpts) -> Result<TrackRef, CoreError> {
        self.ensure_open()?;
        if matches!(src, SourceSpec::Encoded(_)) {
            return Err(CoreError::Track(
                "encoded streams can only be queued (enqueue_stream)".into(),
            ));
        }
        check("volume", opts.volume, VOLUME)?;
        loop_state(opts.loops)?;
        // Held until the track is started (visible to `TrackRegistry::playing`).
        let _slot = self.reserve_playing()?;
        let (track, done) = self.tracks.register(src.kind());
        let (input, _) = src.to_input_with(
            done,
            &self.wake,
            self.send_muted.clone(),
            self.clock.clone(),
        )?;
        self.start_track(&track, input, opts)?;
        Ok(track)
    }

    /// The sequential track queue.
    pub fn queue(&self) -> &TrackQueue {
        &self.queue
    }

    /// The connection's track registry (`TrackQueue::enqueue` registers entries in it).
    pub fn tracks(&self) -> &TrackRegistry {
        &self.tracks
    }

    /// The encoded stream of a lazy `Encoded` queue entry that became current (once).
    pub fn queue_take_stream(&self, t: &TrackRef) -> Option<Arc<EncodedShared>> {
        self.queue.take_stream(t)
    }

    /// Stops every track, queue entry and live stream; the connection stays up.
    ///
    /// `Driver::stop()` (`SetTrack(None)`) fires no `End` event, so the stopped tracks resolve
    /// through the `DROPPED` grace (§7.10).
    pub fn stop_all(&self) {
        if self.is_closed() {
            return;
        }
        // `stop()` (not just `clear()`): also an entry the worker is starting right now ends.
        self.queue.stop();
        let sources: Vec<Arc<dyn LiveSource>> = lock_checked(&self.sources)
            .iter()
            .filter_map(|e| e.src.upgrade())
            .collect();
        for s in &sources {
            s.close(CloseMode::Abort);
        }
        self.with_driver(|d| d.stop());
        #[cfg(feature = "test-hooks")]
        self.drop_parked();
        self.worker.wake();
    }

    /// `None` = songbird default (128 kbps); otherwise 6 000..=510 000 bps.
    pub fn set_bitrate(&self, bps: Option<i32>) -> Result<(), CoreError> {
        self.ensure_open()?;
        let b = match bps {
            Some(b) => Bitrate::Bits(
                i32::try_from(check("bitrate", i64::from(b), BITRATE_BPS)?)
                    .map_err(|_| CoreError::invalid("bitrate out of range"))?,
            ),
            None => DEFAULT_BITRATE,
        };
        self.with_driver(|d| d.set_bitrate(b));
        Ok(())
    }

    // ── track ops ────────────────────────────────────────────────────────────────────────────────

    /// songbird's handle of a started track (`TrackError` otherwise).
    fn songbird_handle(t: &TrackRef) -> Result<TrackHandle, CoreError> {
        match t.handle.get() {
            Some(h) => Ok(h.clone()),
            None if t.is_started() => Err(CoreError::Track(
                "track has no songbird handle (detached connection)".into(),
            )),
            None if t.flags.ended().is_some() => Err(CoreError::Track("track has ended".into())),
            None => Err(CoreError::Track("track has not started".into())),
        }
    }

    /// Starts a `get_info` op; returns the op id. songbird answers only on a live mixer, so a
    /// parked one times out (`OpResult::TimedOut`).
    pub fn start_track_info(&self, t: &TrackRef, timeout: Duration) -> Result<u32, CoreError> {
        self.ensure_open()?;
        #[cfg(feature = "test-hooks")]
        if let Some(fut) = super::detached::info_op(t, self.det.ops_answered(), timeout) {
            let id = self.ops.start(&self.wake, fut)?;
            self.fail_ops_if_closed();
            return Ok(id);
        }
        let h = Self::songbird_handle(t)?;
        // A live PCM source applies its volume in its decoder (its songbird track stays at 1.0).
        let track = t.clone();
        let id = self.ops.start(&self.wake, async move {
            match tokio::time::timeout(timeout, h.get_info()).await {
                Ok(Ok(state)) => {
                    let mut info = TrackInfoData::from_state(&state);
                    if let Some(v) = track.source_volume() {
                        info.volume = v;
                    }
                    OpResult::Info(info)
                }
                Ok(Err(e)) => OpResult::Err(CoreError::Track(e.to_string().into())),
                Err(_) => OpResult::TimedOut,
            }
        })?;
        self.fail_ops_if_closed();
        Ok(id)
    }

    /// An op started while the connection closes: T8's `fail_all` may already have run (the
    /// slots lock orders this check after T0 whenever `fail_all` missed the op), so the op is
    /// resolved now instead of running until its timeout.
    fn fail_ops_if_closed(&self) {
        if self.is_closed() {
            self.ops.fail_all();
            self.wake.notify(WAKE_OPS);
        }
    }

    /// Starts a seek op; returns the op id (`TrackError` for live sources and pending entries).
    pub fn start_track_seek(
        &self,
        t: &TrackRef,
        pos: Duration,
        timeout: Duration,
    ) -> Result<u32, CoreError> {
        self.ensure_open()?;
        check("seconds", pos.as_secs_f64(), SEEK_S)?;
        if t.flags.kind.is_live() {
            return Err(CoreError::Track("live sources cannot seek".into()));
        }
        #[cfg(feature = "test-hooks")]
        if let Some(fut) = super::detached::seek_op(t, pos, self.det.ops_answered(), timeout) {
            let id = self.ops.start(&self.wake, fut)?;
            self.fail_ops_if_closed();
            return Ok(id);
        }
        let h = Self::songbird_handle(t)?;
        let id = self.ops.start(&self.wake, async move {
            match tokio::time::timeout(timeout, h.seek(pos).result_async()).await {
                Ok(Ok(d)) => OpResult::Seeked(d),
                Ok(Err(e)) => OpResult::Err(CoreError::Track(e.to_string().into())),
                Err(_) => OpResult::TimedOut,
            }
        })?;
        self.fail_ops_if_closed();
        Ok(id)
    }

    /// Finished op results.
    pub fn take_op_results(&self, out: &mut Vec<(u32, OpResult)>) {
        self.ops.take_results(out);
    }

    // ── lifecycle ────────────────────────────────────────────────────────────────────────────────

    /// Complete teardown T0–T9 (§4.7). Idempotent and non-blocking; safe from `Drop` on any thread
    /// and during interpreter finalisation. Never takes L2 while running actions, never waits for
    /// another thread.
    pub fn close(&self, reason: CloseReason) {
        // T0: before taking any lock.
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        // §4.10: in a forked child (or any foreign process) leak everything: the doorbell and the
        // Driver are shared with the parent, and locks may be held by threads that do not exist.
        if runtime::in_foreign_process(self.pid) {
            return;
        }
        // T1: phase = Closed(reason) (the first reason wins), copy the status.
        let (reason, status) = {
            let mut gw = lock_checked(&self.gw);
            let r = gw.force_close(reason);
            (r, gw.status())
        };
        // T2–T4: take the Driver under L1, tear it down outside every lock.
        let driver = lock_checked(&self.driver).take();
        if let Some(d) = driver {
            d.teardown();
        }
        // T5: close every live source (queued audio freed now; writers get
        // StreamClosed("connection_closed"), waiters are woken).
        let sources: Vec<SourceEntry> = std::mem::take(&mut *lock_checked(&self.sources));
        for s in sources.iter().filter_map(|e| e.src.upgrade()) {
            s.conn_closed();
        }
        // T6 (synchronous): close every subscription/router queue, then the event queues with the
        // final Disconnected event, then stop the worker.
        self.worker.close_queues(CloseReason::ConnectionClosed);
        self.events.close_all(Some(&EventData::Disconnected {
            reason: reason.as_str(),
            code: status.close_code,
            will_retry: false,
        }));
        self.worker.shutdown();
        // T7: abort the connect tasks (op tasks are aborted by `fail_all`).
        let tasks: Vec<AbortHandle> = std::mem::take(&mut *lock_checked(&self.tasks));
        for t in &tasks {
            t.abort();
        }
        // T8: resolve every unfinished track, fail pending ops, set the sticky Closed control.
        self.tracks.finish_all(TrackEndReason::Disconnected);
        self.queue.close_all(TrackEndReason::Disconnected);
        #[cfg(feature = "test-hooks")]
        self.drop_parked();
        self.ops.fail_all();
        self.ctrl.set_closed(reason);
        self.wake.notify(WAKE_TRACKS);
        self.wake.notify(WAKE_OPS);
        // T9: last status wake-up, then EOF on the doorbell; leave the registry.
        self.wake.notify(WAKE_CONN);
        self.wake.close();
        super::registry::unregister(self);
        ::log::debug!("voice connection closed: {}", reason.as_str());
    }

    pub fn is_closed(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// `SongbirdClient.stats()` counters (§11.3).
    pub fn stats(&self) -> Vec<(&'static str, u64)> {
        let rx = self.worker.rx_counts();
        let d = self.dispatch.counters();
        let ld = |a: &AtomicU64| a.load(Ordering::Relaxed);
        vec![
            ("ticks_real", rx.ticks_real),
            ("ticks_synthetic", rx.ticks_synthetic),
            ("ticks_dropped", ld(&d.ticks_dropped)),
            ("ticks_skipped", rx.ticks_skipped),
            ("bad_packets", rx.bad_packets),
            ("decode_errors", rx.decode_errors),
            ("plc_frames", rx.plc_frames),
            ("carry_overflow_frames", rx.carry_overflow_frames),
            ("dave_trailers_stripped", rx.dave_trailers_stripped),
            ("decoders_active", rx.decoders_active),
            ("subscriptions", rx.subscriptions),
            ("routers", rx.routers),
            ("lanes", rx.lanes),
            ("dispatch_reinstalls", ld(&self.stats.dispatch_reinstalls)),
            ("dispatch_panics", ld(&d.panics)),
            ("ctrl_dropped", ld(&d.ctrl_dropped)),
            ("heartbeat_misses", self.worker.heartbeat_misses()),
            (
                "panics",
                self.worker
                    .panics()
                    .saturating_add(ld(&d.panics))
                    .saturating_add(self.tracks.panics()),
            ),
            ("connects", ld(&self.stats.connects)),
            ("rejoins", ld(&self.stats.rejoins)),
            (
                "stale_connects_dropped",
                ld(&self.stats.stale_connects_dropped),
            ),
            ("gw_unlisted_inputs", ld(&self.stats.gw_unlisted_inputs)),
            ("pending_ops", self.ops.pending() as u64),
            ("receive_budget_bytes", self.worker.budget_used() as u64),
        ]
    }

    /// Steps the state machine under L2 and executes the actions in step order (§4.6).
    pub(crate) fn apply(&self, input: GwInput) {
        if self.closing.load(Ordering::Acquire) {
            return;
        }
        let kind = input_kind(&input);
        let (unlisted, rearmed) = {
            let mut gw = lock_checked(&self.gw); // L2
            let before = gw.unlisted_inputs();
            let deadline_before = gw.next_deadline();
            let mut batch: SmallVec<[GwAction; 4]> = SmallVec::new();
            gw.step(input, self.clock.now(), &mut batch); // pure; no I/O
            if !batch.is_empty() {
                // L2 → L2q: enqueue order == step order.
                let seq = gw.next_action_seq();
                lock_checked(&self.pending).push_back(ActionBatch {
                    seq,
                    actions: batch,
                });
            }
            let deadline_after = gw.next_deadline();
            (
                gw.unlisted_inputs() != before,
                deadline_after.is_some() && deadline_after != deadline_before,
            )
        }; // L2 released HERE
        if unlisted {
            self.stats
                .gw_unlisted_inputs
                .fetch_add(1, Ordering::Relaxed);
            if UNLISTED_LOG.allow() {
                ::log::debug!("gateway input {kind} ignored in the current phase");
            }
        }
        self.run_pending();
        if rearmed {
            // Gateway deadlines (rejoin backoff, Rejoining timeout, move fallback) are only checked
            // by the ConnWorker's step. A step on another thread (the connect task, Dispatch, a
            // Python thread) that arms a new one must wake the worker, or it sleeps until its
            // next unrelated timer (the heartbeat watchdog, up to 3 s) and the deadline fires
            // late. `notify_one` stores a permit, so the wake is never lost.
            self.worker.wake();
        }
    }

    /// Flat combining: whoever wins `executing` drains every queued batch in order; losers return
    /// at once, because their batch is already queued and the winner will run it.
    fn run_pending(&self) {
        while self
            .executing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // Clears `executing` even if an action unwinds (the caller's `catch_unwind` in
            // `Dispatch::act`, the worker tick or pyo3): a stuck flag would leave every later
            // batch queued forever and the state machine without effects.
            let running = Executing(&self.executing);
            loop {
                let next = lock_checked(&self.pending).pop_front(); // L2q held only for the pop
                let Some(b) = next else {
                    break;
                };
                for a in b.actions {
                    self.execute(a, b.seq);
                }
            }
            drop(running); // executing = false
            // Re-check: a batch queued after our last pop, whose producer lost the CAS.
            if lock_checked(&self.pending).is_empty() {
                break;
            }
        }
    }

    /// Runs one action. Never called while L2 or L2q is held. After close, only `Teardown` runs.
    fn execute(&self, a: GwAction, seq: u64) {
        #[cfg(test)]
        if self.panic_next_execute.swap(false, Ordering::AcqRel) {
            panic!("injected executor panic");
        }
        if self.closing.load(Ordering::Acquire) && !matches!(a, GwAction::Teardown(_)) {
            return;
        }
        match a {
            GwAction::Connect { info, generation } => self.exec_connect(info, generation),
            GwAction::SetSendMute(m) => {
                self.send_muted.store(m, Ordering::Release);
                self.with_driver(|d| d.mute(m));
            }
            GwAction::Emit(mut ev) => {
                if let EventData::Connected { timeline_ms, .. } = &mut ev {
                    *timeline_ms = self.timeline_ms();
                }
                self.events.emit(&ev);
            }
            GwAction::Control(c) => {
                if matches!(c, Control::Rejoin { .. }) {
                    self.stats.rejoins.fetch_add(1, Ordering::Relaxed);
                }
                self.ctrl.set(c);
            }
            GwAction::RxLink { up, ssrc } => {
                #[cfg(test)]
                self.log_exec(Exec::Link { seq, up });
                self.worker.push(WorkerCmd::Link { up, ssrc, seq });
            }
            GwAction::ReinstallDispatch => {
                self.stats
                    .dispatch_reinstalls
                    .fetch_add(1, Ordering::Relaxed);
                self.with_driver(|d| d.reinstall_dispatch(&self.dispatch, self.settings.receive));
                ::log::info!("voice event handlers re-installed (watchdog)");
            }
            GwAction::Teardown(r) => self.close(r),
            GwAction::NotifyStatus => self.wake.notify(WAKE_CONN),
        }
    }

    /// `Connect{info, gen}`: hand the info to the Driver under L1 (a stale generation is dropped),
    /// then await the result on RX under a timeout and feed it back as `ConnectResult`.
    fn exec_connect(&self, info: songbird::ConnectionInfo, generation: u64) {
        #[cfg(test)]
        self.log_exec(Exec::Connect(generation));
        self.stats.connects.fetch_add(1, Ordering::Relaxed);
        // Before the Driver starts the connection, so the receive pipeline sees a channel change
        // ahead of every mapping and tick of the new session (§8.5).
        self.dispatch.media_channel(info.channel_id.0.get());
        let fut = match self.with_driver(|d| d.connect(info, generation)) {
            Some(Some(fut)) => fut,
            Some(None) => {
                self.stats
                    .stale_connects_dropped
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            None => {
                #[cfg(feature = "test-hooks")]
                if self.detached && self.det.auto_connect() {
                    // Detached connections connect instantly (tests inject failures directly).
                    self.apply(GwInput::ConnectResult {
                        generation,
                        result: Ok(()),
                    });
                }
                return;
            }
        };
        let me = self.me.clone();
        let deadline = connect_deadline(&self.settings, self.reconnect);
        let task = runtime::rx().spawn(async move {
            let result = match tokio::time::timeout(deadline, fut).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(songbird::error::ConnectionError::AttemptDiscarded)) => {
                    Err(ConnectFailure::AttemptDiscarded)
                }
                Ok(Err(e)) => Err(ConnectFailure::Other(e.to_string().into())),
                Err(_) => Err(ConnectFailure::TimedOut),
            };
            if let Err(e) = &result {
                ::log::debug!("voice connect attempt {generation} failed: {e:?}");
            }
            if let Some(conn) = me.upgrade() {
                conn.apply(GwInput::ConnectResult { generation, result });
            }
        });
        let mut tasks = lock_checked(&self.tasks);
        tasks.retain(|t| !t.is_finished());
        tasks.push(task.abort_handle());
        drop(tasks);
        if self.is_closed() {
            // T7 may already have run.
            task.abort();
        }
    }

    #[cfg(test)]
    fn log_exec(&self, e: Exec) {
        let mut log = self.exec_log.lock();
        if log.len() < (1 << 20) {
            log.push(e);
        }
    }

    #[cfg(test)]
    #[allow(dead_code)] // used by the test-hooks tests
    pub(crate) fn exec_log(&self) -> Vec<Exec> {
        self.exec_log.lock().clone()
    }

    /// Housekeeping for tracks and the queue (finalisation, `TrackEnded`, advancing), run by the
    /// ConnWorker every tick while tracks may end.
    pub(crate) fn poll_tracks(&self, now: Instant) {
        let tick = u64::try_from(now.saturating_duration_since(self.t0).as_millis())
            .unwrap_or(u64::MAX)
            / u64::from(TICK_MS);
        let mut ended: Vec<(u64, TrackEndReason)> = Vec::new();
        self.tracks.poll_ended(tick, &mut ended);
        if !ended.is_empty() {
            for (track_id, reason) in &ended {
                self.events.emit(&EventData::TrackEnded {
                    track_id: *track_id,
                    reason: *reason,
                });
            }
            self.wake.notify(WAKE_TRACKS);
        }
        if !self.is_closed() && self.queue.needs_advance() {
            self.queue.advance(self);
        }
    }

    /// Whether the worker must keep polling tracks every tick.
    pub(crate) fn tracks_need_poll(&self) -> bool {
        self.tracks.needs_poll() || self.queue.needs_advance()
    }

    /// Rings `WAKE_TRACKS` (a lazy encoded entry became current).
    pub(crate) fn notify_tracks(&self) {
        self.wake.notify(WAKE_TRACKS);
    }

    pub(crate) fn events(&self) -> &EventHub {
        &self.events
    }

    /// The state machine's next armed deadline.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        lock_checked(&self.gw).next_deadline()
    }

    /// The last heartbeat, while a Driver exists (`None`: no handler-liveness watchdog).
    pub(crate) fn last_heartbeat(&self) -> Option<Instant> {
        if self.detached || self.is_closed() {
            return None;
        }
        Some(self.dispatch.last_heartbeat())
    }

    /// The sender `Dispatch` uses (test hooks inject receive inputs through it).
    #[cfg(feature = "test-hooks")]
    pub(crate) fn rx_sender_ref(&self) -> Option<&RxSender> {
        self.rx_tx.as_ref()
    }

    /// The dispatcher (test hooks feed driver events through its paths).
    #[cfg(feature = "test-hooks")]
    pub(crate) fn dispatch(&self) -> &Dispatch {
        &self.dispatch
    }

    /// Hands a memory/queue input to songbird (or, detached, to the parking lot) and binds the
    /// handle.
    fn start_track(
        &self,
        track: &TrackRef,
        input: songbird::input::Input,
        opts: PlayOpts,
    ) -> Result<(), CoreError> {
        #[cfg(feature = "test-hooks")]
        if self.detached {
            let _ctl = self.park(track, input, None, opts.paused);
            if let Some(f) = track.fake.get() {
                // What songbird's `TrackState` would start with (detached `info()`).
                f.set_volume(opts.volume);
                f.set_loops(opts.loops);
            }
            return Ok(());
        }
        let t = songbird::tracks::Track::new(input)
            .volume(opts.volume)
            .loops(loop_state(opts.loops)?);
        let t = if opts.paused { t.pause() } else { t };
        let Some(handle) = self.with_driver(|d| d.play(t)) else {
            return Err(self.closed_err());
        };
        self.tracks.bind(track, handle);
        Ok(())
    }

    /// The queue's current entry becomes a songbird track (`TrackQueue::advance`).
    pub(crate) fn start_queue_entry(
        &self,
        src: &SourceSpec,
        opts: PlayOpts,
        track: &TrackRef,
        done: crate::tracks::DoneGuard,
    ) -> Result<Option<Arc<EncodedShared>>, CoreError> {
        self.ensure_open()?;
        let SourceSpec::Encoded(fac) = src else {
            let (input, _) = src.to_input_with(
                done,
                &self.wake,
                self.send_muted.clone(),
                self.clock.clone(),
            )?;
            self.start_track(track, input, opts)?;
            return Ok(None);
        };
        // A lazy encoded stream is a live source (§4.9): it starts paused with volume 1.0 (the
        // decoder applies the entry's current volume) and the first decoded frame resumes it.
        // Live sources cannot loop. `paused=True` holds the decoded audio until resumed.
        let spec = EncodedSpec {
            volume: opts.volume,
            ..fac.spec
        };
        let token = self.wake.alloc()?;
        let (stream, input) = spawn_encoded_with(
            spec,
            token,
            done,
            self.send_muted.clone(),
            self.clock.clone(),
        )?;
        if opts.paused {
            stream.pause(true);
        }
        // Registered first, so T5 closes it (or `add_source` does, if the connection is closing).
        // Control goes to the internal queue, which outlives the `EncodedShared` (see
        // `open_encoded`).
        track.set_live(stream.live_queue());
        self.add_source(Arc::downgrade(&stream) as Weak<dyn LiveSource>, false);
        self.add_source(stream.live_queue(), false);
        match self.start_live(track, input, Some(stream.id()), 1.0) {
            Ok(ctl) => {
                stream.attach_track(ctl);
                Ok(Some(stream))
            }
            Err(e) => {
                stream.conn_closed();
                Err(e)
            }
        }
    }

    /// Hands a live input to songbird **paused** (§4.9) with the given track volume and returns
    /// its track control.
    fn start_live(
        &self,
        track: &TrackRef,
        input: songbird::input::Input,
        #[allow(unused_variables)] source_id: Option<u32>,
        volume: f32,
    ) -> Result<Box<dyn TrackControl>, CoreError> {
        #[cfg(feature = "test-hooks")]
        if self.detached {
            return Ok(self.park(track, input, source_id, true));
        }
        let t = songbird::tracks::Track::new(input).volume(volume).pause();
        let Some(handle) = self.with_driver(|d| d.play(t)) else {
            return Err(self.closed_err());
        };
        self.tracks.bind(track, handle.clone());
        Ok(Box::new(handle))
    }

    /// Runs `f` on the Driver under L1 (O(1) flume sends only); `None` without a Driver (torn down
    /// or detached). Nothing else is called under L1.
    fn with_driver<R>(&self, f: impl FnOnce(&mut OwnedDriver) -> R) -> Option<R> {
        let mut g = lock_checked(&self.driver);
        g.as_mut().map(f)
    }

    /// One of `MAX_LIVE_SOURCES_PER_CONN` open streams (dead and released sources are not
    /// counted), plus one of the `PREALLOCATED_TRACKS` songbird-resident tracks; hold the
    /// reservations until `add_source` has registered the source.
    fn reserve_live(&self) -> Result<(Reservation<'_>, Reservation<'_>), CoreError> {
        let live = self
            .live_adm
            .reserve("live_sources", MAX_LIVE_SOURCES_PER_CONN, || {
                self.open_live_sources()
            })?;
        Ok((live, self.reserve_resident()?))
    }

    /// Registered `open_*` streams that still count: not released (closed with abort). A closed
    /// stream's reader may stay in a parked mixer until the link is up; `resident_tracks` bounds
    /// those.
    fn open_live_sources(&self) -> usize {
        let held: Vec<Arc<dyn LiveSource>> = {
            let mut v = lock_checked(&self.sources);
            v.retain(|e| e.src.strong_count() > 0);
            v.iter()
                .filter(|e| e.counted)
                .filter_map(|e| e.src.upgrade())
                .collect()
        };
        // Checked (and the upgraded `Arc`s dropped) after L10 is released.
        held.iter().filter(|s| !s.is_released()).count()
    }

    /// One of `MAX_PLAYING_TRACKS_PER_CONN` (`play()` tracks plus the current queue entry, also a
    /// lazy encoded stream), plus one of the `PREALLOCATED_TRACKS` songbird-resident tracks; hold
    /// the reservations until the track is started (`TrackRegistry::playing` or
    /// `TrackQueue::live_current` counts it).
    pub(crate) fn reserve_playing(&self) -> Result<(Reservation<'_>, Reservation<'_>), CoreError> {
        let playing =
            self.playing_adm
                .reserve("playing_tracks", MAX_PLAYING_TRACKS_PER_CONN, || {
                    // Two leaf locks, one after the other (never nested).
                    self.tracks.playing() + self.queue.live_current()
                })?;
        Ok((playing, self.reserve_resident()?))
    }

    /// One of the `PREALLOCATED_TRACKS` (48) tracks songbird may hold (its mixer is sized for
    /// them, §4.8): started tracks whose input it has not dropped yet. Within the two caps above
    /// this only binds while the mixer is parked (joining, connecting, reconnecting): songbird runs
    /// track commands only on a live mixer, so every stream closed and track stopped meanwhile
    /// stays resident until the link is up. `LimitExceeded("resident_tracks")`.
    fn reserve_resident(&self) -> Result<Reservation<'_>, CoreError> {
        self.resident_adm
            .reserve("resident_tracks", usize::from(PREALLOCATED_TRACKS), || {
                self.tracks.resident()
            })
    }

    /// Registers a live source for T5 and the housekeeper (`counted`: towards
    /// `MAX_LIVE_SOURCES_PER_CONN`).
    fn add_source(&self, s: Weak<dyn LiveSource>, counted: bool) {
        {
            let mut v = lock_checked(&self.sources);
            v.retain(|e| e.src.strong_count() > 0);
            v.push(SourceEntry {
                src: s.clone(),
                counted,
            });
        }
        if self.is_closed() {
            // T5 may already have run.
            if let Some(src) = s.upgrade() {
                src.conn_closed();
            }
            return;
        }
        self.worker.push(WorkerCmd::AddSource(s));
    }

    fn ensure_open(&self) -> Result<(), CoreError> {
        if self.is_closed() {
            Err(self.closed_err())
        } else {
            Ok(())
        }
    }

    /// `ConnectionClosed(reason)` with the connection's close reason.
    fn closed_err(&self) -> CoreError {
        CoreError::ConnectionClosed(
            self.status()
                .close_reason
                .unwrap_or(CloseReason::ConnectionClosed),
        )
    }

    /// Whether this is a detached test connection (no songbird Driver).
    pub fn is_detached(&self) -> bool {
        self.detached
    }

    pub fn guild_id(&self) -> NonZeroU64 {
        self.guild_id
    }

    pub fn user_id(&self) -> NonZeroU64 {
        self.user_id
    }
}

/// Clears the executor flag when dropped, including on unwind (§4.6).
struct Executing<'a>(&'a AtomicBool);

impl Drop for Executing<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Drop for ConnShared {
    fn drop(&mut self) {
        if runtime::in_foreign_process(self.pid) {
            // §4.10: a forked child never touches the inherited Driver. Dropping it would send
            // `Poison` through channels and wake a runtime whose locks a vanished parent thread may
            // hold; `close()` skipped T2–T9 for the same reason. Leak it.
            self.closing.store(true, Ordering::Release);
            if let Some(d) = self.driver.get_mut().take() {
                leak_driver(d);
            }
            return;
        }
        // Normally already closed by `native.Connection`'s Drop; idempotent.
        self.close(CloseReason::Dropped);
    }
}

/// §4.10: the inherited Driver of a forked child is leaked, never dropped.
#[cfg(not(test))]
fn leak_driver(d: OwnedDriver) {
    std::mem::forget(d);
}

/// Unit tests run in the creating process: the "leaked" Driver is kept here so the test can
/// assert the leak and then tear it down.
#[cfg(test)]
pub(crate) static LEAKED_DRIVERS: Mutex<Vec<OwnedDriver>> = Mutex::new(Vec::new());

#[cfg(test)]
fn leak_driver(d: OwnedDriver) {
    LEAKED_DRIVERS.lock().push(d);
}

/// A short name of an input for logs (never its payload: tokens and session ids are secrets).
fn input_kind(i: &GwInput) -> &'static str {
    match i {
        GwInput::Join { .. } => "Join",
        GwInput::Move { .. } => "Move",
        GwInput::BeginRejoin => "BeginRejoin",
        GwInput::Leave => "Leave",
        GwInput::SetFlags { .. } => "SetFlags",
        GwInput::VoiceState { .. } => "VoiceState",
        GwInput::VoiceServer { .. } => "VoiceServer",
        GwInput::ConnectResult { .. } => "ConnectResult",
        GwInput::DriverConnect { .. } => "DriverConnect",
        GwInput::DriverReconnect { .. } => "DriverReconnect",
        GwInput::DriverDisconnect { .. } => "DriverDisconnect",
        GwInput::RxStalled { .. } => "RxStalled",
        GwInput::HandlersLost => "HandlersLost",
        GwInput::Deadline => "Deadline",
        GwInput::Close(_) => "Close",
    }
}

#[cfg(test)]
mod tests;
