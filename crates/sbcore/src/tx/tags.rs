//! Segment tags of a `PcmQueue` (W4, §7.7): ≤ `MAX_TAGS` live tags (eviction: oldest closed, else the
//! least-recently-written open one) and a FIFO set of ≤ `MAX_REJECTED_TAGS` rejected names.
//!
//! Everything here runs under L6. Lookups scan at most `MAX_TAGS` entries (with a one-entry cache for
//! the common "same tag as the previous write" case); a new entry allocates its `Arc<str>` name only
//! when the tag is first seen.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::limits::{MAX_REJECTED_TAGS, MAX_TAGS};

/// A slot of the [`TagTable`] plus the slot's generation, so a queued segment that still refers to
/// an evicted tag never credits its replacement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TagId {
    idx: u16,
    generation: u32,
}

/// Accounting of one tag (input-rate frames; gap silence excluded).
#[derive(Clone, Debug)]
pub(crate) struct TagEntry {
    pub(crate) name: Arc<str>,
    pub(crate) written: u64,
    pub(crate) played: u64,
    /// Pull index of the last pull that played this tag (`None`: never pulled).
    pub(crate) last_pull: Option<u64>,
    /// Write sequence of the last accepted write.
    pub(crate) last_write: u64,
    pub(crate) open: bool,
    /// `end_segment(tag)` was called.
    pub(crate) ended: bool,
}

#[derive(Clone, Debug)]
struct Slot {
    generation: u32,
    entry: Option<TagEntry>,
}

/// The tag table (≤ `MAX_TAGS` entries).
#[derive(Debug, Default)]
pub(crate) struct TagTable {
    slots: Vec<Slot>,
    live: usize,
    evicted: u64,
    /// Slot of the most recently opened/found tag.
    last: Option<u16>,
}

impl TagTable {
    pub(crate) fn new() -> Self {
        TagTable::default()
    }

    fn id_of(&self, idx: usize) -> Option<TagId> {
        let slot = self.slots.get(idx)?;
        slot.entry.as_ref()?;
        Some(TagId {
            idx: u16::try_from(idx).ok()?,
            generation: slot.generation,
        })
    }

    /// The live entry named `name`.
    pub(crate) fn find(&self, name: &str) -> Option<TagId> {
        if let Some(last) = self.last
            && let Some(Slot { entry: Some(e), .. }) = self.slots.get(usize::from(last))
            && e.name.as_ref() == name
        {
            return self.id_of(usize::from(last));
        }
        let idx = self
            .slots
            .iter()
            .position(|s| s.entry.as_ref().is_some_and(|e| e.name.as_ref() == name))?;
        self.id_of(idx)
    }

    /// The entry for `name`, opened (and created, evicting one entry if the table is full) for a
    /// write with sequence number `write_seq`. Returns the id and whether an entry was evicted.
    pub(crate) fn open(&mut self, name: &str, write_seq: u64) -> (Option<TagId>, bool) {
        if let Some(id) = self.find(name) {
            if let Some(e) = self.get_mut(id) {
                e.open = true;
                e.last_write = write_seq;
            }
            self.last = Some(id.idx);
            return (Some(id), false);
        }
        let mut evicted = false;
        if self.live >= MAX_TAGS {
            if let Some(victim) = self.victim() {
                if let Some(slot) = self.slots.get_mut(victim) {
                    slot.entry = None;
                    slot.generation = slot.generation.wrapping_add(1);
                }
                self.live -= 1;
                self.evicted += 1;
                evicted = true;
            } else {
                return (None, false);
            }
        }
        let idx = match self.slots.iter().position(|s| s.entry.is_none()) {
            Some(i) => i,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    entry: None,
                });
                self.slots.len() - 1
            }
        };
        let Some(slot) = self.slots.get_mut(idx) else {
            return (None, evicted);
        };
        slot.entry = Some(TagEntry {
            name: Arc::from(name),
            written: 0,
            played: 0,
            last_pull: None,
            last_write: write_seq,
            open: true,
            ended: false,
        });
        self.live += 1;
        let id = self.id_of(idx);
        self.last = id.map(|i| i.idx);
        (id, evicted)
    }

    /// Eviction order (§7.7): the oldest closed tag, otherwise the open tag with the oldest
    /// `last_write`.
    fn victim(&self) -> Option<usize> {
        let oldest = |want_open: bool| {
            self.slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.entry.as_ref().map(|e| (i, e)))
                .filter(|(_, e)| e.open == want_open)
                .min_by_key(|(_, e)| e.last_write)
                .map(|(i, _)| i)
        };
        oldest(false).or_else(|| oldest(true))
    }

    pub(crate) fn get(&self, id: TagId) -> Option<&TagEntry> {
        let slot = self.slots.get(usize::from(id.idx))?;
        if slot.generation != id.generation {
            return None;
        }
        slot.entry.as_ref()
    }

    pub(crate) fn get_mut(&mut self, id: TagId) -> Option<&mut TagEntry> {
        let slot = self.slots.get_mut(usize::from(id.idx))?;
        if slot.generation != id.generation {
            return None;
        }
        slot.entry.as_mut()
    }

    /// Closes every open tag (clear, Active → Idle).
    pub(crate) fn close_all(&mut self) {
        for e in self.slots.iter_mut().filter_map(|s| s.entry.as_mut()) {
            e.open = false;
        }
    }

    /// Every live entry.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &TagEntry> {
        self.slots.iter().filter_map(|s| s.entry.as_ref())
    }

    /// Number of live entries.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.live
    }

    #[cfg(test)]
    pub(crate) fn evicted(&self) -> u64 {
        self.evicted
    }
}

