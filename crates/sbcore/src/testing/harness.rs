//! Detached connections (§13.2): a full `ConnShared` with **no songbird Driver** and a
//! `ManualClock`; `Harness::step` runs `ConnWorker::step` synchronously, so tests are deterministic.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::num::NonZeroU64;
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;

use crate::clock::ManualClock;
use crate::conn::gateway::{DiscKind, DiscReason};
use crate::conn::settings::VoiceSettings;
use crate::conn::shared::ConnShared;
use crate::conn::worker::ConnWorker;
use crate::error::CoreError;
use crate::wake::Doorbell;

/// The worker and clock of a detached connection.
pub struct Harness {
    conn: Weak<ConnShared>,
    worker: Mutex<Option<ConnWorker>>,
    clock: Arc<ManualClock>,
}

impl Harness {
    /// Builds a detached connection. `bell` is the doorbell to adopt; `None` creates an internal
    /// one whose bytes are drained by `step` (tests then poll `take_ready` directly).
    pub fn detached(
        guild_id: NonZeroU64,
        user_id: NonZeroU64,
        settings: VoiceSettings,
        bell: Option<Doorbell>,
    ) -> Result<(Arc<ConnShared>, Harness), CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Advances the manual clock by `advance_ms` and runs one worker iteration.
    pub fn step(&self, advance_ms: u64) -> Result<(), CoreError> {
        self.clock.advance(Duration::from_millis(advance_ms));
        Err(CoreError::Internal("unimplemented"))
    }

    /// The manual clock.
    pub fn clock(&self) -> &Arc<ManualClock> {
        &self.clock
    }
}

/// A songbird driver event to inject through `apply()` (and `RxInput::Link*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverEvent {
    Connect {
        ssrc: u32,
    },
    Reconnect {
        ssrc: u32,
    },
    Disconnect {
        kind: DiscKind,
        reason: Option<DiscReason>,
    },
}

/// One tick: `(ssrc, Some(opus payload) | None = lost)` voices plus silent SSRCs.
pub fn inject_tick(
    conn: &ConnShared,
    voices: Vec<(u32, Option<Vec<u8>>)>,
    silent: Vec<u32>,
) -> Result<(), CoreError> {
    Err(CoreError::Internal("unimplemented"))
}

/// `SpeakingStateUpdate`: maps `ssrc` to `user_id`.
pub fn inject_map(conn: &ConnShared, ssrc: u32, user_id: u64) -> Result<(), CoreError> {
    Err(CoreError::Internal("unimplemented"))
}

/// `ClientDisconnect`.
pub fn inject_user_left(conn: &ConnShared, user_id: u64) -> Result<(), CoreError> {
    Err(CoreError::Internal("unimplemented"))
}

/// `DriverConnect` / `DriverReconnect` / `DriverDisconnect`.
pub fn inject_driver_event(conn: &ConnShared, ev: DriverEvent) -> Result<(), CoreError> {
    Err(CoreError::Internal("unimplemented"))
}

/// One tick of full RTP packets `(ssrc, packet)` (exercises `extract_opus` end to end).
pub fn inject_voice_tick_rtp(
    conn: &ConnShared,
    packets: Vec<(u32, Vec<u8>)>,
) -> Result<(), CoreError> {
    Err(CoreError::Internal("unimplemented"))
}
