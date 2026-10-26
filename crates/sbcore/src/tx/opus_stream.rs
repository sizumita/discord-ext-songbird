//! Push-based Opus packet streams (W4, §7.9): `OpusQueue` (L7) + `OpusStreamReader` + the registry
//! Opus decoder. The mixer `try_lock`s, moves a packet's `Box<[u8]>` into the `Packet`, and falls
//! back to a pre-allocated `SILENT_FRAME` on underrun/contention/self-mute: from a 64-entry reserve
//! (refilled by the housekeeper) via a small reader-local stash, so contention never allocates.
//!
//! The stream starts paused; the first `send` resumes it, and the housekeeper pauses it again after
//! `idle_ms` without packets, so an unfed stream sends no RTP.

use std::collections::VecDeque;
use std::io::{Cursor, ErrorKind};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use songbird::driver::opus;
use songbird::input::codecs::get_codec_registry;
use songbird::input::core::audio::Channels;
use songbird::input::core::codecs::{CODEC_TYPE_OPUS, CodecParameters, DecoderOptions};
use songbird::input::core::errors::{Error as SymphError, Result as SymphResult};
use songbird::input::core::formats::{
    Cue, FormatOptions, FormatReader, Packet, SeekMode, SeekTo, SeekedTo, Track,
};
use songbird::input::core::io::MediaSourceStream;
use songbird::input::core::meta::{Metadata, MetadataLog};
use songbird::input::core::units::TimeBase;
use songbird::input::{Input, LiveInput, Parsed};

use super::probe::probe_raw_header;
use super::{CloseMode, HouseStatus, LiveSource, TrackControl, enter_mixer, next_source_id};
use crate::error::{CloseReason, CoreError};
use crate::events::EventHub;
use crate::limits::{
    FRAMES_PER_TICK_48K, MIX_RATE, OPUS_SILENT_RESERVE, OPUS_STREAM_PACKETS, TICK_MS, VOLUME, check,
};
use crate::runtime::{LiveCounter, live};
use crate::rx::opus::SILENT_FRAME;
use crate::sync::lock_checked;
use crate::tracks::flags::{DECODE_ERROR, DoneGuard, EOF_SEEN, PANICKED};
use crate::wake::{WakeId, WakeToken};

/// Maximum Opus packet size accepted by `send` (bytes).
pub const MAX_OPUS_PACKET: usize = 1_275;

/// The `InvalidAudio` message for a packet outside `1..=MAX_OPUS_PACKET` bytes (the binding raises
/// the same error before copying an oversized buffer).
pub const OPUS_PACKET_SIZE_MSG: &str = "an Opus packet must be 1..=1275 bytes long";

/// Without packets for this long, the track pauses (no RTP).
pub const OPUS_IDLE_MS: u32 = 250;

/// Silent frames the reader keeps for ticks where it cannot take L7.
const READER_STASH: usize = 4;

const END_DRAIN: u8 = 1 << 0;
const END_ABORT: u8 = 1 << 1;
const END_CONN_CLOSED: u8 = 1 << 2;

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
        return Err(CoreError::invalid(OPUS_PACKET_SIZE_MSG));
    }
    match opus::packet::get_nb_samples(p, 48_000) {
        Ok(960) => Ok(()),
        _ => Err(CoreError::invalid(
            "an Opus packet must hold exactly 20 ms (960 samples at 48 kHz)",
        )),
    }
}

/// Codec parameters of an Opus input (48 kHz stereo, 20 ms packets).
pub(crate) fn opus_codec_params() -> CodecParameters {
    let mut p = CodecParameters::new();
    p.for_codec(CODEC_TYPE_OPUS)
        .with_sample_rate(MIX_RATE)
        .with_time_base(TimeBase::new(1, MIX_RATE))
        .with_channels(Channels::FRONT_LEFT | Channels::FRONT_RIGHT)
        .with_max_frames_per_packet(FRAMES_PER_TICK_48K as u64);
    p
}

/// A fresh `SILENT_FRAME` box (allocates).
pub(crate) fn silent_box() -> Box<[u8]> {
    Box::from(&SILENT_FRAME[..])
}

