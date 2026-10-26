//! The synchronous receive core (W3, §8.4–§8.8): unit-testable without a runtime or songbird.
//!
//! Driven by the ConnWorker: `apply` (commands), `on_input` (ticks, mappings, link changes) and
//! `on_timer` (silence synthesis, pruning, stall watchdog).
//!
//! Per tick: extract → decode the SSRCs a consumer needs (48 kHz stereo, exact PLC) → 960-frame
//! per-user frames → speaking FSM → routers → mix buses → converters (one per source, rate and
//! channel count) → lanes (encode + chunk, shared `Arc<ChunkData>`) → subscription fan-out.
//!
//! Commands are queued by `apply` and take effect at the start of the next `on_input`/`on_timer`
//! (which carry the time needed to fast-forward an idle timeline), i.e. at the next tick boundary.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use smallvec::SmallVec;

use super::bus::{BusActivity, MixBus, MixFilter, UserFrames};
use super::chunk::ChunkData;
use super::convert::{Converter, SourceKey};
use super::decoder::{DECODE_SCRATCH_LEN, SsrcState};
use super::lane::{Lane, TickMeta};
use super::opus::{extract, is_silent_frame};
use super::queue::{RxCmd, RxSource, SubId, SubQueue, SubSpec, Subscription};
use super::raw::{RawTick, RawVoice, RxInput};
use super::router::{RouterQueue, RouterSpec};
use super::speaking::SpeakingDetector;
use crate::conn::worker::WorkerShared;
use crate::ctrl::{Control, CtrlQueue};
use crate::dsp::measure;
use crate::dsp::mix::add_into;
use crate::error::CloseReason;
use crate::events::{EventData, EventHub};
use crate::format::{Format, SampleRate};
use crate::limits::{FRAMES_PER_TICK_48K, MAX_SUBS_PER_CONN, TICK_MS};
use crate::sync::lock_checked;

/// Synthesis starts once the timeline is more than this many ticks behind real time (100 ms).
pub const SYNTH_TRIGGER_TICKS: u64 = 5;
/// Synthesis fills up to this many ticks behind real time (40 ms of slack for late real ticks).
pub const SYNTH_SLACK_TICKS: u64 = 2;
/// Silent real ticks are dropped while the timeline is more than this many ticks ahead.
pub const DEBT_TICKS: u64 = 5;
/// At most this many silent ticks are synthesised at once (60 s, the largest queue bound); a
/// longer outage fast-forwards the excess (lanes realign, stream clocks jump).
pub const MAX_SYNTH_BURST: u64 = 3_000;
/// SSRC state of a user who left is pruned this long after `ClientDisconnect` (§8.5).
pub const USER_LEFT_PRUNE: Duration = Duration::from_secs(1);
/// Bound of the SSRC → user map (least recently seen evicted beyond it).
pub const MAX_SSRC_MAPPINGS: usize = 1_024;
/// Ids of subscriptions created by routers have this bit set, so they never collide with the
/// connection's own monotonic `SubId`s.
pub const ROUTER_SUB_ID_BIT: u64 = 1 << 63;

const PRUNE_INTERVAL: Duration = Duration::from_secs(1);
const STALL_REPORT_INTERVAL: Duration = Duration::from_secs(1);
const MAX_PENDING_PRUNES: usize = 1_024;
const MAX_CLASSIFY: usize = 4_096;
const TICK_SAMPLES_48K: usize = FRAMES_PER_TICK_48K * 2;

static ZERO_FRAME: [f32; TICK_SAMPLES_48K] = [0.0; TICK_SAMPLES_48K];

/// Pipeline configuration (from `VoiceSettings`).
#[derive(Clone, Copy, Debug)]
pub struct RxConfig {
    pub enabled: bool,
    pub hangover_ms: u32,
    pub voice_threshold_dbfs: Option<f32>,
    pub max_decoded_users: usize,
    pub decode_state_timeout: Duration,
    pub stall_timeout: Duration,
    /// Timeline origin (ConnWorker start).
    pub t0: Instant,
}

/// What the worker should do next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxTimer {
    pub next_wake: Option<Instant>,
    /// Set while no real tick has arrived for `stall_timeout` (receive on, link up). Reported on
    /// every call while the stall lasts (the gateway rate-limits `RxStalled` itself).
    pub stalled_since: Option<Instant>,
}

/// Receive counters (§11.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxCounts {
    pub ticks_real: u64,
    pub ticks_synthetic: u64,
    pub ticks_skipped: u64,
    pub bad_packets: u64,
    pub decode_errors: u64,
    pub plc_frames: u64,
    pub carry_overflow_frames: u64,
    pub dave_trailers_stripped: u64,
    pub decoders_active: u64,
    pub subscriptions: u64,
    pub routers: u64,
    pub lanes: u64,
    pub converters: u64,
    pub buses: u64,
    pub router_streams: u64,
}

type LaneKey = (SourceKey, Format, u32);
type ConvKey = (SourceKey, SampleRate, u8);

/// One SSRC → user mapping.
#[derive(Clone, Copy, Debug)]
struct SsrcMap {
    user: u64,
    last_seen: Instant,
}

/// Decoder state of one SSRC plus bookkeeping.
struct StateSlot {
    state: SsrcState,
    last_seen: Instant,
    /// A packet or loss for this SSRC was decoded in the current tick.
    touched: bool,
}

