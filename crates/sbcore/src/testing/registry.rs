//! Process-wide bookkeeping of the test hooks (§13.2): per-connection injection state, the parking
//! lot for the songbird inputs of detached connections, and the `FakeTrackControl` call logs.
//!
//! Every table is a leaf `parking_lot` lock (L10 class), bounded, and never held while an input or
//! a control is dropped (drops happen after unlocking).

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Instant;

use parking_lot::Mutex;
use songbird::input::Input;

use crate::clock::ManualClock;
use crate::conn::shared::ConnShared;
use crate::tracks::flags::TrackFlags;
use crate::tx::{FakeTrackControl, TrackControl};

/// Identity of a connection: the address of its `ConnShared` (stable while any `Arc`/`Weak`
/// exists; entries are removed when the harness drops).
pub(crate) type ConnKey = usize;

pub(crate) fn key_of(conn: &ConnShared) -> ConnKey {
    conn as *const ConnShared as usize
}

/// Most connections whose injection state is remembered at once (oldest evicted).
const MAX_CONNS: usize = 1_024;
/// Most SSRC sequence counters per connection (cleared when exceeded).
const MAX_SSRCS: usize = 4_096;
/// Most parked inputs across the process (a further input is dropped at once: its track ends).
pub const MAX_PARKED: usize = 4_096;
/// Most live `FakeTrackControl` logs remembered (oldest forgotten; the control keeps working).
const MAX_FAKES: usize = 65_536;

struct ConnEntry {
    key: ConnKey,
    clock: Option<Weak<ManualClock>>,
    seqs: HashMap<u32, u16>,
}

static CONNS: Mutex<Vec<ConnEntry>> = Mutex::new(Vec::new());

/// Registers a detached connection's manual clock (replacing any stale entry at that address).
pub(crate) fn register_conn(key: ConnKey, clock: &Arc<ManualClock>) {
    let evicted = {
        let mut conns = CONNS.lock();
        conns.retain(|e| e.key != key);
        let evicted = if conns.len() >= MAX_CONNS {
            Some(conns.remove(0))
        } else {
            None
        };
        conns.push(ConnEntry {
            key,
            clock: Some(Arc::downgrade(clock)),
            seqs: HashMap::new(),
        });
        evicted
    };
    drop(evicted);
}

/// Forgets a connection's injection state.
pub(crate) fn unregister_conn(key: ConnKey) {
    let removed: Vec<ConnEntry> = {
        let mut conns = CONNS.lock();
        let (gone, kept): (Vec<_>, Vec<_>) = conns.drain(..).partition(|e| e.key == key);
        *conns = kept;
        gone
    };
    drop(removed);
}

/// "Now" for a connection: its manual clock if detached, else the real clock.
pub(crate) fn now_for(key: ConnKey) -> Instant {
    let clock = CONNS
        .lock()
        .iter()
        .find(|e| e.key == key)
        .and_then(|e| e.clock.as_ref().and_then(Weak::upgrade));
    match clock {
        Some(c) => c.now(),
        None => Instant::now(),
    }
}

/// The next RTP sequence number of `ssrc` on a connection (consecutive per SSRC, wrapping).
pub(crate) fn next_seq(key: ConnKey, ssrc: u32) -> u16 {
    let mut conns = CONNS.lock();
    let idx = match conns.iter().position(|e| e.key == key) {
        Some(i) => i,
        None => {
            if conns.len() >= MAX_CONNS {
                conns.remove(0);
            }
            conns.push(ConnEntry {
                key,
                clock: None,
                seqs: HashMap::new(),
            });
            conns.len() - 1
        }
    };
    match conns.get_mut(idx) {
        Some(entry) => {
            if entry.seqs.len() >= MAX_SSRCS && !entry.seqs.contains_key(&ssrc) {
                entry.seqs.clear();
            }
            let seq = entry.seqs.entry(ssrc).or_insert(0);
            let cur = *seq;
            *seq = seq.wrapping_add(1);
            cur
        }
        None => 0,
    }
}

// ── parking lot ─────────────────────────────────────────────────────────────────────────────────────

struct Parked {
    conn: ConnKey,
    source_id: Option<u32>,
    flags: Arc<TrackFlags>,
    input: Input,
}

static PARKED: Mutex<Vec<Parked>> = Mutex::new(Vec::new());

/// Parks the songbird input of a detached connection's track (no Driver exists to play it).
///
/// `source_id` is the `LiveSource::id()` of a live source (`MixerHarness::take` looks it up), or
/// `None` for memory/file tracks. The input is dropped, exactly as songbird would dispose it, when
/// its flags resolve (`stop()`, `End`), when the connection closes, or when the harness drops; if
/// [`MAX_PARKED`] inputs are already parked it is dropped at once (its track then ends).
pub fn park_input(conn: &ConnShared, source_id: Option<u32>, flags: Arc<TrackFlags>, input: Input) {
    park_keyed(key_of(conn), source_id, flags, input);
}

/// [`park_input`] by connection key (also used to put back an input `take_parked` returned).
pub(crate) fn park_keyed(
    conn: ConnKey,
    source_id: Option<u32>,
    flags: Arc<TrackFlags>,
    input: Input,
) {
    let rejected = {
        let mut parked = PARKED.lock();
        if parked.len() >= MAX_PARKED {
            Some(input)
        } else {
            parked.push(Parked {
                conn,
                source_id,
                flags,
                input,
            });
            None
        }
    };
    drop(rejected);
}

/// Removes the parked input of live source `source_id`: `(connection, flags, input)`.
pub(crate) fn take_parked(source_id: u32) -> Option<(ConnKey, Arc<TrackFlags>, Input)> {
    let mut parked = PARKED.lock();
    let idx = parked.iter().position(|p| p.source_id == Some(source_id))?;
    let p = parked.swap_remove(idx);
    Some((p.conn, p.flags, p.input))
}

