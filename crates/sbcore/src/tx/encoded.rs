//! Push-based encoded streams (W4, §7.9): `ByteRing` (L8 + condvar) → a `songbird-decode-N` thread
//! (symphonia probe/decode, `StreamResampler` to 48 kHz) → an internal `PcmQueue` (48 kHz stereo,
//! 1 s cap) → `LiveReader`. At most `MAX_ENCODED_WORKERS` decode threads per process.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use parking_lot::{Condvar, Mutex};

use super::queue::PcmQueue;
use super::{CloseMode, HouseStatus, LiveSource};
use crate::error::CoreError;
use crate::events::EventHub;
use crate::tracks::flags::DoneGuard;
use crate::wake::{WakeId, WakeToken};

/// Live decode threads in this process (`MAX_ENCODED_WORKERS`).
pub static ENCODED_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// `open_encoded_stream` options. No hint: symphonia 0.5.5 ignores it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EncodedSpec {
    /// `ENCODED_BUFFER_BYTES`.
    pub max_buffer_bytes: usize,
    pub volume: f32,
}

/// State of the byte ring (under L8).
#[derive(Debug, Default)]
struct RingInner {
    bytes: VecDeque<u8>,
    finished: bool,
    closed: bool,
    space_waiting: bool,
}

/// Encoded bytes between Python writers and the decode thread.
#[derive(Debug)]
struct ByteRing {
    /// L8.
    inner: Mutex<RingInner>,
    /// Signalled when bytes arrive, on finish and on close.
    readable: Condvar,
    cap: usize,
}

/// What `native.EncodedSink` holds.
pub struct EncodedShared {
    id: u32,
    ring: ByteRing,
    /// Decoded lookahead (48 kHz stereo, fixed 1 s cap).
    pcm: Arc<PcmQueue>,
    /// Signalled by the housekeeper when the internal queue has space; `close()` wakes it too.
    space: Condvar,
    space_lock: Mutex<()>,
    token: WakeToken,
}

impl EncodedShared {
    /// Appends encoded bytes (`PlaybackFull` when the ring is full; nothing consumed).
    pub fn write(&self, data: &[u8]) -> Result<(), CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Arms a space wake-up for `n` bytes; `true` ⇒ there is already enough space.
    pub fn arm_space(&self, n: usize) -> bool {
        false
    }

    /// End of input: the track ends after the decoded audio has played.
    pub fn finish(&self) {
        // phase 0: nothing to finish.
    }

    pub fn wake_id(&self) -> WakeId {
        self.token.id()
    }
}

impl LiveSource for EncodedShared {
    fn id(&self) -> u32 {
        self.id
    }

    fn housekeep(&self, now: Instant, events: &EventHub) -> HouseStatus {
        HouseStatus::Live
    }

    fn on_link(&self, up: bool) {
        // phase 0: link state is not tracked yet.
    }

    fn close(&self, mode: CloseMode) {
        // phase 0: nothing to close.
    }
}

/// Opens the ring, spawns the decode thread and builds the (paused) songbird input
/// (`LimitExceeded` beyond `MAX_ENCODED_WORKERS`). The internal `PcmQueue` takes its id from
/// `tx::next_source_id()`, and `EncodedShared`'s `LiveSource::id` is that id.
pub fn spawn_encoded(
    spec: EncodedSpec,
    token: WakeToken,
    done: DoneGuard,
) -> Result<(Arc<EncodedShared>, songbird::input::Input), CoreError> {
    Err(CoreError::Internal("unimplemented"))
}