/// One user's (or the unmapped SSRCs') data for the current tick.
struct UserTick {
    user: Option<u64>,
    frame: Box<[f32; TICK_SAMPLES_48K]>,
    has_audio: bool,
    packet_voiced: bool,
    lost: bool,
    concealed: bool,
    voiced: bool,
}

struct BusEntry {
    bus: MixBus,
    /// Lanes fed by this bus.
    refs: usize,
    out: Box<[f32; TICK_SAMPLES_48K]>,
    activity: BusActivity,
    concealed: bool,
}

struct ConvEntry {
    conv: Converter,
    /// Lanes fed by this converter.
    refs: usize,
    out: Vec<f32>,
}

struct LaneEntry {
    lane: Lane,
    /// Subscriptions on (or moving to) this lane.
    refs: usize,
    conv: ConvKey,
    emitted: Vec<Arc<ChunkData>>,
}

/// A registered subscription.
struct SubEntry {
    queue: Arc<SubQueue>,
    lane: LaneKey,
    /// Only chunks starting at or after this timeline position are delivered.
    start_ms: u64,
    /// `set_filter` in progress: `(new lane, switch_ms)`. The old lane delivers chunks before
    /// `switch_ms`, the new one from `switch_ms` (no gap, no duplicate).
    pending: Option<(LaneKey, u64)>,
    mixed: bool,
}

/// A registered router.
struct RouterEntry {
    spec: RouterSpec,
    queue: Arc<RouterQueue>,
    /// Users that already got a stream (their queue, while it lives).
    routed: HashMap<u64, Weak<SubQueue>>,
}

/// The receive pipeline of one connection.
pub struct RxPipeline {
    cfg: RxConfig,
    events: Arc<EventHub>,
    ctrl: Arc<CtrlQueue>,
    unclassified: Arc<Mutex<Vec<u64>>>,
    worker: Weak<WorkerShared>,
    pending: Vec<RxCmd>,
    // Mapping and decoding.
    ssrc_users: HashMap<u32, SsrcMap>,
    states: HashMap<u32, StateSlot>,
    speaking: SpeakingDetector,
    bots: HashSet<u64>,
    classify_seen: HashSet<u64>,
    pending_prunes: VecDeque<(Instant, u64)>,
    next_prune: Option<Instant>,
    // Consumers.
    buses: HashMap<u32, BusEntry>,
    bus_ids: HashMap<MixFilter, u32>,
    next_bus_id: u32,
    converters: HashMap<ConvKey, ConvEntry>,
    lanes: HashMap<LaneKey, LaneEntry>,
    user_lanes: HashMap<u64, usize>,
    subs: HashMap<SubId, SubEntry>,
    routers: HashMap<u64, RouterEntry>,
    next_router_sub: u64,
    // Timeline and watchdog.
    tick_index: u64,
    link_up: bool,
    stall_ref: Option<Instant>,
    stall_reported: bool,
    // Per-tick scratch (reused).
    scratch: Box<[f32]>,
    tmp: Box<[f32; TICK_SAMPLES_48K]>,
    cur: Vec<UserTick>,
    frame_pool: Vec<Box<[f32; TICK_SAMPLES_48K]>>,
    ev: Vec<EventData>,
    counts: RxCounts,
    #[cfg(test)]
    emitted: Vec<EventData>,
}

/// The entry of `user` in this tick's user list (created from the frame pool on first use).
fn user_entry<'a>(
    cur: &'a mut Vec<UserTick>,
    pool: &mut Vec<Box<[f32; TICK_SAMPLES_48K]>>,
    user: Option<u64>,
) -> Option<&'a mut UserTick> {
    if !cur.iter().any(|e| e.user == user) {
        let mut frame = pool
            .pop()
            .unwrap_or_else(|| Box::new([0.0; TICK_SAMPLES_48K]));
        frame.fill(0.0);
        cur.push(UserTick {
            user,
            frame,
            has_audio: false,
            packet_voiced: false,
            lost: false,
            concealed: false,
            voiced: false,
        });
    }
    cur.iter_mut().rev().find(|e| e.user == user)
}

/// Whether any packet of `raw` carries voiced (non-silent) Opus.
fn tick_has_voice(raw: &RawTick) -> bool {
    raw.voices.iter().any(|v| match v {
        RawVoice::Packet {
            packet,
            payload_offset,
            payload_end,
            ..
        } => extract(packet, *payload_offset, *payload_end)
            .and_then(|x| packet.get(x.range))
            .is_some_and(|p| !is_silent_frame(p)),
        RawVoice::Lost { .. } => false,
    })
}

fn ticks_ms(ticks: u64) -> u64 {
    ticks.saturating_mul(u64::from(TICK_MS))
}

impl RxPipeline {
    pub fn new(
        cfg: RxConfig,
        events: Arc<EventHub>,
        ctrl: Arc<CtrlQueue>,
        unclassified: Arc<Mutex<Vec<u64>>>,
    ) -> Self {
        RxPipeline {
            speaking: SpeakingDetector::new(cfg.hangover_ms.div_ceil(TICK_MS)),
            cfg,
            events,
            ctrl,
            unclassified,
            worker: Weak::new(),
            pending: Vec::new(),
            ssrc_users: HashMap::new(),
            states: HashMap::new(),
            bots: HashSet::new(),
            classify_seen: HashSet::new(),
            pending_prunes: VecDeque::new(),
            next_prune: None,
            buses: HashMap::new(),
            bus_ids: HashMap::new(),
            next_bus_id: 0,
            converters: HashMap::new(),
            lanes: HashMap::new(),
            user_lanes: HashMap::new(),
            subs: HashMap::new(),
            routers: HashMap::new(),
            next_router_sub: 0,
            tick_index: 0,
            link_up: false,
            stall_ref: None,
            stall_reported: false,
            scratch: vec![0.0f32; DECODE_SCRATCH_LEN].into_boxed_slice(),
            tmp: Box::new([0.0; TICK_SAMPLES_48K]),
            cur: Vec::new(),
            frame_pool: Vec::new(),
            ev: Vec::new(),
            counts: RxCounts::default(),
            #[cfg(test)]
            emitted: Vec::new(),
        }
    }

