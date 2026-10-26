//! Every track of one connection (W2, §7.10): `Weak` references only, polled by the housekeeper.
//!
//! Only tracks that can end on their own are polled: started tracks (handed to songbird) and
//! pending queue entries resolved by the queue (`watch`). A track leaves the polled list once its
//! `TrackEnded` was reported, so polling costs O(tracks that may still end).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use songbird::tracks::{PlayMode, TrackHandle, TrackState};

use super::flags::{DROPPED, DoneGuard, PANICKED, TrackEndReason, TrackFlags, TrackKind, TrackRef};
use crate::sync::lock_checked;

/// Registry of a connection's tracks (L10).
#[derive(Debug)]
pub struct TrackRegistry {
    /// L10: tracks that may still end or still need their `TrackEnded` report.
    list: Mutex<Vec<Weak<TrackFlags>>>,
    /// L10: songbird uuid (`as_u128`) → flags, for Dispatch track events.
    map: Mutex<HashMap<u128, Weak<TrackFlags>>>,
    /// L10: every started track whose input songbird may still hold (no `DROPPED` hint yet),
    /// resolved or not: a track stopped or closed while the mixer is parked stays in the mixer
    /// until the mixer is live again (`resident`, bounded by `PREALLOCATED_TRACKS`).
    resident: Mutex<Vec<Weak<TrackFlags>>>,
    next_id: AtomicU64,
    conn_closing: Arc<AtomicBool>,
    /// Tracks whose reader, decoder or decode thread caught a panic (`PANICKED`), counted when
    /// their end is reported (`stats()["panics"]`, §5.7).
    panics: AtomicU64,
}

impl TrackRegistry {
    /// `conn_closing` is the connection's `closing` flag (`disconnected` resolution, §7.10).
    pub fn new(conn_closing: Arc<AtomicBool>) -> Self {
        TrackRegistry {
            list: Mutex::new(Vec::new()),
            map: Mutex::new(HashMap::new()),
            resident: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            conn_closing,
            panics: AtomicU64::new(0),
        }
    }

    /// Whether the owning connection is closing.
    pub fn is_closing(&self) -> bool {
        self.conn_closing.load(Ordering::Acquire)
    }

    /// New flags (stable id per connection) and the guard its input will own.
    pub fn register(&self, kind: TrackKind) -> (TrackRef, DoneGuard) {
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        let flags = Arc::new(TrackFlags::new(id, kind, self.conn_closing.clone()));
        let done = DoneGuard::new(flags.clone());
        (TrackRef::new(flags), done)
    }

    /// Attaches songbird's handle (and uuid) once the track was handed to the Driver.
    pub fn bind(&self, r: &TrackRef, h: TrackHandle) {
        let uuid = h.uuid().as_u128();
        r.flags.set_uuid(uuid);
        {
            let mut map = lock_checked(&self.map);
            map.retain(|_, w| w.upgrade().is_some_and(|f| !f.is_reported()));
            map.insert(uuid, Arc::downgrade(&r.flags));
        }
        let _ = r.handle.set(h);
        self.mark_started(r);
    }

    /// Marks a track as handed to songbird (detached connections: to the fake control) and starts
    /// polling it.
    pub(crate) fn mark_started(&self, r: &TrackRef) {
        r.flags.mark_started();
        {
            let mut v = lock_checked(&self.resident);
            v.retain(is_resident);
            v.push(Arc::downgrade(&r.flags));
        }
        self.watch(&r.flags);
    }

    /// Started tracks whose input songbird may still hold (live and memory, resolved or not): a
    /// track's input is gone once its `DoneGuard` dropped (`DROPPED`). Bounded by
    /// `PREALLOCATED_TRACKS` at admission, because `MAX_LIVE_SOURCES_PER_CONN` counts only open
    /// streams and `playing()` only unresolved tracks, while a parked mixer keeps every closed
    /// stream and stopped track until it is live again.
    pub fn resident(&self) -> usize {
        let mut v = lock_checked(&self.resident);
        v.retain(is_resident);
        v.len()
    }

    /// Polls `flags` until its `TrackEnded` was reported (e.g. a pending entry the queue resolved).
    pub(crate) fn watch(&self, flags: &Arc<TrackFlags>) {
        if flags.set_watched() {
            lock_checked(&self.list).push(Arc::downgrade(flags));
        }
        // A track started while the connection closes (e.g. `play()` racing `close()`): T8's
        // `finish_all` may already have swept the list, and no housekeeper runs any more. The list
        // lock orders this load after T0 whenever `finish_all` missed the entry, so the track
        // always resolves.
        if self.is_closing() {
            flags.resolve(TrackEndReason::Disconnected, None);
        }
    }

