//! Every track of one connection (W2, §7.10): `Weak` references only, pruned by the housekeeper.
#![allow(dead_code, unused_variables, clippy::ptr_arg)] // phase-0 skeleton

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;
use songbird::tracks::{TrackHandle, TrackState};

use super::flags::{DoneGuard, TrackEndReason, TrackFlags, TrackKind, TrackRef};

/// Registry of a connection's tracks (L10).
#[derive(Debug)]
pub struct TrackRegistry {
    list: Mutex<Vec<Weak<TrackFlags>>>,
    /// songbird uuid (`as_u128`) → flags, for Dispatch track events.
    map: Mutex<HashMap<u128, Weak<TrackFlags>>>,
    next_id: AtomicU64,
    conn_closing: Arc<AtomicBool>,
}

impl TrackRegistry {
    /// `conn_closing` is the connection's `closing` flag (`disconnected` resolution, §7.10).
    pub fn new(conn_closing: Arc<AtomicBool>) -> Self {
        TrackRegistry {
            list: Mutex::new(Vec::new()),
            map: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            conn_closing,
        }
    }

    /// New flags (stable id per connection) and the guard its input will own.
    pub fn register(&self, kind: TrackKind) -> (TrackRef, DoneGuard) {
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        let flags = Arc::new(TrackFlags::new(id, kind, self.conn_closing.clone()));
        {
            let mut list = self.list.lock();
            list.retain(|w| w.strong_count() > 0);
            list.push(Arc::downgrade(&flags));
        }
        let done = DoneGuard::new(flags.clone());
        (
            TrackRef {
                flags,
                handle: Arc::new(OnceLock::new()),
            },
            done,
        )
    }

    /// Attaches songbird's handle (and uuid) once the track was handed to the Driver.
    pub fn bind(&self, r: &TrackRef, h: TrackHandle) {
        // phase 0: the uuid map is not maintained yet.
        r.flags.set_uuid(h.uuid().as_u128());
        let _ = r.handle.set(h);
    }

    /// Dispatch's global `End`/`Error` handler: authoritative `resolve()` (§7.10).
    pub fn on_track_event(&self, list: &[(&TrackState, &TrackHandle)]) {
        // phase 0: events are ignored.
    }

    /// Finalises DROPPED-but-unresolved tracks older than 2 ticks (§7.10) and reports newly
    /// resolved ones.
    pub fn poll_ended(&self, tick: u64, out: &mut Vec<(u64, TrackEndReason)>) {
        // phase 0: nothing is finalised.
    }

    /// Resolves every unresolved track with `r` (T8).
    pub fn finish_all(&self, r: TrackEndReason) {
        // phase 0: nothing is resolved.
    }

    /// Unresolved `play()`/queue tracks, for `MAX_PLAYING_TRACKS_PER_CONN`.
    pub fn playing(&self) -> usize {
        0
    }
}
