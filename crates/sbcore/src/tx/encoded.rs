//! Push-based encoded streams (W4, §7.9): `ByteRing` (L8 + condvar) → a `songbird-decode-N` thread
//! (symphonia probe/decode, `StreamResampler` to 48 kHz) → an internal `PcmQueue` (48 kHz stereo,
//! 1 s cap) → `LiveReader`. At most `MAX_ENCODED_WORKERS` decode threads per process.
//!
//! Every blocking wait of the decode thread (ring bytes, internal-queue space) is a timed condvar
//! wait that `close()` wakes, so teardown never waits on it.

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use smallvec::SmallVec;
use songbird::input::codecs::{get_codec_registry, get_probe};
use songbird::input::core::audio::SampleBuffer;
use songbird::input::core::codecs::{Decoder, DecoderOptions};
use songbird::input::core::errors::Error as SymphError;
use songbird::input::core::formats::{FormatOptions, FormatReader, Track};
use songbird::input::core::io::{MediaSource, MediaSourceStream};
use songbird::input::core::meta::MetadataOptions;
use songbird::input::core::probe::Hint;

use super::queue::{DisconnectPolicy, PcmQueue, PlaybackSpec};
use super::reader::live_pcm_input;
use super::{CloseMode, HouseStatus, LiveSource, PcmProvider, TrackControl, next_source_id};
use crate::clock::Clock;
use crate::dsp::StreamResampler;
use crate::dsp::mix::f32_sample_to_i16;
use crate::error::{CloseReason, CoreError};
use crate::events::{EventData, EventHub};
use crate::format::{PCM16_48K_STEREO, SampleRate};
use crate::limits::{
    ENCODED_BUFFER_BYTES, LimitError, LimitNum, MAX_ENCODED_WORKERS, MIX_RATE, SOURCE_RATE_HZ,
    VOLUME, check,
};
use crate::runtime::{LiveCounter, Role, live, set_role};
use crate::sync::lock_checked;
use crate::tracks::flags::{DECODE_ERROR, DoneGuard, PANICKED, TrackFlags};
use crate::wake::{WakeId, WakeToken};

/// Live decode threads in this process (`MAX_ENCODED_WORKERS`).
pub static ENCODED_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// Names `songbird-decode-N` threads.
static DECODE_THREAD_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Ids of the encoded streams whose decode thread probed its input (tests).
#[cfg(test)]
static PROBED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Ids of the encoded streams whose decode thread closes the ring itself right before the probe,
/// as a `close()` landing between `wait_for_input` and the probe's first read would (tests).
#[cfg(test)]
static CLOSE_BEFORE_PROBE: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// `(stream id, log::dependencies_muted())` of every decode thread at its exit (tests).
#[cfg(test)]
static MUTED_AT_EXIT: Mutex<Vec<(u32, bool)>> = Mutex::new(Vec::new());

/// Decoded lookahead of the internal queue.
const LOOKAHEAD_MS: u32 = 1_000;
/// Decoded audio is pushed in pieces of at most this many 48 kHz frames (100 ms).
const PUSH_FRAMES: usize = 4_800;
/// Upper bound of every timed wait of the decode thread.
const WAIT: Duration = Duration::from_millis(50);

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
    /// Bytes a Python writer waits for (0 = nobody).
    space_wanted: usize,
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
    /// Mirror of `ring.closed` for lock-free checks.
    closed: AtomicBool,
    conn_closed: AtomicBool,
    end_reported: AtomicBool,
}

impl std::fmt::Debug for EncodedShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodedShared")
            .field("id", &self.id)
            .field("cap", &self.ring.cap)
            .finish_non_exhaustive()
    }
}

impl EncodedShared {
    fn closed_err(&self) -> CoreError {
        if self.conn_closed.load(Ordering::Acquire) {
            CoreError::StreamClosed(CloseReason::ConnectionClosed)
        } else {
            CoreError::StreamClosed(CloseReason::Closed)
        }
    }

