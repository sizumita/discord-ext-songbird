//! Detached connections (§13.2): a full `ConnShared` with **no songbird Driver** and a
//! `ManualClock`; `Harness::step` runs `ConnWorker::step` synchronously, so tests are deterministic.
//!
//! Injection (`inject_*`) feeds the **same** tick channel and `apply()` path that `Dispatch` uses;
//! only songbird's UDP/WS layer is bypassed.

use std::num::NonZeroU64;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::Mutex;
use smallvec::SmallVec;
use tokio::sync::mpsc::error::TrySendError;

use super::registry::{self, ConnKey};
use super::rtp::{self, RtpOpts};
use crate::clock::{Clock, ManualClock};
use crate::conn::gateway::{ConnectFailure, DiscKind, DiscReason, GwInput};
use crate::conn::settings::{ConnParams, VoiceSettings};
use crate::conn::shared::ConnShared;
use crate::conn::worker::ConnWorker;
use crate::error::CoreError;
use crate::limits::{self, LimitError, LimitNum, TICK_CHANNEL_CAP, TICK_MS};
use crate::runtime::{Role, role, set_role};
use crate::rx::raw::{RawTick, RawVoice, RxInput};
use crate::wake::Doorbell;

/// Longest single `Harness::step` advance (1 h of manual time).
pub const MAX_STEP_MS: u64 = 3_600_000;
/// Most voices or silent SSRCs in one injected tick.
pub const MAX_TICK_VOICES: usize = 256;

/// The read end of a harness-owned doorbell (`bell=None`), drained by `step`.
#[cfg(unix)]
type BellReader = std::os::unix::net::UnixStream;
#[cfg(not(unix))]
type BellReader = std::net::TcpStream;

/// The worker and clock of a detached connection.
pub struct Harness {
    conn: Weak<ConnShared>,
    key: ConnKey,
    worker: Mutex<Option<ConnWorker>>,
    clock: Arc<ManualClock>,
    bell_rx: Option<BellReader>,
}

impl Harness {
    /// Builds a detached connection. `bell` is the doorbell to adopt; `None` creates an internal
    /// one whose bytes are drained by `step` (tests then poll `take_ready` directly). `Connect`
    /// actions wait for an injected outcome (`DriverEvent::Connect` / `ConnectFailed`).
    pub fn detached(
        guild_id: NonZeroU64,
        user_id: NonZeroU64,
        settings: VoiceSettings,
        bell: Option<Doorbell>,
        reconnect: bool,
    ) -> Result<(Arc<ConnShared>, Harness), CoreError> {
        let (bell, bell_rx) = match bell {
            Some(b) => (b, None),
            None => {
                let (b, rx) =
                    internal_bell().map_err(|_| CoreError::Internal("harness doorbell"))?;
                (b, Some(rx))
            }
        };
        let (clock, manual) = Clock::manual();
        let params = ConnParams {
            guild_id,
            user_id,
            reconnect,
            settings,
            clock,
        };
        let (conn, worker) = ConnShared::new_detached(params, bell)?;
        // No Driver runs the connect future: the test supplies its outcome
        // (`inject_driver_event(Connect | ConnectFailed)`), so the state stays `connecting` until then.
        conn.set_detached_auto_connect(false);
        let key = registry::key_of(&conn);
        registry::register_conn(key, &manual);
        let harness = Harness {
            conn: Arc::downgrade(&conn),
            key,
            worker: Mutex::new(Some(worker)),
            clock: manual,
            bell_rx,
        };
        Ok((conn, harness))
    }

    /// Runs one worker iteration at the current manual time (processing whatever was injected),
    /// then advances the clock by `advance_ms` in steps of at most one tick (20 ms, or earlier if
    /// the worker asked to be woken sooner), running one iteration after each step.
    ///
    /// Afterwards the inputs of ended tracks are disposed of, as songbird would (all of them once
    /// the connection is closed), and an internal doorbell is drained.
    pub fn step(&self, advance_ms: u64) -> Result<(), CoreError> {
        limits::check("advance_ms", advance_ms, 0..=MAX_STEP_MS)?;
        let prev_role = role();
        set_role(Role::Rx);
        let result = self.run(Duration::from_millis(advance_ms));
        set_role(prev_role);
        let closed = self.conn.upgrade().is_none_or(|c| c.is_closed());
        registry::sweep(self.key, closed);
        self.drain_bell();
        result
    }