/// Names rejected by `clear(reject_late=True)` (FIFO eviction beyond `MAX_REJECTED_TAGS`).
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

    /// Adds `name` (no duplicates); the oldest name is forgotten beyond `MAX_REJECTED_TAGS`.
    pub(crate) fn insert(&mut self, name: Arc<str>) {
        if self.contains(&name) {
            return;
        }
        if self.names.len() >= MAX_REJECTED_TAGS {
            self.names.pop_front();
        }
        self.names.push_back(name);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_find_and_reopen() {
        let mut t = TagTable::new();
        let (a, ev) = t.open("a", 1);
        assert!(!ev);
        let a = a.unwrap();
        assert_eq!(t.find("a"), Some(a));
        assert_eq!(t.find("b"), None);
        t.get_mut(a).unwrap().written = 10;
        t.close_all();
        assert!(!t.get(a).unwrap().open);
        let (a2, _) = t.open("a", 2);
        assert_eq!(a2, Some(a));
        let e = t.get(a).unwrap();
        assert!(e.open);
        assert_eq!(e.written, 10);
        assert_eq!(e.last_write, 2);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn evicts_oldest_closed_then_least_recently_written_open() {
        let mut t = TagTable::new();
        for i in 0..MAX_TAGS {
            t.open(&format!("t{i}"), i as u64);
        }
        assert_eq!(t.len(), MAX_TAGS);
        // Close t10 and t5 (t5 has the older last_write): t5 is evicted first.
        let t10 = t.find("t10").unwrap();
        let t5 = t.find("t5").unwrap();
        t.get_mut(t10).unwrap().open = false;
        t.get_mut(t5).unwrap().open = false;
        let (_, ev) = t.open("new1", 1000);
        assert!(ev);
        assert!(t.find("t5").is_none());
        assert!(t.get(t5).is_none(), "stale id must not resolve");
        assert!(t.find("t10").is_some());
        let (_, ev) = t.open("new2", 1001);
        assert!(ev);
        assert!(t.find("t10").is_none());
        // No closed tags left: the open tag with the oldest last_write (t0) goes.
        let (_, ev) = t.open("new3", 1002);
        assert!(ev);
        assert!(t.find("t0").is_none());
        assert!(t.find("t1").is_some());
        assert_eq!(t.evicted(), 3);
        assert_eq!(t.len(), MAX_TAGS);
        // Re-writing an evicted name starts a fresh entry.
        let (id, _) = t.open("t0", 2000);
        assert_eq!(t.get(id.unwrap()).unwrap().written, 0);
    }

    #[test]
    fn reject_set_is_fifo_and_bounded() {
        let mut r = RejectSet::new();
        for i in 0..(MAX_REJECTED_TAGS + 10) {
            r.insert(Arc::from(format!("r{i}").as_str()));
        }
        assert_eq!(r.len(), MAX_REJECTED_TAGS);
        assert!(!r.contains("r0"));
        assert!(!r.contains("r9"));
        assert!(r.contains("r10"));
        r.insert(Arc::from("r10"));
        assert_eq!(r.len(), MAX_REJECTED_TAGS);
    }
}
