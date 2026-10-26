//! [`ConnShared`]: the per-connection facade the binding calls (W2, §4, §12.3).
//!
//! Every method is synchronous and non-blocking (O(1) or O(input)); no lock is held across an
//! `.await`, a syscall or another blocking wait (I7). `native.Connection` holds the only long-lived
//! strong `Arc`; songbird handlers and tasks hold `Weak<ConnShared>` (I4).
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::{HashSet, VecDeque};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::task::AbortHandle;

use super::dispatch::Dispatch;
use super::driver::OwnedDriver;
use super::gateway::{ActionBatch, GatewayState, GwInput, StatusSnapshot};
use super::ops::{OpRegistry, OpResult};
use super::settings::ConnParams;
#[cfg(feature = "test-hooks")]
use super::worker::ConnWorker;
use super::worker::WorkerShared;
use crate::clock::Clock;
use crate::ctrl::{Control, CtrlQueue};
use crate::error::{CloseReason, CoreError};
use crate::events::{EventHub, EventQueue, KindMask};
use crate::runtime::LiveCounter;
use crate::rx::queue::{SubSpec, Subscription};
use crate::rx::raw::RxSender;
use crate::rx::router::{RouterHandle, RouterSpec};
use crate::sync::lock_checked;
use crate::tracks::flags::{PlayOpts, TrackRef};
use crate::tracks::queue::TrackQueue;
use crate::tracks::registry::TrackRegistry;
use crate::tx::LiveSource;
use crate::tx::encoded::{EncodedShared, EncodedSpec};
use crate::tx::opus_stream::{OpusQueue, OpusStreamSpec};
use crate::tx::queue::{PcmQueue, PlaybackSpec};
use crate::tx::source::SourceSpec;
use crate::wake::{Doorbell, WakeHub};

pub use super::registry::{registry_snapshot, shutdown_all};

/// Bot flags set from Python (`set_users_bot`) and users still awaiting classification.
#[derive(Debug, Default)]
pub(crate) struct BotTable {
    bots: HashSet<u64>,
    known: HashSet<u64>,
}

/// Connection-level counters (§11.3: `connects`, `rejoins`, `stale_connects_dropped`, …).
#[derive(Debug, Default)]
pub(crate) struct ConnStats {
    connects: AtomicU64,
    rejoins: AtomicU64,
    stale_connects_dropped: AtomicU64,
    gw_unlisted_inputs: AtomicU64,
    dispatch_reinstalls: AtomicU64,
    heartbeat_misses: AtomicU64,
    panics: AtomicU64,
}

/// Everything one voice connection owns (§4.3).
pub struct ConnShared {
    guild_id: NonZeroU64,
    user_id: NonZeroU64,
    reconnect: bool,
    receive: bool,
    /// L1.
    driver: Mutex<Option<OwnedDriver>>,
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
    rx_tx: Option<RxSender>,
    dispatch: Dispatch,
    /// L10: live sources, pruned on insert, closed at T5.
    sources: Mutex<Vec<Weak<dyn LiveSource>>>,
    /// L10: connect/op tasks only (the ConnWorker exits on the shutdown flag, T6/T7).
    tasks: Mutex<Vec<AbortHandle>>,
    ops: OpRegistry,
    /// L10.
    bots: Mutex<BotTable>,
    unclassified: Arc<Mutex<Vec<u64>>>,
    closing: Arc<AtomicBool>,
    send_muted: Arc<AtomicBool>,
    /// The creating process; teardown is skipped in any other process (§4.10).
    pid: u32,
    clock: Clock,
    stats: ConnStats,
    _live: LiveCounter,
}

impl ConnShared {
    /// Builds the Driver, the Dispatch and the ConnWorker (spawned on the RX runtime) and registers
    /// the connection. The doorbell is dropped (closed) on failure.
    pub fn new(p: ConnParams, bell: Doorbell) -> Result<Arc<Self>, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// A full connection with **no songbird Driver** and the worker returned to the caller instead
    /// of spawned (`native._testing.detached_connection`, §13.2).
    #[cfg(feature = "test-hooks")]
    pub fn new_detached(
        p: ConnParams,
        bell: Doorbell,
    ) -> Result<(Arc<Self>, ConnWorker), CoreError> {
        Err(CoreError::Internal("unimplemented"))
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
        self.apply(GwInput::BeginRejoin);
    }

    /// `Leave`: the following `VoiceState(None)` closes with `Requested`.
    pub fn request_leave(&self) {
        self.apply(GwInput::Leave);
    }

    /// Forwards discord.py's `on_voice_state_update` for our own user.
    pub fn on_voice_state(&self, session_id: &str, channel_id: Option<u64>) {
        self.apply(GwInput::VoiceState {
            session_id: session_id.into(),
            channel: channel_id,
        });
    }

    /// Forwards discord.py's `on_voice_server_update` (`endpoint=None`: server reallocating).
    pub fn on_voice_server(&self, token: &str, endpoint: Option<&str>) {
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
        0
    }