    /// Appends encoded bytes (`PlaybackFull` when the ring is full; nothing consumed). A write
    /// larger than the whole ring is `LimitExceeded`. `PlaybackFull` reports 0 ms for both
    /// fields: the duration of encoded bytes is unknown.
    pub fn write(&self, data: &[u8]) -> Result<(), CoreError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_err());
        }
        if data.len() > self.ring.cap {
            return Err(CoreError::Limit(LimitError {
                what: "write_bytes",
                value: LimitNum::Int(data.len() as i128),
                min: LimitNum::Int(0),
                max: LimitNum::Int(self.ring.cap as i128),
            }));
        }
        {
            let mut g = lock_checked(&self.ring.inner);
            if g.closed {
                return Err(self.closed_err());
            }
            if g.finished {
                return Err(CoreError::StreamClosed(CloseReason::Closed));
            }
            if g.bytes.len() + data.len() > self.ring.cap {
                g.space_wanted = data.len().max(1);
                return Err(CoreError::PlaybackFull {
                    needed_ms: 0,
                    free_ms: 0,
                });
            }
            g.bytes.try_reserve(data.len())?;
            g.bytes.extend(data.iter().copied());
        }
        self.ring.readable.notify_all();
        Ok(())
    }

    /// The largest single write (`max_buffer_bytes`): the binding checks a buffer against it before
    /// copying it (§7.3).
    pub fn max_write_bytes(&self) -> usize {
        self.ring.cap
    }

    /// Arms a space wake-up for `n` bytes; `true` ⇒ there is already enough space (or closed).
    pub fn arm_space(&self, n: usize) -> bool {
        let mut g = lock_checked(&self.ring.inner);
        if g.closed || g.finished {
            return true;
        }
        let wanted = n.min(self.ring.cap).max(1);
        if self.ring.cap - g.bytes.len() >= wanted {
            g.space_wanted = 0;
            true
        } else {
            g.space_wanted = wanted;
            false
        }
    }

    /// End of input: the track ends after the decoded audio has played.
    pub fn finish(&self) {
        {
            let mut g = lock_checked(&self.ring.inner);
            g.finished = true;
        }
        self.ring.readable.notify_all();
    }

    pub fn wake_id(&self) -> WakeId {
        self.token.id()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.pcm.is_closed()
    }

    /// Whether `finish()` (or `close(drain=True)`) was called.
    pub fn is_finished(&self) -> bool {
        lock_checked(&self.ring.inner).finished
    }

    /// The owner (`native.EncodedSink`) dropped its handle: abort, unless the input was already
    /// complete (`finish()` / `close(drain=True)`: the decoded audio still plays out and the
    /// track ends on its own, as for a file) or the stream is closed.
    pub fn abandon(&self) {
        if !self.is_finished() && !self.is_closed() {
            LiveSource::close(self, CloseMode::Abort);
        }
    }

    /// The internal decoded-audio queue (the songbird track reads it).
    #[cfg(test)]
    pub(crate) fn pcm(&self) -> &Arc<PcmQueue> {
        &self.pcm
    }

    /// The internal decoded-audio queue as the track's live source (`TrackRef::live`, T5,
    /// `stop_all`). Unlike this `EncodedShared` (held by the decode thread, which exits at end of
    /// input, and by the Python sink), it lives as long as the songbird reader does, so the
    /// track's pause, resume and volume keep reaching it for the whole track: a user pause set
    /// before the input ended can still be lifted.
    pub(crate) fn live_queue(&self) -> Weak<dyn LiveSource> {
        Arc::downgrade(&self.pcm) as Weak<dyn LiveSource>
    }

    /// Real audio of this stream played so far, in ms (the drain progress `aclose` watches).
    pub fn played_ms(&self) -> u64 {
        self.pcm.stats().played_ms
    }

    /// Attaches songbird's track (or a `FakeTrackControl`), once (`ConnShared::open_encoded` and
    /// the queue's lazy `Encoded` entries). The first decoded frame resumes it.
    pub(crate) fn attach_track(&self, t: Box<dyn TrackControl>) {
        self.pcm.attach_track(t);
    }

    /// User pause (`paused=True` queue entries): the decoded audio is held and the track stays
    /// paused until `pause(false)`.
    pub(crate) fn pause(&self, paused: bool) {
        self.pcm.pause(paused);
    }

    /// Bytes buffered in the ring (not yet decoded).
    pub fn buffered_bytes(&self) -> usize {
        lock_checked(&self.ring.inner).bytes.len()
    }

    /// Decode thread: waits until the ring holds a byte or the input is finished; `false` if the
    /// stream closed first (there is nothing to probe).
    fn wait_for_input(&self) -> bool {
        let ring = &self.ring;
        let mut g = lock_checked(&ring.inner);
        loop {
            if g.closed {
                return false;
            }
            if !g.bytes.is_empty() || g.finished {
                return true;
            }
            let _ = ring.readable.wait_for(&mut g, WAIT);
        }
    }

    /// Closes the ring (wakes the decode thread and any waiter); queued bytes are freed.
    fn close_ring(&self) {
        self.closed.store(true, Ordering::Release);
        let old = {
            let mut g = lock_checked(&self.ring.inner);
            g.closed = true;
            g.space_wanted = 0;
            std::mem::take(&mut g.bytes)
        };
        drop(old);
        self.ring.readable.notify_all();
        self.space.notify_all();
    }

    /// Waits (bounded) until the internal queue can take `frames`; `false` once closed.
    fn wait_space(&self, frames: u64) -> bool {
        loop {
            if self.closed.load(Ordering::Acquire) || self.pcm.is_closed() {
                return false;
            }
            if self.pcm.free_frames() >= frames {
                return true;
            }
            let mut g = lock_checked(&self.space_lock);
            let _ = self.space.wait_for(&mut g, WAIT);
        }
    }

    /// Waits (bounded) for the next housekeeping tick or `close()`; `false` once closed.
    fn wait_tick(&self) -> bool {
        if self.closed.load(Ordering::Acquire) || self.pcm.is_closed() {
            return false;
        }
        let mut g = lock_checked(&self.space_lock);
        let _ = self.space.wait_for(&mut g, WAIT);
        !(self.closed.load(Ordering::Acquire) || self.pcm.is_closed())
    }
}