/// State under L7.
#[derive(Debug, Default)]
struct OpusInner {
    packets: VecDeque<Box<[u8]>>,
    /// Pre-allocated `SILENT_FRAME` boxes (capacity `OPUS_SILENT_RESERVE`, never grown).
    silent_reserve: Vec<Box<[u8]>>,
    space_waiting: bool,
    closed: Option<CloseMode>,
    paused_idle: bool,
    track_playing: bool,
    stop_sent: bool,
    active: bool,
}

/// Housekeeper-only state.
#[derive(Clone, Copy, Debug)]
struct OpusHouse {
    seen_sent: u64,
    seen_real: u64,
    last_activity: Instant,
    end_reported: bool,
}

/// The queue of one `OpusPacketStream`.
pub struct OpusQueue {
    id: u32,
    /// L7.
    inner: Mutex<OpusInner>,
    max_packets: usize,
    volume: f32,
    token: WakeToken,
    send_muted: Arc<AtomicBool>,
    queued: AtomicU64,
    /// Packets accepted by `send`.
    sent: AtomicU64,
    /// Ticks pulled by the mixer, and those that carried a real packet.
    pulls: AtomicU64,
    real_pulls: AtomicU64,
    contended: AtomicU64,
    reader_gone: AtomicBool,
    end_flags: AtomicU8,
    house: Mutex<Option<OpusHouse>>,
    track: OnceLock<Box<dyn TrackControl>>,
}

impl std::fmt::Debug for OpusQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusQueue")
            .field("id", &self.id)
            .field("max_packets", &self.max_packets)
            .finish_non_exhaustive()
    }
}

impl OpusQueue {
    /// Starts paused (`paused_idle`); the first `send` resumes the track. Its `LiveSource::id` is
    /// drawn from `tx::next_source_id()` (the contract has no id parameter).
    pub fn new(
        spec: &OpusStreamSpec,
        token: WakeToken,
        send_muted: Arc<AtomicBool>,
    ) -> Result<Arc<Self>, CoreError> {
        let max_packets = check("max_packets", spec.max_packets, OPUS_STREAM_PACKETS)?;
        let volume = check("volume", spec.volume, VOLUME)?;
        let mut reserve = Vec::with_capacity(OPUS_SILENT_RESERVE);
        reserve.extend((0..OPUS_SILENT_RESERVE).map(|_| silent_box()));
        Ok(Arc::new(OpusQueue {
            id: next_source_id(),
            inner: Mutex::new(OpusInner {
                packets: VecDeque::with_capacity(max_packets.min(16)),
                silent_reserve: reserve,
                paused_idle: true,
                ..OpusInner::default()
            }),
            max_packets,
            volume,
            token,
            send_muted,
            queued: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            pulls: AtomicU64::new(0),
            real_pulls: AtomicU64::new(0),
            contended: AtomicU64::new(0),
            reader_gone: AtomicBool::new(false),
            end_flags: AtomicU8::new(0),
            house: Mutex::new(None),
            track: OnceLock::new(),
        }))
    }

    fn check_open(&self) -> Result<(), CoreError> {
        let flags = self.end_flags.load(Ordering::Acquire);
        if flags == 0 {
            Ok(())
        } else if flags & END_CONN_CLOSED != 0 {
            Err(CoreError::StreamClosed(CloseReason::ConnectionClosed))
        } else {
            Err(CoreError::StreamClosed(CloseReason::Closed))
        }
    }

    fn sync_track(&self, g: &mut OpusInner) {
        let desired = !g.paused_idle && g.closed != Some(CloseMode::Abort);
        if desired == g.track_playing {
            return;
        }
        if let Some(t) = self.track.get() {
            if desired {
                t.play();
            } else {
                t.pause();
            }
            g.track_playing = desired;
        }
    }

