//! Item tags of a `PcmQueue` (W4, §7.7): ≤ `MAX_TAGS` live tags (eviction: oldest closed, else the
//! least-recently-written open one) and a FIFO set of ≤ `MAX_REJECTED_TAGS` rejected names.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::VecDeque;
use std::sync::Arc;

/// Index of a tag in the [`TagTable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TagId(pub(crate) u16);

/// Accounting of one tag (input-rate frames; gap silence excluded).
#[derive(Clone, Debug)]
pub(crate) struct TagEntry {
    pub(crate) name: Arc<str>,
    pub(crate) written: u64,
    pub(crate) played: u64,
    /// Pull index of the last pull that played this tag.
    pub(crate) last_pull: u64,
    /// Write sequence of the last accepted write.
    pub(crate) last_write: u64,
    pub(crate) open: bool,
    /// `end_item(tag)` was called.
    pub(crate) ended: bool,
}

/// The tag table.
#[derive(Debug, Default)]
pub(crate) struct TagTable {
    entries: Vec<TagEntry>,
    evicted: u64,
}

impl TagTable {
    pub(crate) fn new() -> Self {
        TagTable::default()
    }

    /// The entry for `name`, opening a new one (evicting if full) if needed.
    pub(crate) fn open(&mut self, name: &str, write_seq: u64) -> Option<TagId> {
        None
    }

    pub(crate) fn get(&self, id: TagId) -> Option<&TagEntry> {
        self.entries.get(usize::from(id.0))
    }

    pub(crate) fn find(&self, name: &str) -> Option<TagId> {
        None
    }

    /// Closes every open tag (clear, Active → Idle).
    pub(crate) fn close_all(&mut self) {
        for e in &mut self.entries {
            e.open = false;
        }
    }

    pub(crate) fn evicted(&self) -> u64 {
        self.evicted
    }
}

/// Names rejected by `clear(reject_late=True)` (FIFO eviction).
#[derive(Debug, Default)]
pub(crate) struct RejectSet {
    names: VecDeque<Arc<str>>,
}

impl RejectSet {
    pub(crate) fn new() -> Self {
        RejectSet::default()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.names.iter().any(|n| n.as_ref() == name)
    }

    pub(crate) fn insert(&mut self, name: Arc<str>) {
        // phase 0: not recorded yet.
    }
}