    /// Lets routers create budget-checked streams: their reservations, T6 registration and
    /// `RemoveSub` commands go through `worker`. The ConnWorker attaches its `WorkerShared` when it
    /// takes ownership of the pipeline. Without it (a pipeline driven on its own), router streams
    /// are created unbudgeted and detached.
    pub fn attach_worker(&mut self, worker: Weak<WorkerShared>) {
        self.worker = worker;
    }

    /// Queues one command (subscribe, unsubscribe, filter change, router, bot flags); it takes
    /// effect at the start of the next `on_input`/`on_timer`.
    pub fn apply(&mut self, cmd: RxCmd) {
        self.pending.push(cmd);
    }

    /// Processes one input from `Dispatch` (or the test hooks).
    pub fn on_input(&mut self, input: RxInput, now: Instant) {
        self.settle(now);
        match input {
            RxInput::Tick(raw) => self.on_tick(raw, now),
            RxInput::Map { ssrc, user_id } => self.on_map(ssrc, user_id, now),
            RxInput::UserLeft { user_id } => self.on_user_left(user_id, now),
            RxInput::LinkUp { .. } => {
                self.link_up = true;
                self.stall_ref = Some(now);
                self.stall_reported = false;
            }
            RxInput::LinkDown => {
                self.link_up = false;
                self.stall_reported = false;
            }
        }
        self.flush_events();
    }

    /// Silence synthesis, state pruning and the stall watchdog.
    pub fn on_timer(&mut self, now: Instant) -> RxTimer {
        self.settle(now);
        self.prune_closed();
        if self.cfg.enabled && self.has_consumers() {
            self.synthesize(now);
        }
        self.prune(now);
        let (stalled_since, stall_wake) = self.check_stall(now);
        self.flush_events();
        let mut next = stall_wake;
        let mut consider = |t: Option<Instant>| {
            if let Some(t) = t {
                next = Some(next.map_or(t, |n: Instant| n.min(t)));
            }
        };
        if self.cfg.enabled && self.has_consumers() {
            // Synthesis triggers once `tick_index + 5 < expected(now)`.
            let due = self.tick_index.saturating_add(SYNTH_TRIGGER_TICKS + 1);
            consider(
                self.cfg
                    .t0
                    .checked_add(Duration::from_millis(ticks_ms(due))),
            );
        }
        consider(self.pending_prunes.front().map(|(due, _)| *due));
        if !self.states.is_empty() {
            consider(self.next_prune);
        }
        RxTimer {
            next_wake: next,
            stalled_since,
        }
    }

    /// `20 · tick_index`.
    pub fn timeline_ms(&self) -> u64 {
        ticks_ms(self.tick_index)
    }

    /// Closes every subscription and router queue with `r` and drops all consumer state (lanes,
    /// converters, buses, decoders). Mappings survive.
    pub fn close_all(&mut self, r: CloseReason) {
        for cmd in self.pending.drain(..) {
            match cmd {
                RxCmd::AddSub { queue, .. } => queue.close(r),
                RxCmd::AddRouter { queue, .. } => queue.close(false),
                _ => {}
            }
        }
        for s in self.subs.values() {
            s.queue.close(r);
        }
        for rt in self.routers.values() {
            rt.queue.close(false);
        }
        self.subs.clear();
        self.routers.clear();
        self.lanes.clear();
        self.converters.clear();
        self.buses.clear();
        self.bus_ids.clear();
        self.user_lanes.clear();
        self.states.clear();
        for u in self.cur.drain(..) {
            self.frame_pool.push(u.frame);
        }
    }

    pub fn counts(&self) -> RxCounts {
        let n = |x: usize| x as u64;
        RxCounts {
            decoders_active: n(self.states.len()),
            subscriptions: n(self.subs.len()),
            routers: n(self.routers.len()),
            lanes: n(self.lanes.len()),
            converters: n(self.converters.len()),
            buses: n(self.buses.len()),
            ..self.counts
        }
    }

    /// Events emitted so far (tests only; the hub is the real sink).
    #[cfg(test)]
    pub(crate) fn take_emitted(&mut self) -> Vec<EventData> {
        std::mem::take(&mut self.emitted)
    }

    // ── time ─────────────────────────────────────────────────────────────────────────────────────

    /// `⌊(now − t0) / 20 ms⌋`.
    fn expected(&self, now: Instant) -> u64 {
        let ms = now.saturating_duration_since(self.cfg.t0).as_millis();
        u64::try_from(ms / u128::from(TICK_MS)).unwrap_or(u64::MAX)
    }

    fn has_consumers(&self) -> bool {
        !self.subs.is_empty() || !self.routers.is_empty()
    }