impl LiveSource for EncodedShared {
    fn id(&self) -> u32 {
        self.id
    }

    fn housekeep(&self, now: Instant, _events: &EventHub) -> HouseStatus {
        if self.pcm.reader_gone() {
            if !self.end_reported.swap(true, Ordering::AcqRel) {
                self.close_ring();
                let mut evs: SmallVec<[EventData; 2]> = SmallVec::new();
                let _ = self.pcm.housekeep_into(now, &mut evs);
                self.token.notify();
            }
            return HouseStatus::Finished;
        }
        let ring = {
            let mut g = lock_checked(&self.ring.inner);
            let free = self.ring.cap - g.bytes.len();
            if g.space_wanted > 0 && free >= g.space_wanted {
                g.space_wanted = 0;
                true
            } else {
                false
            }
        };
        let mut evs: SmallVec<[EventData; 2]> = SmallVec::new();
        let _ = self.pcm.housekeep_into(now, &mut evs);
        // The mixer freed space in the internal queue: let the decode thread continue.
        self.space.notify_all();
        if ring {
            self.token.notify();
        }
        HouseStatus::Live
    }

    fn on_link(&self, up: bool) {
        LiveSource::on_link(&*self.pcm, up);
    }

    fn close(&self, mode: CloseMode) {
        match mode {
            CloseMode::Abort => {
                self.close_ring();
                LiveSource::close(&*self.pcm, CloseMode::Abort);
            }
            CloseMode::Drain => self.finish(),
        }
        self.token.notify();
    }

