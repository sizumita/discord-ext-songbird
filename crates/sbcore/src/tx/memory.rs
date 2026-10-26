//! In-memory sources (W4, §7.9): `AudioFile` bytes (probed eagerly), `RawPcm` samples and
//! `OpusPackets`. Each is copied once into an `Arc` shared by the Python object and every track
//! (an `Arc<Vec<_>>`, so building it never copies a second time).
//!
//! Per-track state lives in small wrappers created at play time: [`MemPcmCursor`] (a
//! [`PcmProvider`] read by `LiveReader` + `PcmDecoder`), [`MemOpusReader`] and [`DoneSource`] (a
//! fresh cursor over the shared bytes for songbird's own decoder).

use std::io::{self, Cursor, ErrorKind, Read, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use songbird::input::codecs::{get_codec_registry, get_probe};
use songbird::input::core::codecs::DecoderOptions;
use songbird::input::core::errors::{Error as SymphError, Result as SymphResult};
use songbird::input::core::formats::{
    Cue, FormatOptions, FormatReader, Packet, SeekMode, SeekTo, SeekedTo, Track,
};
use songbird::input::core::io::{MediaSource, MediaSourceStream};
use songbird::input::core::meta::{Metadata, MetadataLog, MetadataOptions};
use songbird::input::core::probe::Hint;
use songbird::input::core::units::TimeBase;
use songbird::input::{AudioStream, Input, LiveInput, Parsed};

use super::opus_stream::{opus_codec_params, validate_opus_packet};
use super::probe::probe_raw_header;
use super::{PcmProvider, PullReport, enter_mixer};
use crate::dsp::StreamResampler;
use crate::dsp::mix::f32_to_i16;
use crate::error::CoreError;
use crate::format::{Encoding, Format, PcmParser, SampleRate};
use crate::limits::{
    FRAMES_PER_TICK_48K, LimitError, LimitNum, MAX_OPUS_PACKETS, MAX_SOURCE_BYTES, MIX_RATE,
    SOURCE_RATE_HZ, check,
};
use crate::runtime::{LiveCounter, live};
use crate::tracks::flags::{DECODE_ERROR, DoneGuard, EOF_SEEN};

fn too_big(what: &'static str, value: u64, max: u64) -> CoreError {
    CoreError::Limit(LimitError {
        what,
        value: LimitNum::Int(i128::from(value)),
        min: LimitNum::Int(0),
        max: LimitNum::Int(i128::from(max)),
    })
}

/// What the eager probe found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbeInfo {
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    pub duration_ms: Option<u64>,
}

/// Shared bytes viewed through a `Cursor` (a `MediaSource`).
#[derive(Clone, Debug)]
pub(crate) struct ArcBytes(pub(crate) Arc<Vec<u8>>);

impl AsRef<[u8]> for ArcBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

/// Wraps the media source of one played `AudioFile`: records `EOF_SEEN` when a read returns 0
/// and drops the track's `DoneGuard` with the input (§7.9).
pub(crate) struct DoneSource<M: MediaSource> {
    inner: M,
    done: DoneGuard,
    _input: LiveCounter,
}

impl<M: MediaSource> DoneSource<M> {
    pub(crate) fn new(inner: M, done: DoneGuard) -> Self {
        DoneSource {
            inner,
            done,
            _input: LiveCounter::new(&live::SONGBIRD_INPUT),
        }
    }
}

impl<M: MediaSource> Read for DoneSource<M> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 && !buf.is_empty() {
            self.done.flags().hint(EOF_SEEN);
        }
        Ok(n)
    }
}

impl<M: MediaSource> Seek for DoneSource<M> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl<M: MediaSource> MediaSource for DoneSource<M> {
    fn is_seekable(&self) -> bool {
        self.inner.is_seekable()
    }

    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }
}

/// Encoded audio held in memory (`native.AudioFile`).
#[derive(Debug)]
pub struct EncodedBytes {
    data: Arc<Vec<u8>>,
    info: ProbeInfo,
}