    /// Dispatch's global `End`/`Error` handler: authoritative `resolve()` (§7.10).
    /// `PlayMode::End` → finished, `Stop` → stopped, `Errored(e)` → error.
    pub fn on_track_event(&self, list: &[(&TrackState, &TrackHandle)]) {
        for (state, handle) in list {
            let (reason, err) = match &state.playing {
                PlayMode::End => (TrackEndReason::Finished, None),
                PlayMode::Stop => (TrackEndReason::Stopped, None),
                PlayMode::Errored(e) => (TrackEndReason::Error, Some(e.to_string())),
                _ => continue,
            };
            let flags = lock_checked(&self.map)
                .get(&handle.uuid().as_u128())
                .and_then(Weak::upgrade);
            if let Some(f) = flags {
                f.resolve(reason, err.as_deref());
            }
        }
    }

    /// Finalises DROPPED-but-unresolved tracks older than 2 ticks (§7.10) and reports newly
    /// resolved ones. Reported and dead entries leave the list.
    pub fn poll_ended(&self, tick: u64, out: &mut Vec<(u64, TrackEndReason)>) {
        let mut list = lock_checked(&self.list);
        list.retain(|w| {
            let Some(f) = w.upgrade() else {
                return false;
            };
            f.finalise_dropped(tick);
            if f.take_report() {
                if f.hints() & PANICKED != 0 {
                    self.panics.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(r) = f.ended() {
                    out.push((f.id, r));
                }
                return false;
            }
            true
        });
    }

    /// Tracks whose reader, decoder or decode thread caught a panic (reported ends only).
    pub fn panics(&self) -> u64 {
        self.panics.load(Ordering::Relaxed)
    }

    /// Resolves every unresolved polled track with `r` (T8).
    pub fn finish_all(&self, r: TrackEndReason) {
        let list = lock_checked(&self.list);
        for f in list.iter().filter_map(Weak::upgrade) {
            f.resolve(r, None);
        }
    }

    /// Unresolved started `play()`/queue tracks, for `MAX_PLAYING_TRACKS_PER_CONN` (live sources
    /// are counted separately).
    pub fn playing(&self) -> usize {
        lock_checked(&self.list)
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|f| !f.kind.is_live() && f.is_started() && f.ended().is_none())
            .count()
    }

    /// Whether any track may still end or still needs its report (the worker then polls every tick).
    pub fn needs_poll(&self) -> bool {
        !lock_checked(&self.list).is_empty()
    }
}