    fn conn_closed(&self) {
        self.conn_closed.store(true, Ordering::Release);
        self.close_ring();
        self.pcm.conn_closed();
        // Wake writers waiting for ring space: they now get StreamClosed("connection_closed").
        self.token.notify();
    }

    fn set_user_paused(&self, paused: bool) -> bool {
        self.pcm.pause(paused);
        true
    }

    fn set_source_volume(&self, v: f32) -> bool {
        self.pcm.set_volume(v).is_ok()
    }

    fn source_volume(&self) -> Option<f32> {
        Some(self.pcm.volume())
    }

    fn is_released(&self) -> bool {
        LiveSource::is_released(&*self.pcm)
    }
}

/// The decode thread's view of the ring (a non-seekable `MediaSource`).
struct ByteRingSource {
    shared: Arc<EncodedShared>,
}

impl Read for ByteRingSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let ring = &self.shared.ring;
        let mut g = lock_checked(&ring.inner);
        loop {
            if g.closed {
                // `close()` cut the input off: whatever symphonia now reports about the truncated
                // stream on this thread (e.g. "probe reach EOF at 0 bytes." at ERROR when the close
                // landed between `wait_for_input` and the probe) is not an error.
                crate::log::mute_dependencies();
                return Ok(0);
            }
            if !g.bytes.is_empty() {
                let n = buf.len().min(g.bytes.len());
                for (d, s) in buf.iter_mut().zip(g.bytes.drain(..n)) {
                    *d = s;
                }
                return Ok(n);
            }
            if g.finished {
                return Ok(0);
            }
            let _ = ring.readable.wait_for(&mut g, WAIT);
        }
    }
}

impl Seek for ByteRingSource {
    fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "encoded streams are not seekable",
        ))
    }
}

impl MediaSource for ByteRingSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// Releases one `ENCODED_WORKERS` slot on drop.
struct WorkerSlot;

impl WorkerSlot {
    fn acquire() -> Result<Self, CoreError> {
        let prev = ENCODED_WORKERS.fetch_add(1, Ordering::AcqRel);
        if prev >= MAX_ENCODED_WORKERS {
            ENCODED_WORKERS.fetch_sub(1, Ordering::AcqRel);
            return Err(CoreError::Limit(LimitError {
                what: "encoded_streams",
                value: LimitNum::Int(prev as i128 + 1),
                min: LimitNum::Int(0),
                max: LimitNum::Int(MAX_ENCODED_WORKERS as i128),
            }));
        }
        Ok(WorkerSlot)
    }
}

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        ENCODED_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How the decode loop ended.
enum Outcome {
    /// Input finished and fully decoded.
    Eof,
    /// The stream was closed (abort, reader gone, connection closed).
    Closed,
    /// Unrecognised data or a fatal decode error.
    Failed,
}

/// Decoded audio → 48 kHz stereo i16 pushed into the internal queue.
struct Pusher<'a> {
    shared: &'a EncodedShared,
    rs: Option<(u32, StreamResampler)>,
    stereo: Vec<f32>,
    out48: Vec<f32>,
}