impl EncodedBytes {
    /// Probes eagerly (`Source` error if unrecognised or if no compiled-in codec can decode it);
    /// `info` comes from the probe result. Runs on a Python thread with the thread state detached.
    pub fn new(data: Vec<u8>) -> Result<Self, CoreError> {
        if data.len() > MAX_SOURCE_BYTES {
            return Err(too_big(
                "source_bytes",
                data.len() as u64,
                MAX_SOURCE_BYTES as u64,
            ));
        }
        if data.is_empty() {
            return Err(CoreError::Source("empty audio data".into()));
        }
        let data = Arc::new(data);
        let info = probe_bytes(&data)?;
        Ok(EncodedBytes { data, info })
    }

    pub fn info(&self) -> &ProbeInfo {
        &self.info
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The shared bytes (each play gets a fresh cursor over them).
    pub fn data(&self) -> &[u8] {
        self.data.as_slice()
    }

    /// A songbird input over a fresh cursor (songbird probes it on its BlockyTaskPool).
    pub(crate) fn to_input(&self, done: DoneGuard) -> Input {
        let src = DoneSource::new(Cursor::new(ArcBytes(self.data.clone())), done);
        Input::Live(
            LiveInput::Raw(AudioStream {
                input: Box::new(src),
            }),
            None,
        )
    }
}

/// Probes `data` like songbird will (default track, else the first decodable one).
fn probe_bytes(data: &Arc<Vec<u8>>) -> Result<ProbeInfo, CoreError> {
    let mss = MediaSourceStream::new(
        Box::new(Cursor::new(ArcBytes(data.clone()))),
        Default::default(),
    );
    let probed = get_probe()
        .format(
            &Hint::new(),
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| CoreError::Source(format!("unrecognised audio data: {e}")))?;
    let format = probed.format;
    let registry = get_codec_registry();
    let decodable = |t: &&Track| {
        registry
            .make(&t.codec_params, &DecoderOptions::default())
            .is_ok()
    };
    let track = format
        .default_track()
        .filter(decodable)
        .or_else(|| format.tracks().iter().find(decodable))
        .ok_or_else(|| {
            CoreError::Source("no track with a supported codec (see supported_codecs())".into())
        })?;
    let p = &track.codec_params;
    let codec = registry
        .get_codec(p.codec)
        .map_or_else(|| format!("{}", p.codec), |d| d.short_name.to_string());
    let sample_rate = p.sample_rate;
    let duration_ms = match (p.n_frames, sample_rate) {
        (Some(n), Some(r)) if r > 0 => Some(n.saturating_mul(1_000) / u64::from(r)),
        _ => None,
    };
    Ok(ProbeInfo {
        codec,
        sample_rate,
        channels: p
            .channels
            .map(|c| u8::try_from(c.count()).unwrap_or(u8::MAX)),
        duration_ms,
    })
}

/// Reads a whole file (≤ `max_bytes`, `try_reserve`). Called under `py.detach`.
/// I/O failures are `CoreError::Io` (→ `OSError`); a file over `max_bytes` is `CoreError::Limit`.
pub fn read_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>, CoreError> {
    let file = std::fs::File::open(path).map_err(CoreError::Io)?;
    let len = file.metadata().map_err(CoreError::Io)?.len();
    let max = max_bytes as u64;
    if len > max {
        return Err(too_big("file_bytes", len, max));
    }
    let mut out = Vec::new();
    out.try_reserve_exact(usize::try_from(len).unwrap_or(max_bytes))?;
    file.take(max.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(CoreError::Io)?;
    if out.len() as u64 > max {
        return Err(too_big("file_bytes", out.len() as u64, max));
    }
    Ok(out)
}

/// Raw PCM held in memory (`native.RawPcm`), interleaved `i16` at a stream rate.
#[derive(Debug)]
pub struct MemPcm {
    fmt: Format,
    samples: Arc<Vec<i16>>,
}

impl MemPcm {
    /// Whole frames of `data` in `fmt` (parsed once). A trailing partial frame is `InvalidAudio`.
    pub fn from_bytes(fmt: Format, data: &[u8]) -> Result<Self, CoreError> {
        if data.len() > MAX_SOURCE_BYTES {
            return Err(too_big(
                "source_bytes",
                data.len() as u64,
                MAX_SOURCE_BYTES as u64,
            ));
        }
        if !data.len().is_multiple_of(fmt.frame_bytes()) {
            return Err(CoreError::invalid(
                "data length is not a whole number of frames",
            ));
        }
        let mut samples = Vec::new();
        PcmParser::new(fmt).parse_frames(data, &mut samples)?;
        Ok(MemPcm {
            fmt,
            samples: Arc::new(samples),
        })
    }

    /// Takes interleaved samples in `fmt` (length must be whole frames).
    pub fn from_i16(fmt: Format, samples: Vec<i16>) -> Result<Self, CoreError> {
        if !samples
            .len()
            .is_multiple_of(usize::from(fmt.channels.max(1)))
        {
            return Err(CoreError::invalid(
                "sample count is not a whole number of frames",
            ));
        }
        if samples.len().saturating_mul(2) > MAX_SOURCE_BYTES {
            return Err(too_big(
                "source_bytes",
                (samples.len() as u64).saturating_mul(2),
                MAX_SOURCE_BYTES as u64,
            ));
        }
        Ok(MemPcm {
            fmt,
            samples: Arc::new(samples),
        })
    }

    /// Any rate in `SOURCE_RATE_HZ` (pcm16le/f32le); resampled once to 48 kHz unless 8/16/24/48 kHz.
    /// CPU-heavy: callers run it with the thread state detached.
    pub fn from_any_rate(
        rate_hz: u32,
        channels: u8,
        enc: Encoding,
        data: &[u8],
    ) -> Result<Self, CoreError> {
        check("sample_rate", rate_hz, SOURCE_RATE_HZ)?;
        if !(1..=2).contains(&channels) {
            return Err(CoreError::invalid("channels must be 1 or 2"));
        }
        if !matches!(enc, Encoding::Pcm16Le | Encoding::F32Le) {
            return Err(CoreError::invalid(
                "from_samples encoding must be 'pcm16le' or 'f32le'",
            ));
        }
        if SampleRate::from_hz(rate_hz).is_some() {
            return Self::from_bytes(Format::new(rate_hz, channels, enc)?, data);
        }
        if data.len() > MAX_SOURCE_BYTES {
            return Err(too_big(
                "source_bytes",
                data.len() as u64,
                MAX_SOURCE_BYTES as u64,
            ));
        }
        let ch = usize::from(channels);
        let fb = enc.sample_width() * ch;
        if !data.len().is_multiple_of(fb) {
            return Err(CoreError::invalid(
                "data length is not a whole number of frames",
            ));
        }
        let n = data.len() / enc.sample_width();
        let mut input: Vec<f32> = Vec::new();
        input.try_reserve_exact(n)?;
        match enc {
            Encoding::Pcm16Le => {
                let (chunks, _) = data.as_chunks::<2>();
                input.extend(
                    chunks
                        .iter()
                        .map(|c| f32::from(i16::from_le_bytes(*c)) / 32_768.0),
                );
            }
            _ => {
                let (chunks, _) = data.as_chunks::<4>();
                input.extend(chunks.iter().map(|c| f32::from_le_bytes(*c)));
            }
        }
        let mut rs = StreamResampler::new(rate_hz, SampleRate::Hz48000, ch)?;
        let frames = (n / ch) as u64;
        let expected = usize::try_from(rs.expected_output_frames(frames))
            .unwrap_or(usize::MAX)
            .saturating_add(4096)
            .saturating_mul(ch);
        let mut out: Vec<f32> = Vec::new();
        out.try_reserve_exact(expected)?;
        rs.process(&input, &mut out);
        rs.flush(&mut out);
        drop(input);
        let mut samples = vec![0i16; 0];
        samples.try_reserve_exact(out.len())?;
        samples.resize(out.len(), 0);
        f32_to_i16(&out, &mut samples);
        drop(out);
        Self::from_i16(Format::new(MIX_RATE, channels, enc)?, samples)
    }

    pub fn format(&self) -> Format {
        self.fmt
    }

    /// Number of frames.
    pub fn frames(&self) -> u64 {
        (self.samples.len() / usize::from(self.fmt.channels.max(1))) as u64
    }

    pub fn duration_ms(&self) -> u64 {
        self.fmt.ms_for_frames(self.frames())
    }

    /// A songbird input playing these samples from the start (seekable, loopable).
    pub(crate) fn to_input(self: &Arc<Self>, done: DoneGuard) -> Result<Input, CoreError> {
        let cursor = Arc::new(MemPcmCursor {
            pcm: self.clone(),
            pos: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        });
        super::reader::live_pcm_input(cursor, done, true)
    }
}

/// The per-track read position over a shared [`MemPcm`].
#[derive(Debug)]
pub(crate) struct MemPcmCursor {
    pcm: Arc<MemPcm>,
    /// Next frame to play.
    pos: AtomicU64,
    /// Bumped by seeks (the decoder resets its resampler and declicks).
    generation: AtomicU64,
}

impl PcmProvider for MemPcmCursor {
    fn format(&self) -> Format {
        self.pcm.fmt
    }

    fn pull(&self, out: &mut [i16]) -> PullReport {
        let ch = usize::from(self.pcm.fmt.channels.max(1));
        let n = self.pcm.fmt.frames_per_tick() as u64;
        let total = self.pcm.frames();
        let pos = self.pos.load(Ordering::Acquire).min(total);
        let take = n.min(total - pos);
        let start = pos as usize * ch;
        let len = take as usize * ch;
        if let (Some(dst), Some(src)) =
            (out.get_mut(..len), self.pcm.samples.get(start..start + len))
        {
            dst.copy_from_slice(src);
        }
        if let Some(rest) = out.get_mut(len..) {
            rest.fill(0);
        }
        self.pos.store(pos + take, Ordering::Release);
        let t = u32::try_from(take).unwrap_or(u32::MAX);
        PullReport {
            consumed: t,
            real: t,
        }
    }

    fn should_end(&self) -> bool {
        self.pos.load(Ordering::Acquire) >= self.pcm.frames()
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn volume(&self) -> f32 {
        // songbird applies the track volume.
        1.0
    }

    fn seek_to_frame(&self, frame: u64) -> Option<u64> {
        let f = frame.min(self.pcm.frames());
        self.pos.store(f, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        Some(f)
    }
}

/// Pre-encoded 20 ms / 48 kHz Opus packets (`native.OpusPackets`).
#[derive(Debug)]
pub struct OpusFrames {
    bytes: Arc<Vec<u8>>,
    /// `len() + 1` offsets: packet `i` is `bytes[offsets[i]..offsets[i + 1]]`.
    offsets: Arc<Vec<u32>>,
}

impl OpusFrames {
    /// Validates every packet (`validate_opus_packet`, ≤ `MAX_OPUS_PACKETS`, ≤ `MAX_SOURCE_BYTES`).
    pub fn from_packets<'a>(it: impl IntoIterator<Item = &'a [u8]>) -> Result<Self, CoreError> {
        let mut bytes: Vec<u8> = Vec::new();
        let mut offsets: Vec<u32> = vec![0];
        for p in it {
            if offsets.len() > MAX_OPUS_PACKETS {
                return Err(too_big(
                    "opus_packets",
                    offsets.len() as u64,
                    MAX_OPUS_PACKETS as u64,
                ));
            }
            validate_opus_packet(p)?;
            let total = bytes.len() + p.len();
            if total > MAX_SOURCE_BYTES {
                return Err(too_big(
                    "source_bytes",
                    total as u64,
                    MAX_SOURCE_BYTES as u64,
                ));
            }
            bytes.try_reserve(p.len())?;
            bytes.extend_from_slice(p);
            offsets.try_reserve(1)?;
            offsets.push(u32::try_from(total).unwrap_or(u32::MAX));
        }
        Ok(OpusFrames {
            bytes: Arc::new(bytes),
            offsets: Arc::new(offsets),
        })
    }

    /// Number of packets.
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `len() × 20` ms.
    pub fn duration_ms(&self) -> u64 {
        (self.len() as u64).saturating_mul(20)
    }

    /// Packet `i`.
    pub fn packet(&self, i: usize) -> Option<&[u8]> {
        let start = *self.offsets.get(i)? as usize;
        let end = *self.offsets.get(i + 1)? as usize;
        self.bytes.get(start..end)
    }

    /// A songbird input playing these packets (passthrough-capable, seekable by index).
    pub(crate) fn to_input(self: &Arc<Self>, done: DoneGuard) -> Result<Input, CoreError> {
        let (meta, mss) = probe_raw_header(2)?;
        let params = opus_codec_params();
        let decoder = get_codec_registry()
            .make(&params, &DecoderOptions::default())
            .map_err(|e| CoreError::Source(format!("no Opus decoder: {e}")))?;
        let reader = MemOpusReader {
            frames: self.clone(),
            idx: 0,
            track: Track::new(0, params),
            done,
            mss: Some(mss),
            meta: MetadataLog::default(),
            _input: LiveCounter::new(&live::SONGBIRD_INPUT),
        };
        Ok(Input::Live(
            LiveInput::Parsed(Parsed {
                format: Box::new(reader),
                decoder,
                track_id: 0,
                meta,
                supports_backseek: true,
            }),
            None,
        ))
    }
}

/// The reader of an `OpusPackets` track: one packet-sized `Box` copy per tick (symphonia's
/// `Packet` owns its buffer).
pub(crate) struct MemOpusReader {
    frames: Arc<OpusFrames>,
    idx: usize,
    track: Track,
    done: DoneGuard,
    mss: Option<MediaSourceStream>,
    meta: MetadataLog,
    _input: LiveCounter,
}

impl MemOpusReader {
    fn next_inner(&mut self) -> SymphResult<Packet> {
        let Some(p) = self.frames.packet(self.idx) else {
            self.done.flags().hint(EOF_SEEN);
            return Err(SymphError::IoError(ErrorKind::UnexpectedEof.into()));
        };
        let ts = (self.idx as u64).saturating_mul(FRAMES_PER_TICK_48K as u64);
        self.idx += 1;
        Ok(Packet::new_from_boxed_slice(
            0,
            ts,
            FRAMES_PER_TICK_48K as u64,
            Box::from(p),
        ))
    }
}

impl FormatReader for MemOpusReader {
    fn try_new(_source: MediaSourceStream, _options: &FormatOptions) -> SymphResult<Self>
    where
        Self: Sized,
    {
        Err(SymphError::Unsupported(
            "MemOpusReader is constructed by sbcore, not probed",
        ))
    }

    fn cues(&self) -> &[Cue] {
        &[]
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.meta.metadata()
    }

    /// Seeks by packet index (`ts / 960`), clamped to the end.
    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> SymphResult<SeekedTo> {
        let required_ts = match to {
            SeekTo::TimeStamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => TimeBase::new(1, MIX_RATE).calc_timestamp(time),
        };
        let idx = required_ts / FRAMES_PER_TICK_48K as u64;
        self.idx = usize::try_from(idx)
            .unwrap_or(usize::MAX)
            .min(self.frames.len());
        Ok(SeekedTo {
            track_id: 0,
            required_ts,
            actual_ts: (self.idx as u64).saturating_mul(FRAMES_PER_TICK_48K as u64),
        })
    }

    fn tracks(&self) -> &[Track] {
        std::slice::from_ref(&self.track)
    }

    fn next_packet(&mut self) -> SymphResult<Packet> {
        enter_mixer();
        match catch_unwind(AssertUnwindSafe(|| self.next_inner())) {
            Ok(r) => r,
            Err(_) => {
                self.done.flags().hint(DECODE_ERROR);
                self.idx = self.frames.len();
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

#[cfg(all(test, unix))]
pub(crate) mod tests;