    /// Validates and queues one packet (`PlaybackFull` when full; nothing consumed).
    pub fn send(&self, packet: &[u8]) -> Result<(), CoreError> {
        self.check_open()?;
        validate_opus_packet(packet)?;
        let mut data: Vec<u8> = Vec::new();
        data.try_reserve_exact(packet.len())?;
        data.extend_from_slice(packet);
        let data = data.into_boxed_slice();
        let mut g = lock_checked(&self.inner);
        self.check_open()?;
        if g.packets.len() >= self.max_packets {
            g.space_waiting = true;
            return Err(CoreError::PlaybackFull {
                needed_ms: u64::from(TICK_MS),
                free_ms: 0,
            });
        }
        g.packets.push_back(data);
        self.queued.store(g.packets.len() as u64, Ordering::Release);
        self.sent.fetch_add(1, Ordering::Release);
        g.active = true;
        if g.paused_idle {
            g.paused_idle = false;
            self.sync_track(&mut g);
        }
        Ok(())
    }

    /// Arms a space wake-up; `true` ⇒ there is already space (or the stream is closed).
    pub fn arm_space(&self) -> bool {
        if self.end_flags.load(Ordering::Acquire) != 0 {
            return true;
        }
        let mut g = lock_checked(&self.inner);
        if g.packets.len() < self.max_packets {
            true
        } else {
            g.space_waiting = true;
            false
        }
    }

    /// Discards queued packets; returns the milliseconds discarded.
    pub fn clear(&self) -> u64 {
        let (old, ring) = {
            let mut g = lock_checked(&self.inner);
            let old = std::mem::take(&mut g.packets);
            self.queued.store(0, Ordering::Release);
            (old, std::mem::take(&mut g.space_waiting))
        };
        let n = old.len() as u64;
        drop(old);
        if ring {
            self.token.notify();
        }
        n.saturating_mul(u64::from(TICK_MS))
    }

    pub fn buffered_ms(&self) -> u64 {
        self.queued
            .load(Ordering::Acquire)
            .saturating_mul(u64::from(TICK_MS))
    }

    pub fn wake_id(&self) -> WakeId {
        self.token.id()
    }

    /// The track volume this stream was opened with (songbird applies it).
    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn is_closed(&self) -> bool {
        self.end_flags.load(Ordering::Acquire) != 0
    }

    /// The owner (`native.OpusSink`) dropped its handle: abort, unless a close was already
    /// requested (a `close(drain=True)` in progress completes).
    pub fn abandon(&self) {
        if !self.is_closed() {
            LiveSource::close(self, CloseMode::Abort);
        }
    }

    /// Whether the reader holding this queue was dropped.
    pub fn reader_gone(&self) -> bool {
        self.reader_gone.load(Ordering::Acquire)
    }

    /// Attaches songbird's track (or a `FakeTrackControl`), once (`ConnShared::open_opus_stream`).
    pub(crate) fn attach_track(&self, t: Box<dyn TrackControl>) {
        if self.track.set(t).is_err() {
            return;
        }
        let stop = {
            let mut g = lock_checked(&self.inner);
            self.sync_track(&mut g);
            // Same rule as `close()`: a drain-closed track that is not playing can never drain.
            let stop = match g.closed {
                Some(CloseMode::Abort) => true,
                Some(CloseMode::Drain) => !g.track_playing,
                None => false,
            } && !g.stop_sent;
            g.stop_sent |= stop;
            stop
        };
        if stop && let Some(t) = self.track.get() {
            t.stop();
        }
    }

    /// Atomics only (mixer).
    fn should_end(&self) -> bool {
        let flags = self.end_flags.load(Ordering::Acquire);
        flags & (END_ABORT | END_CONN_CLOSED) != 0
            || (flags & END_DRAIN != 0 && self.queued.load(Ordering::Acquire) == 0)
    }