impl Pusher<'_> {
    /// Feeds interleaved f32 at `rate` with `channels` channels.
    fn feed(&mut self, samples: &[f32], rate: u32, channels: usize) -> Result<(), Outcome> {
        self.stereo.clear();
        match channels {
            0 => return Ok(()),
            1 => self.stereo.extend(samples.iter().flat_map(|s| [*s, *s])),
            _ => {
                for frame in samples.chunks_exact(channels) {
                    if let [l, r, ..] = frame {
                        self.stereo.push(*l);
                        self.stereo.push(*r);
                    }
                }
            }
        }
        if rate == MIX_RATE {
            self.flush()?;
            let buf = std::mem::take(&mut self.stereo);
            let r = self.push48(&buf);
            self.stereo = buf;
            return r;
        }
        if check("sample_rate", rate, SOURCE_RATE_HZ).is_err() {
            return Err(Outcome::Failed);
        }
        if self.rs.as_ref().is_none_or(|(r, _)| *r != rate) {
            self.flush()?;
            let rs =
                StreamResampler::new(rate, SampleRate::Hz48000, 2).map_err(|_| Outcome::Failed)?;
            self.rs = Some((rate, rs));
        }
        self.out48.clear();
        if let Some((_, rs)) = self.rs.as_mut() {
            rs.process(&self.stereo, &mut self.out48);
        }
        let buf = std::mem::take(&mut self.out48);
        let r = self.push48(&buf);
        self.out48 = buf;
        r
    }

    /// Drains the resampler (end of input or a rate change).
    fn flush(&mut self) -> Result<(), Outcome> {
        if let Some((_, mut rs)) = self.rs.take() {
            let mut tail = Vec::new();
            rs.flush(&mut tail);
            self.push48(&tail)?;
        }
        Ok(())
    }

    fn push48(&self, samples: &[f32]) -> Result<(), Outcome> {
        for piece in samples.chunks(PUSH_FRAMES * 2) {
            let frames = (piece.len() / 2) as u64;
            if frames == 0 {
                continue;
            }
            loop {
                if !self.shared.wait_space(frames) {
                    return Err(Outcome::Closed);
                }
                let boxed: Box<[i16]> = piece
                    .get(..frames as usize * 2)
                    .unwrap_or(&[])
                    .iter()
                    .map(|s| f32_sample_to_i16(*s))
                    .collect();
                match self.shared.pcm.write_samples(boxed) {
                    Ok(_) => break,
                    // Frames fit (only this thread writes), but the queue's segment bound did not
                    // (a codec with tiny packets): wait for the mixer to consume, then retry.
                    Err(CoreError::PlaybackFull { .. }) => {
                        if !self.shared.wait_tick() {
                            return Err(Outcome::Closed);
                        }
                    }
                    Err(_) => return Err(Outcome::Closed),
                }
            }
        }
        Ok(())
    }
}

/// Consecutive undecodable packets tolerated before an encoded stream fails (a long stream with
/// occasional corrupt frames, e.g. an MP3 joined mid-stream, keeps playing).
const MAX_BAD_PACKETS_IN_A_ROW: u32 = 64;

/// Counts consecutive undecodable packets.
#[derive(Debug, Default)]
struct BadPackets(u32);

impl BadPackets {
    /// A packet failed to decode; `false` once the limit is exceeded.
    fn bad(&mut self) -> bool {
        self.0 += 1;
        self.0 <= MAX_BAD_PACKETS_IN_A_ROW
    }

    /// A packet decoded.
    fn good(&mut self) {
        self.0 = 0;
    }
}

/// A decoder for the reader's default track, else for its first decodable one, and that track's
/// id (as songbird picks a file's track).
fn pick_decoder(format: &dyn FormatReader) -> Option<(Box<dyn Decoder>, u32)> {
    let registry = get_codec_registry();
    let make = |t: &Track| {
        registry
            .make(&t.codec_params, &DecoderOptions::default())
            .ok()
            .map(|d| (d, t.id))
    };
    format
        .default_track()
        .and_then(make)
        .or_else(|| format.tracks().iter().find_map(make))
}

