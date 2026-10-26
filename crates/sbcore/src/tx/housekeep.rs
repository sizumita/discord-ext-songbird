//! The send housekeeper (W4, §7.5): runs on the ConnWorker once per tick while live sources exist.
//! Mixer threads cannot notify, so it turns their atomics into wake-ups, events and auto-pause.
#![allow(unused_variables)] // phase-0 skeleton

use std::sync::Weak;
use std::time::Instant;

use super::{CloseMode, LiveSource};
use crate::events::EventHub;

/// Live sources of one connection (`Weak`: Python owns them).
#[derive(Default)]
pub struct Housekeeper {
    sources: Vec<Weak<dyn LiveSource>>,
    link_up: bool,
}

impl Housekeeper {
    pub fn new() -> Self {
        Housekeeper::default()
    }

    /// Registers a source (dead entries are pruned first; ≤ `MAX_LIVE_SOURCES_PER_CONN` live).
    pub fn add(&mut self, s: Weak<dyn LiveSource>) {
        self.sources.retain(|w| w.strong_count() > 0);
        self.sources.push(s);
    }

    /// Link up/down (from `WorkerCmd::Link`).
    pub fn link(&mut self, up: bool) {
        self.link_up = up;
    }

    /// One housekeeping pass; returns the next wake-up (`None` = no live sources).
    pub fn tick(&mut self, now: Instant, events: &EventHub) -> Option<Instant> {
        None
    }

    /// Aborts every live source (worker exit).
    pub fn close_all(&mut self) {
        for s in self.sources.drain(..).filter_map(|w| w.upgrade()) {
            s.close(CloseMode::Abort);
        }
    }
}