    /// Takes the next packet for the mixer (`try_lock` only; `None` on underrun, contention or
    /// self-mute). Tops up the reader's stash of silent frames while it holds the lock.
    fn take_packet(&self, stash: &mut Vec<Box<[u8]>>) -> Option<Box<[u8]>> {
        let muted = self.send_muted.load(Ordering::Relaxed);
        let Some(mut g) = self.inner.try_lock() else {
            self.contended.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        while stash.len() < stash.capacity() {
            match g.silent_reserve.pop() {
                Some(b) => stash.push(b),
                None => break,
            }
        }
        if muted {
            return None;
        }
        let p = g.packets.pop_front();
        if p.is_some() {
            self.queued.store(g.packets.len() as u64, Ordering::Release);
            self.real_pulls.fetch_add(1, Ordering::Release);
        }
        p
    }

    /// One housekeeping pass (RX): reserve refill, space wake-ups, idle pause.
    /// The housekeeper-only state is copied out of its mutex, so that mutex is never held while
    /// L7 is taken (no new lock nesting, §5.3).
    pub(crate) fn housekeep_at(&self, now: Instant) -> HouseStatus {
        let mut h = {
            let mut house = lock_checked(&self.house);
            *house.get_or_insert_with(|| OpusHouse {
                seen_sent: 0,
                seen_real: 0,
                last_activity: now,
                end_reported: false,
            })
        };
        let status = self.housekeep_with(now, &mut h);
        *lock_checked(&self.house) = Some(h);
        status
    }

    fn housekeep_with(&self, now: Instant, h: &mut OpusHouse) -> HouseStatus {
        if self.reader_gone.load(Ordering::Acquire) {
            if !h.end_reported {
                h.end_reported = true;
                self.end_flags.fetch_or(END_ABORT, Ordering::AcqRel);
                let old = {
                    let mut g = lock_checked(&self.inner);
                    self.queued.store(0, Ordering::Release);
                    std::mem::take(&mut g.packets)
                };
                drop(old);
                self.token.notify();
            }
            return HouseStatus::Finished;
        }
        let sent = self.sent.load(Ordering::Acquire);
        let real = self.real_pulls.load(Ordering::Acquire);
        if sent != h.seen_sent || real != h.seen_real {
            h.seen_sent = sent;
            h.seen_real = real;
            h.last_activity = now;
        }
        let mut ring = false;
        let deficit = {
            let mut g = lock_checked(&self.inner);
            if g.space_waiting && g.packets.len() < self.max_packets {
                g.space_waiting = false;
                ring = true;
            }
            if g.active
                && g.packets.is_empty()
                && g.closed.is_none()
                && now.saturating_duration_since(h.last_activity)
                    >= Duration::from_millis(u64::from(OPUS_IDLE_MS))
            {
                g.active = false;
                g.paused_idle = true;
                self.sync_track(&mut g);
            }
            OPUS_SILENT_RESERVE.saturating_sub(g.silent_reserve.len())
        };
        if deficit > 0 {
            // Allocate outside L7, then top the reserve up (its capacity is never exceeded).
            let mut fresh: Vec<Box<[u8]>> = (0..deficit).map(|_| silent_box()).collect();
            let mut g = lock_checked(&self.inner);
            while g.silent_reserve.len() < g.silent_reserve.capacity() {
                match fresh.pop() {
                    Some(b) => g.silent_reserve.push(b),
                    None => break,
                }
            }
        }
        if ring {
            self.token.notify();
        }
        HouseStatus::Live
    }
}

impl LiveSource for OpusQueue {
    fn id(&self) -> u32 {
        self.id
    }

    fn housekeep(&self, now: Instant, _events: &EventHub) -> HouseStatus {
        self.housekeep_at(now)
    }

    fn on_link(&self, _up: bool) {
        // Opus streams keep their packets across link changes.
    }

    fn close(&self, mode: CloseMode) {
        let bit = match mode {
            CloseMode::Drain => END_DRAIN,
            CloseMode::Abort => END_ABORT,
        };
        self.end_flags.fetch_or(bit, Ordering::AcqRel);
        let (old, stop, ring) = {
            let mut g = lock_checked(&self.inner);
            if g.closed == Some(CloseMode::Abort) {
                return;
            }
            g.closed = Some(mode);
            let old = if mode == CloseMode::Abort {
                self.queued.store(0, Ordering::Release);
                std::mem::take(&mut g.packets)
            } else {
                VecDeque::new()
            };
            // Without an attached track the stop is left to `attach_track` (it would be lost).
            let stop = (mode == CloseMode::Abort || !g.track_playing)
                && !g.stop_sent
                && self.track.get().is_some();
            g.stop_sent |= stop;
            (old, stop, std::mem::take(&mut g.space_waiting))
        };
        drop(old);
        if stop && let Some(t) = self.track.get() {
            t.stop();
        }
        if ring {
            self.token.notify();
        }
    }