    fn run(&self, advance: Duration) -> Result<(), CoreError> {
        let mut guard = self.worker.lock();
        let Some(worker) = guard.as_mut() else {
            return Ok(());
        };
        let target = self.clock.elapsed().saturating_add(advance);
        loop {
            let now = self.clock.now();
            let next = match catch_unwind(AssertUnwindSafe(|| worker.step(now))) {
                Ok(next) => next,
                Err(_) => {
                    // The worker is unusable after a panic; drop it (its exit path is cleanup only).
                    *guard = None;
                    return Err(CoreError::Internal(
                        "testing::Harness::step (worker panicked)",
                    ));
                }
            };
            let elapsed = self.clock.elapsed();
            if elapsed >= target {
                return Ok(());
            }
            let until = next.and_then(|at| at.checked_duration_since(now));
            self.clock
                .advance(next_increment(target.saturating_sub(elapsed), until));
        }
    }

    fn drain_bell(&self) {
        let Some(rx) = &self.bell_rx else {
            return;
        };
        let mut buf = [0u8; 256];
        loop {
            match std::io::Read::read(&mut &*rx, &mut buf) {
                Ok(n) if n > 0 => continue,
                _ => return,
            }
        }
    }

    /// The manual clock.
    pub fn clock(&self) -> &Arc<ManualClock> {
        &self.clock
    }

    /// Manual time elapsed since the connection was created.
    pub fn elapsed(&self) -> Duration {
        self.clock.elapsed()
    }

    /// Songbird inputs currently parked for this connection (live sources not yet taken by a
    /// `MixerHarness`, and memory/file tracks).
    pub fn parked(&self) -> usize {
        registry::parked_count(self.key)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // The worker drops first (best-effort cleanup), then every parked input (their
        // `DoneGuard`s fire, as songbird's `SetTrack(None)` would), then the injection state.
        drop(self.worker.lock().take());
        registry::sweep(self.key, true);
        registry::unregister_conn(self.key);
    }
}

/// The next manual-clock increment of [`Harness::step`]: at most one tick, earlier when the worker
/// asked to be woken sooner (`until`, but at least 1 ms), and **never past `remaining`**, so a step
/// of `n` ms advances the clock by exactly `n` ms.
fn next_increment(remaining: Duration, until: Option<Duration>) -> Duration {
    let mut inc = Duration::from_millis(u64::from(TICK_MS)).min(remaining);
    if let Some(u) = until
        && !u.is_zero()
    {
        inc = inc.min(u).max(Duration::from_millis(1));
    }
    inc.min(remaining)
}

#[cfg(unix)]
fn internal_bell() -> std::io::Result<(Doorbell, BellReader)> {
    let (rx, tx) = std::os::unix::net::UnixStream::pair()?;
    rx.set_nonblocking(true)?;
    Ok((Doorbell::from_unix_stream(tx)?, rx))
}

#[cfg(windows)]
fn internal_bell() -> std::io::Result<(Doorbell, BellReader)> {
    use std::os::windows::io::IntoRawSocket;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let tx = std::net::TcpStream::connect(listener.local_addr()?)?;
    let (rx, _) = listener.accept()?;
    rx.set_nonblocking(true)?;
    let raw = i64::try_from(tx.into_raw_socket())
        .map_err(|_| std::io::Error::other("socket handle out of range"))?;
    // SAFETY: `raw` is a connected socket we own; `adopt` takes ownership exactly once.
    let bell = unsafe { Doorbell::adopt(raw)? };
    Ok((bell, rx))
}

#[cfg(not(any(unix, windows)))]
fn internal_bell() -> std::io::Result<(Doorbell, BellReader)> {
    Err(std::io::Error::other("no doorbell on this platform"))
}

/// A songbird driver event to inject through `apply()` (and `RxInput::Link*`), plus the outcome
/// of the connect future a real Driver would report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverEvent {
    /// A successful connect: `DriverConnect{ssrc}`, then — if the state is `connecting` — the
    /// connect future's `ConnectResult{generation, Ok}`, then `RxInput::LinkUp`.
    Connect { ssrc: u32 },
    /// A failed connect future: `ConnectResult{generation, Err(TimedOut | Other)}`.
    ConnectFailed { timed_out: bool },
    /// `DriverReconnect{ssrc}`, then `RxInput::LinkUp`.
    Reconnect { ssrc: u32 },
    /// `DriverDisconnect{kind, reason}`, then `RxInput::LinkDown`.
    Disconnect {
        kind: DiscKind,
        reason: Option<DiscReason>,
    },
}