/// Drops the parked inputs of `conn`: every one if `all`, else those whose track has ended
/// (songbird disposes a stopped/finished track's input). Drops happen after unlocking.
pub(crate) fn sweep(conn: ConnKey, all: bool) -> usize {
    let dropped: Vec<Parked> = {
        let mut parked = PARKED.lock();
        let (gone, kept): (Vec<_>, Vec<_>) = parked
            .drain(..)
            .partition(|p| p.conn == conn && (all || p.flags.ended().is_some()));
        *parked = kept;
        gone
    };
    let n = dropped.len();
    drop(dropped);
    n
}

/// Number of parked inputs of `conn`.
pub(crate) fn parked_count(conn: ConnKey) -> usize {
    PARKED.lock().iter().filter(|p| p.conn == conn).count()
}

// ── fake track controls ─────────────────────────────────────────────────────────────────────────────

struct FakeEntry {
    flags: Weak<TrackFlags>,
    fake: Weak<FakeTrackControl>,
}

static FAKES: Mutex<Vec<FakeEntry>> = Mutex::new(Vec::new());

/// A `TrackControl` forwarding to a shared [`FakeTrackControl`] whose log `track_log` can read.
struct SharedFake(Arc<FakeTrackControl>);

impl TrackControl for SharedFake {
    fn pause(&self) {
        self.0.pause();
    }

    fn play(&self) {
        self.0.play();
    }

    fn stop(&self) {
        self.0.stop();
    }
}

/// The track control a detached connection attaches instead of a songbird `TrackHandle` (§13.2):
/// it records `pause`/`play`/`stop`, and `stop()` resolves `flags` as a Dispatch `End(Stop)` would.
pub fn fake_track_control(flags: &Arc<TrackFlags>) -> Box<dyn TrackControl> {
    let fake = Arc::new(FakeTrackControl::new(flags.clone()));
    {
        let mut fakes = FAKES.lock();
        fakes.retain(|e| e.fake.strong_count() > 0);
        if fakes.len() >= MAX_FAKES {
            fakes.remove(0);
        }
        fakes.push(FakeEntry {
            flags: Arc::downgrade(flags),
            fake: Arc::downgrade(&fake),
        });
    }
    Box::new(SharedFake(fake))
}

/// The call log of the fake control attached to `flags`; empty if there is none.
pub(crate) fn fake_log(flags: &Arc<TrackFlags>) -> Vec<&'static str> {
    let fake = FAKES
        .lock()
        .iter()
        .rev()
        .find(|e| std::ptr::eq(e.flags.as_ptr(), Arc::as_ptr(flags)))
        .and_then(|e| e.fake.upgrade());
    fake.map(|f| f.log()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::tracks::flags::{TrackEndReason, TrackKind};

    fn flags(id: u64) -> Arc<TrackFlags> {
        Arc::new(TrackFlags::new(
            id,
            TrackKind::Playback,
            Arc::new(AtomicBool::new(false)),
        ))
    }

    #[test]
    fn fake_control_logs_and_stop_resolves() {
        let f = flags(1);
        let ctl = fake_track_control(&f);
        ctl.play();
        ctl.pause();
        assert_eq!(fake_log(&f), vec!["play", "pause"]);
        assert!(f.ended().is_none());
        ctl.stop();
        assert_eq!(f.ended(), Some(TrackEndReason::Stopped));
        assert_eq!(fake_log(&f), vec!["play", "pause", "stop"]);
        // A track without a fake control has an empty log.
        assert!(fake_log(&flags(2)).is_empty());
        drop(ctl);
        assert!(fake_log(&f).is_empty());
    }

    #[test]
    fn sequence_numbers_are_per_connection_and_ssrc() {
        let (a, b) = (0x1000_usize, 0x2000_usize);
        assert_eq!(next_seq(a, 1), 0);
        assert_eq!(next_seq(a, 1), 1);
        assert_eq!(next_seq(a, 2), 0);
        assert_eq!(next_seq(b, 1), 0);
        unregister_conn(a);
        assert_eq!(next_seq(a, 1), 0);
        unregister_conn(a);
        unregister_conn(b);
    }

    #[test]
    fn parking_take_and_sweep() {
        let key = 0x4000_usize;
        let (a, b, c) = (flags(10), flags(11), flags(12));
        park_keyed(key, Some(900_001), a.clone(), Input::from(vec![0u8; 4]));
        park_keyed(key, Some(900_002), b.clone(), Input::from(vec![0u8; 4]));
        park_keyed(key, None, c.clone(), Input::from(vec![0u8; 4]));
        assert_eq!(parked_count(key), 3);
        let (k, f, input) = take_parked(900_001).unwrap();
        assert_eq!(k, key);
        assert!(Arc::ptr_eq(&f, &a));
        assert!(take_parked(900_001).is_none());
        park_keyed(k, Some(900_001), f, input);
        // Only ended tracks are swept unless `all`.
        assert_eq!(sweep(key, false), 0);
        b.resolve(TrackEndReason::Stopped, None);
        assert_eq!(sweep(key, false), 1);
        assert_eq!(parked_count(key), 2);
        assert_eq!(sweep(key, true), 2);
        assert_eq!(parked_count(key), 0);
    }

    #[test]
    fn now_for_uses_the_registered_manual_clock() {
        let key = 0x3000_usize;
        let clock = ManualClock::new();
        register_conn(key, &clock);
        let t0 = now_for(key);
        clock.advance(std::time::Duration::from_millis(500));
        assert_eq!(now_for(key) - t0, std::time::Duration::from_millis(500));
        unregister_conn(key);
    }
}