    fn conn_closed(&self) {
        self.end_flags
            .fetch_or(END_CONN_CLOSED | END_ABORT, Ordering::AcqRel);
        LiveSource::close(self, CloseMode::Abort);
    }
}

/// The reader of an Opus stream: one packet (or a silent frame) per tick.
pub(crate) struct OpusStreamReader {
    q: Arc<OpusQueue>,
    track: Track,
    ts: u64,
    done: DoneGuard,
    mss: Option<MediaSourceStream>,
    meta: MetadataLog,
    stash: Vec<Box<[u8]>>,
    poisoned: bool,
    _input: LiveCounter,
}

impl OpusStreamReader {
    fn next_inner(&mut self) -> SymphResult<Packet> {
        if self.q.should_end() {
            self.done.flags().hint(EOF_SEEN);
            return Err(SymphError::IoError(ErrorKind::UnexpectedEof.into()));
        }
        let data = match self.q.take_packet(&mut self.stash) {
            Some(p) => p,
            // Last resort (empty reserve and stash): one 3-byte allocation.
            None => self.stash.pop().unwrap_or_else(silent_box),
        };
        self.q.pulls.fetch_add(1, Ordering::Relaxed);
        let ts = self.ts;
        self.ts = self.ts.wrapping_add(FRAMES_PER_TICK_48K as u64);
        Ok(Packet::new_from_boxed_slice(
            0,
            ts,
            FRAMES_PER_TICK_48K as u64,
            data,
        ))
    }
}

impl FormatReader for OpusStreamReader {
    fn try_new(_source: MediaSourceStream, _options: &FormatOptions) -> SymphResult<Self>
    where
        Self: Sized,
    {
        Err(SymphError::Unsupported(
            "OpusStreamReader is constructed by sbcore, not probed",
        ))
    }

    fn cues(&self) -> &[Cue] {
        &[]
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.meta.metadata()
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> SymphResult<SeekedTo> {
        let required_ts = match to {
            SeekTo::TimeStamp { ts, .. } => ts,
            SeekTo::Time { .. } => self.ts,
        };
        Ok(SeekedTo {
            track_id: 0,
            required_ts,
            actual_ts: self.ts,
        })
    }

    fn tracks(&self) -> &[Track] {
        std::slice::from_ref(&self.track)
    }

    fn next_packet(&mut self) -> SymphResult<Packet> {
        enter_mixer();
        if self.poisoned {
            return Err(SymphError::IoError(ErrorKind::UnexpectedEof.into()));
        }
        match catch_unwind(AssertUnwindSafe(|| self.next_inner())) {
            Ok(r) => r,
            Err(_) => {
                self.poisoned = true;
                self.done.flags().hint(DECODE_ERROR | PANICKED);
                Err(SymphError::IoError(ErrorKind::UnexpectedEof.into()))
            }
        }
    }

    fn into_inner(mut self: Box<Self>) -> MediaSourceStream {
        self.mss.take().unwrap_or_else(|| {
            MediaSourceStream::new(Box::new(Cursor::new(Vec::<u8>::new())), Default::default())
        })
    }
}

impl Drop for OpusStreamReader {
    fn drop(&mut self) {
        self.q.reader_gone.store(true, Ordering::Release);
    }
}

/// `Input::Live(LiveInput::Parsed(..))` over an `OpusQueue` (`CODEC_TYPE_OPUS`, 48 kHz).
pub fn opus_stream_input(q: Arc<OpusQueue>, done: DoneGuard) -> Result<Input, CoreError> {
    let (meta, mss) = probe_raw_header(2)?;
    let params = opus_codec_params();
    let decoder = get_codec_registry()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| CoreError::Source(format!("no Opus decoder: {e}")))?;
    let reader = OpusStreamReader {
        q,
        track: Track::new(0, params),
        ts: 0,
        done,
        mss: Some(mss),
        meta: MetadataLog::default(),
        stash: Vec::with_capacity(READER_STASH),
        poisoned: false,
        _input: LiveCounter::new(&live::SONGBIRD_INPUT),
    };
    Ok(Input::Live(
        LiveInput::Parsed(Parsed {
            format: Box::new(reader),
            decoder,
            track_id: 0,
            meta,
            supports_backseek: false,
        }),
        None,
    ))
}

#[cfg(all(test, unix))]
pub(crate) mod tests;