/// The body of a decode thread.
fn decode_loop(shared: &Arc<EncodedShared>) -> Outcome {
    // A stream closed before its first byte (never written, a queue entry skipped or the connection
    // closed before the feed produced data) is a normal close: probing it would only make symphonia
    // log "probe reach EOF at 0 bytes" at ERROR (symphonia-core 0.5.5 `probe.rs:298`).
    if !shared.wait_for_input() {
        return Outcome::Closed;
    }
    #[cfg(test)]
    PROBED.lock().push(shared.id);
    #[cfg(test)]
    if CLOSE_BEFORE_PROBE.lock().contains(&shared.id) {
        shared.close_ring();
    }
    let mss = MediaSourceStream::new(
        Box::new(ByteRingSource {
            shared: shared.clone(),
        }),
        Default::default(),
    );
    let closed_or = |o: Outcome| {
        if shared.closed.load(Ordering::Acquire) {
            Outcome::Closed
        } else {
            o
        }
    };
    let probed = match get_probe().format(
        &Hint::new(),
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(p) => p,
        Err(_) => return closed_or(Outcome::Failed),
    };
    let mut format = probed.format;
    let Some((mut decoder, mut track_id)) = pick_decoder(&*format) else {
        return closed_or(Outcome::Failed);
    };
    let mut pusher = Pusher {
        shared,
        rs: None,
        stereo: Vec::new(),
        out48: Vec::new(),
    };
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    let mut bad_packets = BadPackets::default();
    loop {
        if shared.closed.load(Ordering::Acquire) {
            return Outcome::Closed;
        }
        let pkt = match format.next_packet() {
            Ok(p) => p,
            Err(SymphError::ResetRequired) => {
                // A chained Ogg stream (one physical stream per song, as Icecast radio sends)
                // began a new physical stream: its tracks replace the old ones, so pick the
                // decodable one again and carry on. The pusher keeps its resampler (it flushes
                // and rebuilds it if the rate changed).
                if shared.closed.load(Ordering::Acquire) {
                    return Outcome::Closed;
                }
                let Some((d, id)) = pick_decoder(&*format) else {
                    return closed_or(Outcome::Failed);
                };
                decoder = d;
                track_id = id;
                sbuf = None;
                bad_packets = BadPackets::default();
                continue;
            }
            Err(e) => {
                if shared.closed.load(Ordering::Acquire) {
                    return Outcome::Closed;
                }
                let drained = {
                    let g = lock_checked(&shared.ring.inner);
                    g.finished && g.bytes.is_empty()
                };
                let eof =
                    matches!(&e, SymphError::IoError(io) if io.kind() == ErrorKind::UnexpectedEof);
                if eof || drained {
                    break;
                }
                return Outcome::Failed;
            }
        };
        if pkt.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&pkt) {
            Ok(d) => {
                bad_packets.good();
                d
            }
            Err(SymphError::DecodeError(_)) | Err(SymphError::IoError(_)) => {
                // A corrupt packet: skip it, but give up on a run of them.
                if !bad_packets.bad() {
                    return closed_or(Outcome::Failed);
                }
                continue;
            }
            Err(_) => return closed_or(Outcome::Failed),
        };
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        let spec = *decoded.spec();
        let need = decoded.capacity() as u64;
        if sbuf
            .as_ref()
            .is_none_or(|b| (b.capacity() as u64) < need * spec.channels.count() as u64)
        {
            sbuf = Some(SampleBuffer::<f32>::new(need, spec));
        }
        let Some(b) = sbuf.as_mut() else {
            return Outcome::Failed;
        };
        b.copy_interleaved_ref(decoded);
        if let Err(o) = pusher.feed(b.samples(), spec.rate, spec.channels.count()) {
            return o;
        }
    }
    if let Err(o) = pusher.flush() {
        return o;
    }
    Outcome::Eof
}

fn decode_main(shared: Arc<EncodedShared>, flags: Arc<TrackFlags>, slot: WorkerSlot) {
    set_role(Role::Decode);
    let outcome = match catch_unwind(AssertUnwindSafe(|| decode_loop(&shared))) {
        Ok(o) => o,
        Err(_) => {
            flags.hint(PANICKED);
            ::log::error!(
                "encoded stream {}: the decode thread panicked; the track ends with reason \
                 'error' (please report this bug)",
                shared.id
            );
            Outcome::Failed
        }
    };
    match outcome {
        Outcome::Eof => LiveSource::close(&*shared.pcm, CloseMode::Drain),
        Outcome::Closed => {}
        Outcome::Failed => {
            ::log::warn!(
                "encoded stream {}: unrecognised or undecodable data; the track ends with reason \
                 'error'",
                shared.id
            );
            flags.hint(DECODE_ERROR);
            shared.close_ring();
            shared.pcm.fail();
        }
    }
    #[cfg(test)]
    MUTED_AT_EXIT
        .lock()
        .push((shared.id, crate::log::dependencies_muted()));
    drop(shared);
    drop(slot);
}