    /// Idle timelines are lazy: jump to `expected(now) − 2` instead of synthesising.
    fn fast_forward(&mut self, now: Instant) {
        let exp = self.expected(now);
        if self.tick_index.saturating_add(SYNTH_TRIGGER_TICKS) < exp {
            self.tick_index = exp - SYNTH_SLACK_TICKS;
        }
    }

    /// Synthesises silent ticks up to `expected(now) − 2` once more than 100 ms behind.
    fn synthesize(&mut self, now: Instant) {
        let exp = self.expected(now);
        if self.tick_index.saturating_add(SYNTH_TRIGGER_TICKS) >= exp {
            return;
        }
        let target = exp - SYNTH_SLACK_TICKS;
        let mut n = target.saturating_sub(self.tick_index);
        if n > MAX_SYNTH_BURST {
            self.tick_index = target - MAX_SYNTH_BURST;
            n = MAX_SYNTH_BURST;
        }
        for _ in 0..n {
            let at = self
                .cfg
                .t0
                .checked_add(Duration::from_millis(ticks_ms(self.tick_index)))
                .unwrap_or(now);
            self.run_tick(&[], true, at);
            self.counts.ticks_synthetic += 1;
        }
    }

    // ── commands ─────────────────────────────────────────────────────────────────────────────────

    /// Applies queued commands in order (fast-forwarding an idle timeline first).
    fn settle(&mut self, now: Instant) {
        if self.pending.is_empty() {
            return;
        }
        let mut cmds = std::mem::take(&mut self.pending);
        for cmd in cmds.drain(..) {
            match cmd {
                RxCmd::AddSub { id, spec, queue } => {
                    if !self.has_consumers() {
                        self.fast_forward(now);
                    }
                    self.add_sub(id, &spec, queue, None);
                }
                RxCmd::RemoveSub(id) => self.remove_sub(id),
                RxCmd::SetFilter(id, f) => self.set_filter(id, f),
                RxCmd::AddRouter { id, spec, queue } => {
                    if !self.has_consumers() {
                        self.fast_forward(now);
                    }
                    if !queue.is_closed() {
                        self.routers.insert(
                            id,
                            RouterEntry {
                                spec,
                                queue,
                                routed: HashMap::new(),
                            },
                        );
                    }
                }
                RxCmd::RemoveRouter(id) => {
                    self.routers.remove(&id);
                }
                RxCmd::SetBots(list) => {
                    for (u, bot) in list {
                        if bot {
                            self.bots.insert(u);
                        } else {
                            self.bots.remove(&u);
                        }
                    }
                }
            }
        }
        // Keep the allocation unless commands arrived meanwhile (they cannot: `&mut self`).
        if self.pending.is_empty() {
            self.pending = cmds;
        }
    }

    /// Registers a subscription on its lane. `covering`: start with the chunk containing that tick
    /// (router streams); otherwise at the lane's next chunk.
    fn add_sub(
        &mut self,
        id: SubId,
        spec: &SubSpec,
        queue: Arc<SubQueue>,
        covering: Option<u64>,
    ) -> bool {
        if queue.is_closed() || self.subs.contains_key(&id) {
            return false;
        }
        let (source, mixed) = match &spec.source {
            RxSource::User(u) => (SourceKey::User(*u), false),
            RxSource::Mixed(f) => (SourceKey::Bus(self.bus_for(f)), true),
        };
        match self.acquire_lane(source, spec.format, spec.chunk_ms, covering) {
            Some(key) => {
                let start_ms = self
                    .lanes
                    .get(&key)
                    .map_or(0, |l| l.lane.next_chunk_start_ms());
                self.subs.insert(
                    id,
                    SubEntry {
                        queue,
                        lane: key,
                        start_ms,
                        pending: None,
                        mixed,
                    },
                );
                true
            }
            None => {
                queue.close(CloseReason::InternalError);
                self.gc_buses();
                false
            }
        }
    }

    fn remove_sub(&mut self, id: SubId) {
        if let Some(e) = self.subs.remove(&id) {
            self.release_lane(&e.lane);
            if let Some((p, _)) = e.pending {
                self.release_lane(&p);
            }
        }
    }

    /// Moves a mixed subscription to the lane of `f` at that lane's first complete chunk.
    fn set_filter(&mut self, id: SubId, f: MixFilter) {
        let Some((fmt, chunk_ms)) = self
            .subs
            .get(&id)
            .filter(|e| e.mixed && !e.queue.is_closed())
            .map(|e| (e.lane.1, e.lane.2))
        else {
            return;
        };
        let bus = self.bus_for(&f);
        let Some(new_key) = self.acquire_lane(SourceKey::Bus(bus), fmt, chunk_ms, None) else {
            self.gc_buses();
            return;
        };
        let now_ms = ticks_ms(self.tick_index);
        let cm = u64::from(chunk_ms);
        let switch_ms = now_ms.div_ceil(cm).saturating_mul(cm);
        let mut release: SmallVec<[LaneKey; 2]> = SmallVec::new();
        if let Some(e) = self.subs.get_mut(&id) {
            if new_key == e.lane {
                // Back to the current lane: cancel a move in progress.
                release.push(new_key);
                if let Some((p, _)) = e.pending.take() {
                    release.push(p);
                }
            } else if e.pending.as_ref().is_some_and(|(p, _)| *p == new_key) {
                release.push(new_key); // already moving there
            } else {
                if let Some((p, _)) = e.pending.take() {
                    release.push(p);
                }
                if switch_ms <= now_ms {
                    // On a boundary: the old lane has emitted everything before it.
                    release.push(std::mem::replace(&mut e.lane, new_key));
                    e.start_ms = e.start_ms.max(switch_ms);
                } else {
                    e.pending = Some((new_key, switch_ms));
                }
            }
        } else {
            release.push(new_key);
        }
        for k in release {
            self.release_lane(&k);
        }
    }

