//! Push-based Opus packet streams (W4, §7.9): `OpusQueue` (L7) + `OpusStreamReader` + the registry
//! Opus decoder. The mixer `try_lock`s, moves a packet's `Box<[u8]>` into the `Packet`, and falls
//! back to a pre-allocated `SILENT_FRAME` from a 64-entry reserve on underrun/contention/self-mute.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use songbird::driver::opus;

use super::{CloseMode, HouseStatus, LiveSource};
use crate::error::CoreError;
use crate::events::EventHub;
use crate::tracks::flags::DoneGuard;
use crate::wake::{WakeId, WakeToken};

/// Maximum Opus packet size accepted by `send` (bytes).
pub const MAX_OPUS_PACKET: usize = 1_275;

/// `open_opus_stream` options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OpusStreamSpec {
    /// `OPUS_STREAM_PACKETS`.
    pub max_packets: usize,
    pub volume: f32,
}

/// 1..=1275 bytes and exactly 960 samples at 48 kHz (`InvalidAudio` otherwise).
pub fn validate_opus_packet(p: &[u8]) -> Result<(), CoreError> {
    if p.is_empty() || p.len() > MAX_OPUS_PACKET {
        return Err(CoreError::invalid(
            "an Opus packet must be 1..=1275 bytes long",
        ));
    }
    match opus::packet::get_nb_samples(p, 48_000) {
        Ok(960) => Ok(()),
        _ => Err(CoreError::invalid(
            "an Opus packet must hold exactly 20 ms (960 samples at 48 kHz)",
        )),
    }
}

/// State under L7.
#[derive(Debug, Default)]
struct OpusInner {
    packets: VecDeque<Box<[u8]>>,
    /// Pre-allocated `SILENT_FRAME` boxes (≤ `OPUS_SILENT_RESERVE`), refilled by the housekeeper.
    silent_reserve: Vec<Box<[u8]>>,
    space_waiting: bool,
    closed: Option<CloseMode>,
    paused_idle: bool,
}

/// The queue of one `OpusPacketStream`.
#[derive(Debug)]
pub struct OpusQueue {
    id: u32,
    /// L7.
    inner: Mutex<OpusInner>,
    max_packets: usize,
    token: WakeToken,
    send_muted: Arc<AtomicBool>,
    queued: AtomicU64,
    pulls: AtomicU64,
    reader_gone: AtomicBool,
}

impl OpusQueue {
    /// Starts paused (`paused_idle`); the first `send` resumes the track. Its `LiveSource::id` is
    /// drawn from `tx::next_source_id()` (the contract has no id parameter).
    pub fn new(
        spec: &OpusStreamSpec,
        token: WakeToken,
        send_muted: Arc<AtomicBool>,
    ) -> Result<Arc<Self>, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Validates and queues one packet (`PlaybackFull` when full; nothing consumed).
    pub fn send(&self, packet: &[u8]) -> Result<(), CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Arms a space wake-up; `true` ⇒ there is already space.
    pub fn arm_space(&self) -> bool {
        false
    }

    /// Discards queued packets; returns the milliseconds discarded.
    pub fn clear(&self) -> u64 {
        0
    }

    pub fn buffered_ms(&self) -> u64 {
        self.queued.load(Ordering::Acquire).saturating_mul(20)
    }

    pub fn wake_id(&self) -> WakeId {
        self.token.id()
    }
}

impl LiveSource for OpusQueue {
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
        let mut g = self.inner.lock();
        if g.closed.is_none() {
            g.closed = Some(mode);
        }
    }
}

/// `Input::Live(LiveInput::Parsed(..))` over an `OpusQueue` (`CODEC_TYPE_OPUS`, 48 kHz).
pub fn opus_stream_input(
    q: Arc<OpusQueue>,
    done: DoneGuard,
) -> Result<songbird::input::Input, CoreError> {
    Err(CoreError::Internal("unimplemented"))
}