/// Whether songbird may still hold the input of this started track (its `DoneGuard` lives).
fn is_resident(w: &Weak<TrackFlags>) -> bool {
    w.upgrade().is_some_and(|f| f.hints() & DROPPED == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> (TrackRegistry, Arc<AtomicBool>) {
        let closing = Arc::new(AtomicBool::new(false));
        (TrackRegistry::new(closing.clone()), closing)
    }

    #[test]
    fn ids_are_stable_and_unique() {
        let (r, _) = reg();
        let (a, _ga) = r.register(TrackKind::File);
        let (b, _gb) = r.register(TrackKind::Pcm);
        assert_eq!(a.flags.id + 1, b.flags.id);
        assert!(!a.is_started());
        assert!(!r.needs_poll(), "unstarted tracks are not polled");
    }

    #[test]
    fn dropped_started_track_resolves_after_grace_and_reports_once() {
        let (r, _) = reg();
        let (t, guard) = r.register(TrackKind::File);
        r.mark_started(&t);
        assert_eq!(r.playing(), 1);
        drop(guard);
        let mut out = Vec::new();
        r.poll_ended(1, &mut out);
        r.poll_ended(2, &mut out);
        assert!(out.is_empty());
        r.poll_ended(3, &mut out);
        assert_eq!(out, vec![(t.flags.id, TrackEndReason::Stopped)]);
        assert_eq!(r.playing(), 0);
        assert!(!r.needs_poll());
        out.clear();
        r.poll_ended(4, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn finish_all_resolves_disconnected() {
        let (r, closing) = reg();
        let (a, _ga) = r.register(TrackKind::File);
        let (b, _gb) = r.register(TrackKind::Playback);
        r.mark_started(&a);
        r.mark_started(&b);
        b.flags.resolve(TrackEndReason::Finished, None);
        closing.store(true, Ordering::Release);
        r.finish_all(TrackEndReason::Disconnected);
        assert_eq!(a.flags.ended(), Some(TrackEndReason::Disconnected));
        assert_eq!(b.flags.ended(), Some(TrackEndReason::Finished));
    }

    #[test]
    fn playing_counts_started_unresolved_memory_tracks() {
        let (r, _) = reg();
        let mut keep = Vec::new();
        for kind in [TrackKind::File, TrackKind::Pcm, TrackKind::Playback] {
            let (t, g) = r.register(kind);
            r.mark_started(&t);
            keep.push((t, g));
        }
        let (pending, _g) = r.register(TrackKind::Opus);
        assert_eq!(r.playing(), 2);
        keep[0].0.flags.resolve(TrackEndReason::Stopped, None);
        assert_eq!(r.playing(), 1);
        drop(pending);
    }

    #[test]
    fn resident_counts_started_tracks_until_their_input_is_dropped() {
        let (r, _) = reg();
        let (a, ga) = r.register(TrackKind::File);
        let (b, gb) = r.register(TrackKind::Playback);
        let (_pending, _gp) = r.register(TrackKind::Pcm);
        r.mark_started(&a);
        r.mark_started(&b);
        assert_eq!(r.resident(), 2, "pending entries are not resident");
        // A stop resolved locally (parked mixer): no longer playing, still in songbird.
        a.flags.resolve(TrackEndReason::Stopped, None);
        let mut out = Vec::new();
        r.poll_ended(0, &mut out);
        assert_eq!(out, vec![(a.flags.id, TrackEndReason::Stopped)]);
        assert_eq!(r.playing(), 0);
        assert_eq!(r.resident(), 2, "reported, but its input is still held");
        drop(ga); // songbird disposed of the input
        assert_eq!(r.resident(), 1);
        drop(gb);
        drop(b);
        assert_eq!(r.resident(), 0);
    }

    #[test]
    fn watch_is_idempotent_and_reports_resolved_pending_entries() {
        let (r, _) = reg();
        let (t, _g) = r.register(TrackKind::File);
        t.flags.resolve(TrackEndReason::Stopped, None);
        r.watch(&t.flags);
        r.watch(&t.flags);
        let mut out = Vec::new();
        r.poll_ended(0, &mut out);
        assert_eq!(out, vec![(t.flags.id, TrackEndReason::Stopped)]);
    }

    #[test]
    fn a_track_started_after_teardown_resolves_disconnected() {
        // `play()` racing `close()`: the track is bound after T8's `finish_all` swept the list.
        let (r, closing) = reg();
        let (t, _g) = r.register(TrackKind::File);
        closing.store(true, Ordering::Release);
        r.finish_all(TrackEndReason::Disconnected);
        assert_eq!(t.flags.ended(), None, "not in the list yet");
        r.mark_started(&t);
        assert_eq!(t.flags.ended(), Some(TrackEndReason::Disconnected));
        // An already resolved entry keeps its reason.
        let (s, _g2) = r.register(TrackKind::File);
        s.flags.resolve(TrackEndReason::Stopped, None);
        r.watch(&s.flags);
        assert_eq!(s.flags.ended(), Some(TrackEndReason::Stopped));
    }

    #[test]
    fn panicked_tracks_are_counted_once_when_reported() {
        let (r, _) = reg();
        let (a, _ga) = r.register(TrackKind::Playback);
        let (b, _gb) = r.register(TrackKind::File);
        r.mark_started(&a);
        r.mark_started(&b);
        a.flags.hint(super::super::flags::PANICKED);
        let mut out = Vec::new();
        r.poll_ended(0, &mut out);
        assert_eq!(r.panics(), 0, "counted when the end is reported");
        a.flags.resolve(TrackEndReason::Finished, None);
        b.flags.resolve(TrackEndReason::Finished, None);
        r.poll_ended(1, &mut out);
        r.poll_ended(2, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(r.panics(), 1);
    }

    #[test]
    fn dead_entries_are_pruned() {
        let (r, _) = reg();
        {
            let (t, g) = r.register(TrackKind::File);
            r.mark_started(&t);
            drop(g);
        }
        assert!(r.needs_poll());
        let mut out = Vec::new();
        r.poll_ended(0, &mut out);
        assert!(!r.needs_poll());
        assert!(out.is_empty());
    }
}