/// Sends one input on the connection's tick channel, exactly where `Dispatch` sends it. A full
/// channel raises `LimitExceeded("tick_channel")` (call `Harness::step` to drain it) instead of
/// silently dropping like `Dispatch`; a closed channel (connection closed, or receive disabled)
/// drops the input like `Dispatch` does.
fn send_rx(conn: &ConnShared, input: RxInput) -> Result<(), CoreError> {
    let Some(tx) = conn.rx_sender_ref() else {
        return Ok(());
    };
    match tx.try_send(input) {
        Ok(()) | Err(TrySendError::Closed(_)) => Ok(()),
        Err(TrySendError::Full(_)) => Err(CoreError::Limit(LimitError {
            what: "tick_channel",
            value: LimitNum::Int(TICK_CHANNEL_CAP as i128 + 1),
            min: LimitNum::Int(0),
            max: LimitNum::Int(TICK_CHANNEL_CAP as i128),
        })),
    }
}

fn tick_from(conn: &ConnShared, voices: SmallVec<[RawVoice; 8]>, silent: Vec<u32>) -> RxInput {
    RxInput::Tick(RawTick {
        arrived: now(conn),
        voices,
        silent: silent.into_iter().collect(),
    })
}

fn now(conn: &ConnShared) -> Instant {
    registry::now_for(registry::key_of(conn))
}

/// One tick: `(ssrc, Some(opus payload) | None = lost)` voices plus silent SSRCs. Each payload is
/// wrapped by [`rtp::rtp_wrap`] with consecutive per-SSRC sequence numbers (a lost voice consumes
/// one), so the pipeline sees exactly what songbird's Decrypt mode delivers.
pub fn inject_tick(
    conn: &ConnShared,
    voices: Vec<(u32, Option<Vec<u8>>)>,
    silent: Vec<u32>,
) -> Result<(), CoreError> {
    limits::check("tick_voices", voices.len(), 0..=MAX_TICK_VOICES)?;
    limits::check("tick_silent", silent.len(), 0..=MAX_TICK_VOICES)?;
    let key = registry::key_of(conn);
    let mut raw: SmallVec<[RawVoice; 8]> = SmallVec::new();
    for (ssrc, payload) in voices {
        let seq = registry::next_seq(key, ssrc);
        match payload {
            Some(opus) => {
                let pkt = rtp::rtp_wrap(&opus, seq, ssrc, RtpOpts::default())?;
                let (payload_offset, payload_end) = rtp::payload_bounds(&pkt);
                raw.push(RawVoice::Packet {
                    ssrc,
                    packet: Bytes::from(pkt),
                    payload_offset,
                    payload_end,
                });
            }
            None => raw.push(RawVoice::Lost { ssrc }),
        }
    }
    send_rx(conn, tick_from(conn, raw, silent))
}

/// `SpeakingStateUpdate`: maps `ssrc` to `user_id`.
pub fn inject_map(conn: &ConnShared, ssrc: u32, user_id: u64) -> Result<(), CoreError> {
    send_rx(conn, RxInput::Map { ssrc, user_id })
}

/// `ClientDisconnect`.
pub fn inject_user_left(conn: &ConnShared, user_id: u64) -> Result<(), CoreError> {
    send_rx(conn, RxInput::UserLeft { user_id })
}

/// `DriverConnect` / `DriverReconnect` / `DriverDisconnect` (and the connect future's result).
///
/// Driver events take the `Dispatch` handler's own path (`apply()`, then `RxInput::LinkUp` /
/// `LinkDown` on the tick channel, overflowing into the control queue like `Dispatch` does).
pub fn inject_driver_event(conn: &ConnShared, ev: DriverEvent) -> Result<(), CoreError> {
    match ev {
        DriverEvent::Connect { ssrc } => {
            conn.inject_driver_connect(ssrc, false);
            let st = conn.status();
            if st.state == "connecting" {
                conn.apply(GwInput::ConnectResult {
                    generation: st.generation,
                    result: Ok(()),
                });
            }
            Ok(())
        }
        DriverEvent::ConnectFailed { timed_out } => {
            let st = conn.status();
            conn.apply(GwInput::ConnectResult {
                generation: st.generation,
                result: Err(if timed_out {
                    ConnectFailure::TimedOut
                } else {
                    ConnectFailure::Other("injected connect failure".into())
                }),
            });
            Ok(())
        }
        DriverEvent::Reconnect { ssrc } => {
            conn.inject_driver_connect(ssrc, true);
            Ok(())
        }
        DriverEvent::Disconnect { kind, reason } => {
            conn.inject_driver_disconnect(kind, reason);
            Ok(())
        }
    }
}

