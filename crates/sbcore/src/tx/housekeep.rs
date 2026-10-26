//! The send housekeeper (W4, §7.5): runs on the ConnWorker once per tick while live sources exist.
//! Mixer threads cannot notify, so it turns their atomics into wake-ups, events and auto-pause.

use std::sync::Weak;
use std::time::{Duration, Instant};

use super::{HouseStatus, LiveSource};
use crate::events::EventHub;
use crate::limits::TICK_MS;

/// Live sources of one connection (`Weak`: Python owns them).
#[derive(Default)]
pub struct Housekeeper {
    sources: Vec<Weak<dyn LiveSource>>,
    link_up: bool,
}

impl std::fmt::Debug for Housekeeper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Housekeeper")
            .field("sources", &self.sources.len())
            .field("link_up", &self.link_up)
            .finish()
    }
}

impl Housekeeper {
    pub fn new() -> Self {
        Housekeeper::default()
    }

    /// Registers a source (dead entries are pruned first; the caller enforces
    /// `MAX_LIVE_SOURCES_PER_CONN`). The source learns the current link state at once.
    pub fn add(&mut self, s: Weak<dyn LiveSource>) {
        self.sources.retain(|w| w.strong_count() > 0);
        if let Some(src) = s.upgrade() {
            src.on_link(self.link_up);
            self.sources.push(s);
        }
    }

    /// Link up/down (from `WorkerCmd::Link`): forwarded to every live source.
    pub fn link(&mut self, up: bool) {
        self.link_up = up;
        for s in self.sources.iter().filter_map(Weak::upgrade) {
            s.on_link(up);
        }
    }

    /// One housekeeping pass; returns the next wake-up (`None` = no live sources).
    pub fn tick(&mut self, now: Instant, events: &EventHub) -> Option<Instant> {
        self.sources.retain(|w| match w.upgrade() {
            Some(s) => s.housekeep(now, events) == HouseStatus::Live,
            None => false,
        });
        if self.sources.is_empty() {
            None
        } else {
            now.checked_add(Duration::from_millis(u64::from(TICK_MS)))
        }
    }

    /// Number of registered (possibly dead) sources.
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Closes every live source as the connection closes (worker exit).
    pub fn close_all(&mut self) {
        for s in self.sources.drain(..).filter_map(|w| w.upgrade()) {
            s.conn_closed();
        }
    }
}
