//! The synchronous receive core (W3, §8.4–§8.8): unit-testable without a runtime or songbird.
//!
//! Driven by the ConnWorker: `apply` (commands), `on_input` (ticks, mappings, link changes) and
//! `on_timer` (silence synthesis, pruning, stall watchdog).
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::bus::{MixBus, MixFilter};
use super::convert::{Converter, SourceKey};
use super::decoder::SsrcState;
use super::lane::Lane;
use super::queue::{RxCmd, SubId, SubQueue};
use super::raw::RxInput;
use super::router::{RouterQueue, RouterSpec};
use super::speaking::SpeakingDetector;
use crate::ctrl::CtrlQueue;
use crate::error::CloseReason;
use crate::events::EventHub;
use crate::format::SampleRate;
use crate::limits::TICK_MS;

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
    /// Set while no real tick has arrived for `stall_timeout` (receive on, link up).
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
}

/// A registered subscription.
struct SubEntry {
    queue: Arc<SubQueue>,
    lane: usize,
}

/// A registered router.
struct RouterEntry {
    spec: RouterSpec,
    queue: Arc<RouterQueue>,
}

/// The receive pipeline of one connection.
pub struct RxPipeline {
    cfg: RxConfig,
    events: Arc<EventHub>,
    ctrl: Arc<CtrlQueue>,
    unclassified: Arc<Mutex<Vec<u64>>>,
    ssrc_users: HashMap<u32, u64>,
    states: HashMap<u32, SsrcState>,
    speaking: SpeakingDetector,
    buses: HashMap<MixFilter, (u32, MixBus)>,
    converters: HashMap<(SourceKey, SampleRate, u8), (u32, Converter)>,
    lanes: Vec<Lane>,
    subs: HashMap<SubId, SubEntry>,
    routers: HashMap<u64, RouterEntry>,
    tick_index: u64,
    last_real_tick: Option<Instant>,
    counts: RxCounts,
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
            ssrc_users: HashMap::new(),
            states: HashMap::new(),
            buses: HashMap::new(),
            converters: HashMap::new(),
            lanes: Vec::new(),
            subs: HashMap::new(),
            routers: HashMap::new(),
            tick_index: 0,
            last_real_tick: None,
            counts: RxCounts::default(),
        }
    }

    /// Applies one command (subscribe, unsubscribe, filter change, router, bot flags).
    pub fn apply(&mut self, cmd: RxCmd) {
        // phase 0: commands are dropped.
    }

    /// Processes one input from `Dispatch` (or the test hooks).
    pub fn on_input(&mut self, input: RxInput, now: Instant) {
        // phase 0: inputs are dropped.
    }

    /// Silence synthesis, state pruning and the stall watchdog.
    pub fn on_timer(&mut self, now: Instant) -> RxTimer {
        RxTimer::default()
    }

    /// `20 · tick_index`.
    pub fn timeline_ms(&self) -> u64 {
        self.tick_index.saturating_mul(u64::from(TICK_MS))
    }

    /// Closes every subscription and router queue with `r`.
    pub fn close_all(&mut self, r: CloseReason) {
        // phase 0: nothing is registered.
    }

    pub fn counts(&self) -> RxCounts {
        self.counts
    }
}