/// One tick of full RTP packets `(ssrc, packet)` as [`rtp::rtp_wrap`] builds them (20-byte crypto
/// suffix; exercises `extract_opus` end to end). A packet that does not parse as RTP is still
/// delivered (with an empty payload range), so the pipeline counts it as a bad packet.
pub fn inject_voice_tick_rtp(
    conn: &ConnShared,
    packets: Vec<(u32, Vec<u8>)>,
) -> Result<(), CoreError> {
    limits::check("tick_voices", packets.len(), 0..=MAX_TICK_VOICES)?;
    let mut raw: SmallVec<[RawVoice; 8]> = SmallVec::new();
    for (ssrc, pkt) in packets {
        limits::check("rtp_packet_bytes", pkt.len(), 0..=rtp::MAX_RTP_PACKET)?;
        let (payload_offset, payload_end) = rtp::payload_bounds(&pkt);
        raw.push(RawVoice::Packet {
            ssrc,
            packet: Bytes::from(pkt),
            payload_offset,
            payload_end,
        });
    }
    send_rx(conn, tick_from(conn, raw, Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::settings::RawVoiceSettings;

    fn settings() -> VoiceSettings {
        VoiceSettings::validate(RawVoiceSettings::default()).unwrap()
    }

    #[test]
    fn detached_follows_the_connshared_contract() {
        // Serialised with the tests that assert the process-wide connection counters.
        let _serial = crate::conn::TEST_LOCK.lock();
        let id = NonZeroU64::new(1).unwrap();
        let (conn, h) = Harness::detached(id, id, settings(), None, true).unwrap();
        assert!(conn.is_detached());
        assert_eq!(conn.status().state, "idle");
        h.step(0).unwrap();
        h.step(100).unwrap();
        assert_eq!(h.elapsed(), Duration::from_millis(100));
        assert_eq!(h.parked(), 0);
        conn.close(crate::error::CloseReason::Requested);
        h.step(20).unwrap();
    }

    #[test]
    fn step_increments_never_overshoot() {
        let ms = Duration::from_millis;
        let us = Duration::from_micros;
        assert_eq!(next_increment(ms(100), None), ms(20));
        assert_eq!(next_increment(ms(5), None), ms(5));
        assert_eq!(next_increment(ms(100), Some(ms(3))), ms(3));
        assert_eq!(next_increment(ms(100), Some(Duration::ZERO)), ms(20));
        // A sub-millisecond wake-up is rounded up to 1 ms …
        assert_eq!(next_increment(ms(100), Some(us(200))), ms(1));
        // … but never past the end of the step (it used to overshoot by up to 1 ms).
        assert_eq!(next_increment(us(500), Some(us(200))), us(500));
        // Summing increments reaches the target exactly, whatever the wake-ups.
        let target = ms(100);
        let mut elapsed = Duration::ZERO;
        let wakes = [us(1_500), us(300), us(700), us(19_900)];
        let mut i = 0;
        while elapsed < target {
            elapsed += next_increment(target - elapsed, Some(wakes[i % wakes.len()]));
            i += 1;
        }
        assert_eq!(elapsed, target);
    }

    #[test]
    fn step_is_bounded() {
        assert!(limits::check("advance_ms", MAX_STEP_MS + 1, 0..=MAX_STEP_MS).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn internal_bell_rings_and_drains() {
        let (bell, rx) = internal_bell().unwrap();
        bell.ring();
        let mut buf = [0u8; 8];
        let n = std::io::Read::read(&mut &rx, &mut buf).unwrap();
        assert_eq!(n, 1);
        assert!(std::io::Read::read(&mut &rx, &mut buf).is_err()); // WouldBlock
    }
}