    // ── signalling to Python ─────────────────────────────────────────────────────────────────────

    /// Ready wake ids (Python only, after `sock_recv` returned).
    pub fn take_ready(&self, out: &mut Vec<u8>) {
        self.wake.take_ready(out);
    }

    /// Pending control slots (at most `max`).
    pub fn take_controls(&self, max: usize, out: &mut Vec<Control>) {
        self.ctrl.take(out);
    }

    /// Users first seen since the last call, awaiting bot classification (`ClassifyUsers`).
    pub fn take_unclassified_users(&self) -> Vec<u64> {
        Vec::new()
    }

    /// Records which users are bots (`ignore_bots` filters).
    pub fn set_users_bot(&self, users: &[(u64, bool)]) {
        // phase 0: not recorded yet.
    }

    // ── receive ──────────────────────────────────────────────────────────────────────────────────

    /// A per-user or mixed subscription (`ReceiveDisabled` with `receive=False`).
    pub fn subscribe(&self, spec: SubSpec) -> Result<Subscription, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// A `listen_users` router.
    pub fn subscribe_router(&self, spec: RouterSpec) -> Result<RouterHandle, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// An event stream.
    pub fn subscribe_events(
        &self,
        cap: usize,
        kinds: KindMask,
    ) -> Result<Arc<EventQueue>, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    // ── send ─────────────────────────────────────────────────────────────────────────────────────

    /// A realtime PCM playback (starts paused until the first accepted write, §4.9).
    pub fn open_playback(
        &self,
        spec: PlaybackSpec,
    ) -> Result<(Arc<PcmQueue>, TrackRef), CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// A push-based encoded stream with its decode thread.
    pub fn open_encoded(
        &self,
        spec: EncodedSpec,
    ) -> Result<(Arc<EncodedShared>, TrackRef), CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// A push-based Opus packet stream.
    pub fn open_opus_stream(
        &self,
        spec: OpusStreamSpec,
    ) -> Result<(Arc<OpusQueue>, TrackRef), CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Plays a memory source now (`MAX_PLAYING_TRACKS_PER_CONN`).
    pub fn play(&self, src: &SourceSpec, opts: PlayOpts) -> Result<TrackRef, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// The sequential track queue.
    pub fn queue(&self) -> &TrackQueue {
        &self.queue
    }

    /// The connection's track registry (`TrackQueue::enqueue` registers entries in it).
    pub fn tracks(&self) -> &TrackRegistry {
        &self.tracks
    }

    /// The encoded stream of a lazy `Encoded` queue entry that became current.
    pub fn queue_take_stream(&self, t: &TrackRef) -> Option<Arc<EncodedShared>> {
        None
    }

    /// Stops every track, queue entry and live stream; the connection stays up.
    pub fn stop_all(&self) {
        // phase 0: nothing is playing.
    }

    /// `None` = songbird default (128 kbps); otherwise 6 000..=510 000 bps.
    pub fn set_bitrate(&self, bps: Option<i32>) -> Result<(), CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    // ── track ops ────────────────────────────────────────────────────────────────────────────────

    /// Starts a `get_info` op; returns the op id.
    pub fn start_track_info(&self, t: &TrackRef, timeout: Duration) -> Result<u32, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Starts a seek op; returns the op id.
    pub fn start_track_seek(
        &self,
        t: &TrackRef,
        pos: Duration,
        timeout: Duration,
    ) -> Result<u32, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Finished op results.
    pub fn take_op_results(&self, out: &mut Vec<(u32, OpResult)>) {
        self.ops.take_results(out);
    }

    // ── lifecycle ────────────────────────────────────────────────────────────────────────────────

    /// Complete teardown T0–T9 (§4.7). Idempotent and non-blocking; safe from `Drop` on any thread.
    pub fn close(&self, reason: CloseReason) {
        // T0: before taking any lock.
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        // phase 0: T1–T8 are not implemented yet; T9 removes the registry entry.
        super::registry::unregister(self);
    }

    pub fn is_closed(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// `SongbirdClient.stats()` counters (§11.3).
    pub fn stats(&self) -> Vec<(&'static str, u64)> {
        Vec::new()
    }

    /// Steps the state machine under L2 and executes the actions in step order (§4.6).
    pub(crate) fn apply(&self, input: GwInput) {
        if self.closing.load(Ordering::Acquire) {
            return;
        }
        drop(input); // phase 0: inputs are ignored.
    }

    /// Housekeeping for tracks and the queue (finalisation, advancing), run by the ConnWorker.
    pub(crate) fn poll_tracks(&self, now: std::time::Instant) {
        // phase 0: nothing to do.
    }
}

/// Test hooks (§13.2, W5; see `docs/deviations/W5.md`): the tick-channel sender `Dispatch` feeds, so
/// `testing::inject_*` deliver through the same path. `None` when no channel exists.
#[cfg(feature = "test-hooks")]
impl ConnShared {
    pub(crate) fn test_rx_sender(&self) -> Option<&RxSender> {
        self.rx_tx.as_ref()
    }
}