/// Opens the ring, spawns the decode thread and builds the (paused) songbird input
/// (`LimitExceeded` beyond `MAX_ENCODED_WORKERS`). The internal `PcmQueue` takes its id from
/// `tx::next_source_id()`, and `EncodedShared`'s `LiveSource::id` is that id. Self-mute is not
/// shared with the connection and the internal queue uses the real clock: use
/// [`spawn_encoded_with`].
pub fn spawn_encoded(
    spec: EncodedSpec,
    token: WakeToken,
    done: DoneGuard,
) -> Result<(Arc<EncodedShared>, songbird::input::Input), CoreError> {
    spawn_encoded_with(
        spec,
        token,
        done,
        Arc::new(AtomicBool::new(false)),
        Clock::Real,
    )
}

/// [`spawn_encoded`] with the connection's send-mute flag (§7.8), so the stream holds its
/// position while self-muted like every other live source, and the connection's clock, so the
/// internal queue's idle detection and positions use the same time base as the housekeeper (a
/// detached connection's `ManualClock`).
pub fn spawn_encoded_with(
    spec: EncodedSpec,
    token: WakeToken,
    done: DoneGuard,
    send_muted: Arc<AtomicBool>,
    clock: Clock,
) -> Result<(Arc<EncodedShared>, songbird::input::Input), CoreError> {
    let cap = check(
        "max_buffer_bytes",
        spec.max_buffer_bytes,
        ENCODED_BUFFER_BYTES,
    )?;
    let volume = check("volume", spec.volume, VOLUME)?;
    let slot = WorkerSlot::acquire()?;
    let pcm_spec = PlaybackSpec {
        format: PCM16_48K_STEREO,
        max_buffer_ms: LOOKAHEAD_MS,
        prebuffer_ms: 0,
        idle_ms: 250,
        auto_pause: true,
        max_gap_ms: 0,
        output_latency_ms: 40,
        volume,
        when_disconnected: DisconnectPolicy::Buffer,
    };
    let pcm = PcmQueue::new_internal(next_source_id(), &pcm_spec, None, clock, send_muted, false)?;
    let flags = done.flags().clone();
    let provider: Arc<dyn PcmProvider> = pcm.clone();
    let input = live_pcm_input(provider, done, false)?;
    let shared = Arc::new(EncodedShared {
        id: LiveSource::id(&*pcm),
        ring: ByteRing {
            inner: Mutex::new(RingInner::default()),
            readable: Condvar::new(),
            cap,
        },
        pcm,
        space: Condvar::new(),
        space_lock: Mutex::new(()),
        token,
        closed: AtomicBool::new(false),
        conn_closed: AtomicBool::new(false),
        end_reported: AtomicBool::new(false),
    });
    let n = DECODE_THREAD_SEQ.fetch_add(1, Ordering::Relaxed);
    let thread_shared = shared.clone();
    // Counted from the spawn, not from when the OS first schedules the thread, so the live count
    // never misses a thread that holds a worker slot (a failed spawn drops the closure and both).
    let live = LiveCounter::new(&live::DECODE_THREAD);
    std::thread::Builder::new()
        .name(format!("songbird-decode-{n}"))
        .spawn(move || {
            let _live = live;
            decode_main(thread_shared, flags, slot)
        })
        .map_err(|_| CoreError::Internal("spawn decode thread"))?;
    Ok((shared, input))
}

#[cfg(all(test, unix))]
pub(crate) mod tests;