    /// The bus id of `f` (created on first use with no lanes yet).
    fn bus_for(&mut self, f: &MixFilter) -> u32 {
        if let Some(id) = self.bus_ids.get(f) {
            return *id;
        }
        let id = self.next_bus_id;
        self.next_bus_id = self.next_bus_id.wrapping_add(1);
        self.bus_ids.insert(f.clone(), id);
        self.buses.insert(
            id,
            BusEntry {
                bus: MixBus::new(f.clone()),
                refs: 0,
                out: Box::new([0.0; TICK_SAMPLES_48K]),
                activity: BusActivity::default(),
                concealed: false,
            },
        );
        id
    }

    fn gc_buses(&mut self) {
        let dead: SmallVec<[u32; 4]> = self
            .buses
            .iter()
            .filter(|(_, b)| b.refs == 0)
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            if let Some(b) = self.buses.remove(&id) {
                self.bus_ids.remove(b.bus.filter());
            }
        }
    }

    /// One more reference on the lane `(source, fmt, chunk_ms)`, creating it (and its converter)
    /// if needed. `None` if a converter or lane cannot be built.
    fn acquire_lane(
        &mut self,
        source: SourceKey,
        fmt: Format,
        chunk_ms: u32,
        covering: Option<u64>,
    ) -> Option<LaneKey> {
        let key: LaneKey = (source.clone(), fmt, chunk_ms);
        if let Some(l) = self.lanes.get_mut(&key) {
            l.refs += 1;
            return Some(key);
        }
        let ck: ConvKey = (source.clone(), fmt.rate, fmt.channels);
        if !self.converters.contains_key(&ck) {
            let conv = Converter::new(fmt.rate, fmt.channels).ok()?;
            let out = vec![0.0f32; conv.output_len()];
            self.converters
                .insert(ck.clone(), ConvEntry { conv, refs: 0, out });
        }
        let lane = match covering {
            Some(t) => Lane::covering(source.clone(), fmt, chunk_ms, t),
            None => Lane::new(source.clone(), fmt, chunk_ms, self.tick_index),
        };
        let Ok(lane) = lane else {
            if self.converters.get(&ck).is_some_and(|c| c.refs == 0) {
                self.converters.remove(&ck);
            }
            return None;
        };
        if let Some(c) = self.converters.get_mut(&ck) {
            c.refs += 1;
        }
        match &source {
            SourceKey::User(u) => *self.user_lanes.entry(*u).or_insert(0) += 1,
            SourceKey::Bus(b) => {
                if let Some(e) = self.buses.get_mut(b) {
                    e.refs += 1;
                }
            }
        }
        self.lanes.insert(
            key.clone(),
            LaneEntry {
                lane,
                refs: 1,
                conv: ck,
                emitted: Vec::new(),
            },
        );
        Some(key)
    }

    /// Drops one reference; the last one frees the lane, then its converter and bus.
    fn release_lane(&mut self, key: &LaneKey) {
        let Some(l) = self.lanes.get_mut(key) else {
            return;
        };
        l.refs = l.refs.saturating_sub(1);
        if l.refs > 0 {
            return;
        }
        let Some(l) = self.lanes.remove(key) else {
            return;
        };
        if let Some(c) = self.converters.get_mut(&l.conv) {
            c.refs = c.refs.saturating_sub(1);
            if c.refs == 0 {
                self.converters.remove(&l.conv);
            }
        }
        match &key.0 {
            SourceKey::User(u) => {
                if let Some(n) = self.user_lanes.get_mut(u) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        self.user_lanes.remove(u);
                    }
                }
            }
            SourceKey::Bus(b) => {
                let dead = self.buses.get_mut(b).is_some_and(|e| {
                    e.refs = e.refs.saturating_sub(1);
                    e.refs == 0
                });
                if dead && let Some(e) = self.buses.remove(b) {
                    self.bus_ids.remove(e.bus.filter());
                }
            }
        }
    }

    /// Unregisters subscriptions whose queue closed (any reason) and closed routers.
    fn prune_closed(&mut self) {
        let closed: SmallVec<[SubId; 8]> = self
            .subs
            .iter()
            .filter(|(_, s)| s.queue.is_closed())
            .map(|(id, _)| *id)
            .collect();
        for id in closed {
            self.remove_sub(id);
        }
        self.routers.retain(|_, r| !r.queue.is_closed());
    }

    // ── inputs ───────────────────────────────────────────────────────────────────────────────────

    fn on_tick(&mut self, raw: RawTick, now: Instant) {
        self.stall_ref = Some(now);
        self.stall_reported = false;
        if !self.cfg.enabled {
            return;
        }
        if self.has_consumers() {
            self.synthesize(now);
        } else {
            self.fast_forward(now);
        }
        let exp = self.expected(now);
        if self.tick_index > exp.saturating_add(DEBT_TICKS) && !tick_has_voice(&raw) {
            // Debt after a burst: fully silent real ticks are dropped, voiced ones never.
            self.counts.ticks_skipped += 1;
            return;
        }
        self.counts.ticks_real += 1;
        self.run_tick(&raw.voices, false, raw.arrived);
    }

    fn on_map(&mut self, ssrc: u32, user: u64, now: Instant) {
        let changed = self.ssrc_users.get(&ssrc).is_none_or(|m| m.user != user);
        if !self.ssrc_users.contains_key(&ssrc)
            && self.ssrc_users.len() >= MAX_SSRC_MAPPINGS
            && let Some(old) = self
                .ssrc_users
                .iter()
                .min_by_key(|(_, m)| m.last_seen)
                .map(|(s, _)| *s)
        {
            self.ssrc_users.remove(&old);
        }
        self.ssrc_users.insert(
            ssrc,
            SsrcMap {
                user,
                last_seen: now,
            },
        );
        // A (re)mapped user is back: cancel a pending `UserLeft` prune.
        self.pending_prunes.retain(|(_, u)| *u != user);
        if !changed {
            return;
        }
        if let Some(slot) = self.states.get_mut(&ssrc) {
            if slot.state.user().is_some_and(|u| u != user) {
                slot.state.reset(); // the SSRC now belongs to someone else
            }
            slot.state.set_user(Some(user));
        }
        self.ev.push(EventData::UserMapped {
            user_id: user,
            ssrc,
        });
        if self.classify_seen.len() < MAX_CLASSIFY && self.classify_seen.insert(user) {
            {
                let mut q = lock_checked(&self.unclassified);
                if q.len() < MAX_CLASSIFY {
                    q.push(user);
                }
            }
            self.ctrl.set(Control::ClassifyUsers);
        }
    }

    fn on_user_left(&mut self, user: u64, now: Instant) {
        self.speaking.user_left(user, self.tick_index, &mut self.ev);
        for r in self.routers.values_mut() {
            if r.spec.close_on_leave
                && let Some(w) = r.routed.remove(&user)
                && let Some(q) = w.upgrade()
            {
                q.close(CloseReason::UserLeft);
            }
        }
        self.classify_seen.remove(&user);
        if self.pending_prunes.len() >= MAX_PENDING_PRUNES
            && let Some((_, u)) = self.pending_prunes.pop_front()
        {
            self.prune_user(u);
        }
        self.pending_prunes
            .push_back((now.checked_add(USER_LEFT_PRUNE).unwrap_or(now), user));
    }

    // ── pruning and watchdog ─────────────────────────────────────────────────────────────────────

    fn prune_user(&mut self, user: u64) {
        let ssrcs: SmallVec<[u32; 4]> = self
            .ssrc_users
            .iter()
            .filter(|(_, m)| m.user == user)
            .map(|(s, _)| *s)
            .collect();
        for s in ssrcs {
            self.ssrc_users.remove(&s);
            self.states.remove(&s);
        }
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&(due, user)) = self.pending_prunes.front() {
            if due > now {
                break;
            }
            self.pending_prunes.pop_front();
            self.prune_user(user);
        }
        if self.next_prune.is_none_or(|t| now >= t) {
            self.next_prune = now.checked_add(PRUNE_INTERVAL);
            let timeout = self.cfg.decode_state_timeout;
            self.states
                .retain(|_, s| now.saturating_duration_since(s.last_seen) < timeout);
        }
    }

    /// `(stalled_since, next check)`.
    fn check_stall(&mut self, now: Instant) -> (Option<Instant>, Option<Instant>) {
        if !(self.cfg.enabled && self.link_up) {
            return (None, None);
        }
        let since = *self.stall_ref.get_or_insert(now);
        let Some(deadline) = since.checked_add(self.cfg.stall_timeout) else {
            return (None, None);
        };
        if now < deadline {
            return (None, Some(deadline));
        }
        if !self.stall_reported {
            self.stall_reported = true;
            let since_ms = ticks_ms(self.expected(since));
            self.ev.push(EventData::ReceiveStalled { since_ms });
        }
        (Some(since), now.checked_add(STALL_REPORT_INTERVAL))
    }

    fn flush_events(&mut self) {
        for ev in self.ev.drain(..) {
            self.events.emit(&ev);
            #[cfg(test)]
            self.emitted.push(ev);
        }
    }

    // ── the tick ─────────────────────────────────────────────────────────────────────────────────

    /// Whether any consumer may need decoded audio this tick.
    fn decode_any(&self) -> bool {
        !self.lanes.is_empty()
            || !self.routers.is_empty()
            || self.cfg.voice_threshold_dbfs.is_some()
    }

    /// Whether audio of `user` (`None`: unmapped SSRCs) must be decoded.
    fn needs(&self, user: Option<u64>) -> bool {
        match user {
            Some(u) => {
                self.cfg.voice_threshold_dbfs.is_some()
                    || self.user_lanes.contains_key(&u)
                    || self
                        .buses
                        .values()
                        .any(|b| b.bus.filter().selects(u, &self.bots))
                    || self.routers.values().any(|r| r.spec.selects(u, &self.bots))
            }
            None => self.buses.values().any(|b| b.bus.filter().takes_unmapped()),
        }
    }

    /// A decoder for `ssrc`, evicting the least recently seen one beyond `max_decoded_users`.
    fn create_state(&mut self, ssrc: u32, user: Option<u64>, now: Instant) -> bool {
        if self.states.len() >= self.cfg.max_decoded_users.max(1)
            && let Some(victim) = self
                .states
                .iter()
                .min_by_key(|(_, s)| s.last_seen)
                .map(|(k, _)| *k)
        {
            self.states.remove(&victim);
        }
        match SsrcState::new() {
            Ok(mut state) => {
                state.set_user(user);
                self.states.insert(
                    ssrc,
                    StateSlot {
                        state,
                        last_seen: now,
                        touched: false,
                    },
                );
                true
            }
            Err(_) => {
                self.counts.decode_errors += 1;
                false
            }
        }
    }

    fn run_tick(&mut self, voices: &[RawVoice], synthetic: bool, captured_at: Instant) {
        let tick = self.tick_index;
        for u in self.cur.drain(..) {
            self.frame_pool.push(u.frame);
        }
        for slot in self.states.values_mut() {
            slot.touched = false;
        }
        let decode_any = self.decode_any();
        for v in voices {
            self.on_voice(v, tick, captured_at, decode_any);
        }
        self.collect_frames();
        let thr = self.cfg.voice_threshold_dbfs;
        for e in &mut self.cur {
            e.voiced = e.packet_voiced
                && thr.is_none_or(|t| e.has_audio && measure(&e.frame[..]).rms_dbfs >= t);
        }
        for e in &self.cur {
            if let Some(u) = e.user {
                let concealed = e.lost && !e.packet_voiced;
                self.speaking
                    .on_tick(u, e.voiced, concealed, tick, captured_at, &mut self.ev);
            }
        }
        self.speaking.end_tick(tick, &mut self.ev);
        if !self.routers.is_empty() {
            self.route(tick);
        }
        if !self.lanes.is_empty() {
            self.feed_lanes(tick, synthetic, captured_at);
            self.fan_out(tick);
        }
        self.tick_index = tick.saturating_add(1);
        self.prune_closed();
    }

    /// Records one SSRC's packet/loss and decodes it if a consumer needs its user.
    fn on_voice(&mut self, v: &RawVoice, tick: u64, now: Instant, decode_any: bool) {
        let (ssrc, pkt): (u32, Option<(&[u8], u16)>) = match v {
            RawVoice::Packet {
                ssrc,
                packet,
                payload_offset,
                payload_end,
            } => match extract(packet, *payload_offset, *payload_end) {
                Some(x) => {
                    if x.dave_stripped {
                        self.counts.dave_trailers_stripped += 1;
                    }
                    (*ssrc, packet.get(x.range).map(|p| (p, x.seq)))
                }
                None => {
                    self.counts.bad_packets += 1;
                    (*ssrc, None)
                }
            },
            RawVoice::Lost { ssrc } => (*ssrc, None),
        };
        let user = self.ssrc_users.get_mut(&ssrc).map(|m| {
            m.last_seen = now;
            m.user
        });
        let voiced = pkt.is_some_and(|(p, _)| !is_silent_frame(p));
        if let Some(e) = user_entry(&mut self.cur, &mut self.frame_pool, user) {
            e.packet_voiced |= voiced;
            if pkt.is_none() {
                e.lost = true;
            }
        }
        if !decode_any || !self.needs(user) {
            return;
        }
        if !self.states.contains_key(&ssrc) && !self.create_state(ssrc, user, now) {
            return;
        }
        let Some(slot) = self.states.get_mut(&ssrc) else {
            return;
        };
        slot.touched = true;
        slot.last_seen = now;
        slot.state.touch(tick);
        match pkt {
            Some((p, seq)) => {
                if slot.state.push_packet(p, seq, &mut self.scratch).is_err() {
                    self.counts.decode_errors += 1;
                    slot.state.conceal(&mut self.scratch);
                    self.counts.plc_frames += FRAMES_PER_TICK_48K as u64;
                }
            }
            None => {
                if slot.state.carry_frames() < FRAMES_PER_TICK_48K {
                    slot.state.conceal(&mut self.scratch);
                    self.counts.plc_frames += FRAMES_PER_TICK_48K as u64;
                }
            }
        }
    }

    /// Pops one 960-frame tick from every active decoder and sums it into its user's frame.
    fn collect_frames(&mut self) {
        for (ssrc, slot) in self.states.iter_mut() {
            if !slot.touched && slot.state.carry_frames() == 0 {
                continue;
            }
            let fill = slot.state.take_tick(&mut self.tmp);
            self.counts.carry_overflow_frames += slot.state.take_overflow_frames();
            if fill.real_frames == 0 && fill.concealed_frames == 0 {
                continue;
            }
            let user = self.ssrc_users.get(ssrc).map(|m| m.user);
            if let Some(e) = user_entry(&mut self.cur, &mut self.frame_pool, user) {
                add_into(&mut e.frame[..], &self.tmp[..]);
                e.has_audio = true;
                if fill.concealed_frames > 0 {
                    e.concealed = true;
                }
            }
        }
    }

    /// Creates a stream for every user who is voiced this tick and has none yet.
    fn route(&mut self, tick: u64) {
        let mut todo: SmallVec<[(u64, u64); 8]> = SmallVec::new();
        for (rid, r) in &self.routers {
            if r.queue.is_closed() {
                continue;
            }
            for e in &self.cur {
                if let Some(u) = e.user
                    && e.voiced
                    && !r.routed.contains_key(&u)
                    && r.spec.selects(u, &self.bots)
                {
                    todo.push((*rid, u));
                }
            }
        }
        for (rid, user) in todo {
            self.create_routed(rid, user, tick);
        }
    }

    fn create_routed(&mut self, rid: u64, user: u64, tick: u64) {
        let Some(r) = self.routers.get_mut(&rid) else {
            return;
        };
        // Mark first, so a failure is counted once rather than retried every voiced tick.
        r.routed.insert(user, Weak::new());
        let (spec, rq) = (r.spec.sub_spec(user), r.queue.clone());
        if self.subs.len() >= MAX_SUBS_PER_CONN {
            rq.count_dropped();
            return;
        }
        let Ok(token) = rq.alloc_token() else {
            rq.count_dropped();
            return;
        };
        let queue = SubQueue::new(&spec, token);
        let id = ROUTER_SUB_ID_BIT | self.next_router_sub;
        self.next_router_sub = self.next_router_sub.wrapping_add(1) & !ROUTER_SUB_ID_BIT;
        let sub = match self.worker.upgrade() {
            Some(w) => {
                let reserved = spec
                    .budget_bytes()
                    .and_then(|bytes| w.reserve_budget(bytes));
                match reserved {
                    Ok(guard) => {
                        w.register_queue(&queue);
                        Subscription::new(id, queue.clone(), Arc::downgrade(&w), guard)
                    }
                    Err(_) => {
                        rq.count_dropped();
                        return;
                    }
                }
            }
            None => Subscription::detached(id, queue.clone()),
        };
        if !self.add_sub(id, &spec, queue.clone(), Some(tick)) {
            rq.count_dropped();
            return; // `sub` drops here and closes
        }
        if let Some(r) = self.routers.get_mut(&rid) {
            r.routed.insert(user, Arc::downgrade(&queue));
        }
        self.counts.router_streams += 1;
        rq.push(sub);
    }

    /// Buses, converters and lanes for one tick.
    fn feed_lanes(&mut self, tick: u64, synthetic: bool, captured_at: Instant) {
        if !self.buses.is_empty() {
            let users: Vec<(u64, &[f32; TICK_SAMPLES_48K], bool)> = self
                .cur
                .iter()
                .filter_map(|e| e.user.map(|u| (u, &*e.frame, e.voiced)))
                .collect();
            let unmapped = self
                .cur
                .iter()
                .find(|e| e.user.is_none() && e.has_audio)
                .map(|e| &*e.frame);
            let frames = UserFrames {
                users: &users,
                unmapped,
            };
            for b in self.buses.values_mut() {
                b.activity = b.bus.mix(&frames, &self.bots, &mut b.out);
                b.concealed = self.cur.iter().any(|e| {
                    e.concealed
                        && e.user
                            .is_some_and(|u| b.bus.filter().selects(u, &self.bots))
                });
            }
        }
        for (ck, c) in self.converters.iter_mut() {
            let src: &[f32; TICK_SAMPLES_48K] = match &ck.0 {
                SourceKey::User(u) => self
                    .cur
                    .iter()
                    .find(|e| e.user == Some(*u))
                    .map_or(&ZERO_FRAME, |e| &*e.frame),
                SourceKey::Bus(b) => self.buses.get(b).map_or(&ZERO_FRAME, |b| &*b.out),
            };
            c.conv.process(src, &mut c.out);
        }
        for (key, l) in self.lanes.iter_mut() {
            l.emitted.clear();
            let Some(c) = self.converters.get(&l.conv) else {
                continue;
            };
            let meta = match &key.0 {
                SourceKey::User(u) => {
                    let e = self.cur.iter().find(|e| e.user == Some(*u));
                    let voiced = e.is_some_and(|e| e.voiced);
                    let mut speakers = SmallVec::new();
                    if voiced {
                        speakers.push(*u);
                    }
                    TickMeta {
                        voiced,
                        concealed: e.is_some_and(|e| e.concealed),
                        synthetic,
                        speakers,
                        captured_at,
                    }
                }
                SourceKey::Bus(b) => {
                    let be = self.buses.get(b);
                    TickMeta {
                        voiced: be.is_some_and(|b| b.activity.voiced),
                        concealed: be.is_some_and(|b| b.concealed),
                        synthetic,
                        speakers: be.map(|b| b.activity.speakers.clone()).unwrap_or_default(),
                        captured_at,
                    }
                }
            };
            l.lane.feed(tick, &c.out, &meta, &mut l.emitted);
        }
    }

    /// Pushes this tick's chunks to their subscriptions and completes `set_filter` moves.
    fn fan_out(&mut self, tick: u64) {
        let end_ms = ticks_ms(tick.saturating_add(1));
        let mut finished: SmallVec<[SubId; 4]> = SmallVec::new();
        for (id, s) in &self.subs {
            if s.queue.is_closed() {
                continue;
            }
            let switch = s.pending.as_ref().map(|(_, sw)| *sw);
            if let Some(l) = self.lanes.get(&s.lane) {
                for c in &l.emitted {
                    let t = c.meta.timeline_ms;
                    if t >= s.start_ms && switch.is_none_or(|sw| t < sw) {
                        s.queue.push(c.clone());
                    }
                }
            }
            if let Some((nk, sw)) = &s.pending {
                if let Some(l) = self.lanes.get(nk) {
                    for c in &l.emitted {
                        let t = c.meta.timeline_ms;
                        if t >= *sw && t >= s.start_ms {
                            s.queue.push(c.clone());
                        }
                    }
                }
                if *sw <= end_ms {
                    finished.push(*id);
                }
            }
        }
        for id in finished {
            let old = self.subs.get_mut(&id).and_then(|s| {
                let (nk, sw) = s.pending.take()?;
                s.start_ms = s.start_ms.max(sw);
                Some(std::mem::replace(&mut s.lane, nk))
            });
            if let Some(old) = old {
                self.release_lane(&old);
            }
        }
    }
}
