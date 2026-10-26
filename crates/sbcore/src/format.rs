//! Raw-audio formats (§9.2), the aligned PCM buffer, and the lock-free write-side parser (§7.3).

use std::borrow::Cow;

use crate::dsp::{g711, mix};
use crate::error::CoreError;
use crate::limits::{LimitError, SILENCE_MS, check, mul_checked};

/// A stream sample rate. Files at any rate still play through songbird's decoder (§1.2).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SampleRate {
    Hz8000,
    Hz16000,
    Hz24000,
    Hz48000,
}

impl SampleRate {
    pub const ALL: [SampleRate; 4] = [
        SampleRate::Hz8000,
        SampleRate::Hz16000,
        SampleRate::Hz24000,
        SampleRate::Hz48000,
    ];

    pub const fn hz(self) -> u32 {
        match self {
            SampleRate::Hz8000 => 8_000,
            SampleRate::Hz16000 => 16_000,
            SampleRate::Hz24000 => 24_000,
            SampleRate::Hz48000 => 48_000,
        }
    }

    pub fn from_hz(hz: u32) -> Option<Self> {
        match hz {
            8_000 => Some(SampleRate::Hz8000),
            16_000 => Some(SampleRate::Hz16000),
            24_000 => Some(SampleRate::Hz24000),
            48_000 => Some(SampleRate::Hz48000),
            _ => None,
        }
    }

    /// Frames per 20 ms tick (`hz / 50`).
    pub const fn frames_per_tick(self) -> usize {
        (self.hz() / 50) as usize
    }
}

/// The wire encoding of a stream. Little-endian, interleaved.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Encoding {
    Pcm16Le,
    F32Le,
    MuLaw,
    ALaw,
}

impl Encoding {
    /// The Python name (`Literal["pcm16le", "f32le", "mulaw", "alaw"]`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Encoding::Pcm16Le => "pcm16le",
            Encoding::F32Le => "f32le",
            Encoding::MuLaw => "mulaw",
            Encoding::ALaw => "alaw",
        }
    }

    /// Parses a name or common alias (case-insensitive).
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "pcm16le" | "pcm16" | "pcm_s16le" | "s16le" | "int16" => Some(Encoding::Pcm16Le),
            "f32le" | "f32" | "pcm_f32le" | "float32" => Some(Encoding::F32Le),
            "mulaw" | "ulaw" | "pcmu" | "g711_ulaw" => Some(Encoding::MuLaw),
            "alaw" | "pcma" | "g711_alaw" => Some(Encoding::ALaw),
            _ => None,
        }
    }

    /// Bytes per sample: 2 (pcm16le), 4 (f32le), 1 (G.711).
    pub const fn sample_width(self) -> usize {
        match self {
            Encoding::Pcm16Le => 2,
            Encoding::F32Le => 4,
            Encoding::MuLaw | Encoding::ALaw => 1,
        }
    }

    pub const fn is_g711(self) -> bool {
        matches!(self, Encoding::MuLaw | Encoding::ALaw)
    }
}

/// An immutable raw-audio format: rate ∈ {8, 16, 24, 48} kHz, channels ∈ {1, 2}; G.711 ⇒ 8 kHz mono.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Format {
    pub rate: SampleRate,
    pub channels: u8,
    pub encoding: Encoding,
}

const fn preset(rate: SampleRate, channels: u8, encoding: Encoding) -> Format {
    Format {
        rate,
        channels,
        encoding,
    }
}

pub const PCM16_8K: Format = preset(SampleRate::Hz8000, 1, Encoding::Pcm16Le);
pub const PCM16_16K: Format = preset(SampleRate::Hz16000, 1, Encoding::Pcm16Le);
pub const PCM16_24K: Format = preset(SampleRate::Hz24000, 1, Encoding::Pcm16Le);
pub const PCM16_48K: Format = preset(SampleRate::Hz48000, 1, Encoding::Pcm16Le);
pub const PCM16_48K_STEREO: Format = preset(SampleRate::Hz48000, 2, Encoding::Pcm16Le);
pub const F32_48K: Format = preset(SampleRate::Hz48000, 1, Encoding::F32Le);
pub const F32_48K_STEREO: Format = preset(SampleRate::Hz48000, 2, Encoding::F32Le);
pub const MULAW_8K: Format = preset(SampleRate::Hz8000, 1, Encoding::MuLaw);
pub const ALAW_8K: Format = preset(SampleRate::Hz8000, 1, Encoding::ALaw);
/// `gpt-live-1` default: one format for input and output.
pub const GPT_LIVE: Format = PCM16_24K;
pub const OPENAI_REALTIME: Format = PCM16_24K;
pub const GEMINI_LIVE_INPUT: Format = PCM16_16K;
pub const GEMINI_LIVE_OUTPUT: Format = PCM16_24K;

/// Every named preset (lower-case names are accepted by [`Format::parse`]).
pub const PRESETS: [(&str, Format); 13] = [
    ("PCM16_8K", PCM16_8K),
    ("PCM16_16K", PCM16_16K),
    ("PCM16_24K", PCM16_24K),
    ("PCM16_48K", PCM16_48K),
    ("PCM16_48K_STEREO", PCM16_48K_STEREO),
    ("F32_48K", F32_48K),
    ("F32_48K_STEREO", F32_48K_STEREO),
    ("MULAW_8K", MULAW_8K),
    ("ALAW_8K", ALAW_8K),
    ("GPT_LIVE", GPT_LIVE),
    ("OPENAI_REALTIME", OPENAI_REALTIME),
    ("GEMINI_LIVE_INPUT", GEMINI_LIVE_INPUT),
    ("GEMINI_LIVE_OUTPUT", GEMINI_LIVE_OUTPUT),
];

fn bad_spec(spec: &str) -> CoreError {
    CoreError::InvalidAudio(Cow::Owned(format!(
        "unrecognised audio format {spec:?}; expected e.g. 'pcm16_24k', 'pcm16_48k_stereo', 'f32_48k', \
         'mulaw', 'alaw' or 'pcm16le/24000/1'"
    )))
}

impl Format {
    /// Validates the combination (G.711 ⇒ 8 kHz mono).
    pub fn new(rate_hz: u32, channels: u8, encoding: Encoding) -> Result<Self, CoreError> {
        let rate = SampleRate::from_hz(rate_hz).ok_or_else(|| {
            CoreError::InvalidAudio(Cow::Owned(format!(
                "sample_rate must be 8000, 16000, 24000 or 48000 (got {rate_hz})"
            )))
        })?;
        if !(1..=2).contains(&channels) {
            return Err(CoreError::InvalidAudio(Cow::Owned(format!(
                "channels must be 1 or 2 (got {channels})"
            ))));
        }
        if encoding.is_g711() && (rate != SampleRate::Hz8000 || channels != 1) {
            return Err(CoreError::invalid(
                "mulaw/alaw (G.711) require 8000 Hz mono",
            ));
        }
        Ok(Format {
            rate,
            channels,
            encoding,
        })
    }

    /// Parses `'pcm16_24k'`, `'pcm16_48k_stereo'`, `'f32_48k'`, `'mulaw'`, `'alaw'`,
    /// `'pcm16le/24000/1'`, a preset name (`'GPT_LIVE'`), or a MIME type from [`Format::mime_type`].
    pub fn parse(spec: &str) -> Result<Self, CoreError> {
        let s = spec.trim().to_ascii_lowercase();
        if let Some((_, f)) = PRESETS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&s))
        {
            return Ok(*f);
        }
        if let Some(mime) = s.strip_prefix("audio/") {
            return Self::parse_mime(mime).ok_or_else(|| bad_spec(spec));
        }
        if let Some(enc) = Encoding::parse(&s).filter(|e| e.is_g711()) {
            return Format::new(8_000, 1, enc);
        }
        if s.contains('/') {
            let mut parts = s.split('/');
            let enc = parts
                .next()
                .and_then(Encoding::parse)
                .ok_or_else(|| bad_spec(spec))?;
            let rate: u32 = parts
                .next()
                .and_then(|r| r.trim().parse().ok())
                .ok_or_else(|| bad_spec(spec))?;
            let channels: u8 = match parts.next() {
                Some(c) => c.trim().parse().map_err(|_| bad_spec(spec))?,
                None => 1,
            };
            if parts.next().is_some() {
                return Err(bad_spec(spec));
            }
            return Format::new(rate, channels, enc);
        }
        let mut parts = s.split('_');
        let enc = parts
            .next()
            .and_then(Encoding::parse)
            .ok_or_else(|| bad_spec(spec))?;
        let khz: u32 = parts
            .next()
            .and_then(|r| r.strip_suffix('k'))
            .and_then(|r| r.parse().ok())
            .ok_or_else(|| bad_spec(spec))?;
        let channels = match parts.next() {
            None | Some("mono") => 1,
            Some("stereo") => 2,
            Some(_) => return Err(bad_spec(spec)),
        };
        if parts.next().is_some() {
            return Err(bad_spec(spec));
        }
        Format::new(
            khz.checked_mul(1_000).ok_or_else(|| bad_spec(spec))?,
            channels,
            enc,
        )
    }

    fn parse_mime(mime: &str) -> Option<Self> {
        let mut parts = mime.split(';').map(str::trim);
        let base = parts.next()?;
        let (mut rate, mut channels) = (None, 1u8);
        for p in parts {
            let (k, v) = p.split_once('=')?;
            match k.trim() {
                "rate" => rate = v.trim().parse().ok(),
                "channels" => channels = v.trim().parse().ok()?,
                _ => return None,
            }
        }
        match base {
            "pcmu" => Format::new(8_000, 1, Encoding::MuLaw).ok(),
            "pcma" => Format::new(8_000, 1, Encoding::ALaw).ok(),
            "pcm" | "l16" => Format::new(rate?, channels, Encoding::Pcm16Le).ok(),
            "x-f32le" => Format::new(rate?, channels, Encoding::F32Le).ok(),
            _ => None,
        }
    }

    pub const fn sample_width(self) -> usize {
        self.encoding.sample_width()
    }

    pub const fn frame_bytes(self) -> usize {
        self.encoding.sample_width() * self.channels as usize
    }

    pub const fn frames_per_tick(self) -> usize {
        self.rate.frames_per_tick()
    }

    /// Exact bytes per millisecond (48 for PCM16 24 kHz mono).
    pub const fn bytes_per_ms(self) -> usize {
        (self.rate.hz() / 1_000) as usize * self.frame_bytes()
    }

    pub fn frames_for_ms(self, ms: u32) -> usize {
        (self.rate.hz() / 1_000) as usize * ms as usize
    }

    pub fn bytes_for_ms(self, ms: u32) -> Result<usize, LimitError> {
        let b = mul_checked("ms", u64::from(ms), self.bytes_per_ms() as u64)?;
        usize::try_from(b).map_err(|_| LimitError {
            what: "ms",
            value: crate::limits::LimitNum::Int(i128::from(b)),
            min: crate::limits::LimitNum::Int(0),
            max: crate::limits::LimitNum::Int(usize::MAX as i128),
        })
    }

    /// Duration of `frames` frames in whole milliseconds (floor; saturating).
    pub fn ms_for_frames(self, frames: u64) -> u64 {
        frames.saturating_mul(1_000) / u64::from(self.rate.hz())
    }

    /// The byte value of digital silence: 0x00 (PCM), 0xFF (µ-law), 0xD5 (A-law).
    pub fn silence_byte(self) -> u8 {
        match self.encoding {
            Encoding::Pcm16Le | Encoding::F32Le => 0x00,
            Encoding::MuLaw => g711::ULAW_SILENCE,
            Encoding::ALaw => g711::ALAW_SILENCE,
        }
    }

    /// `'audio/pcm;rate=24000'`, `'audio/pcmu'`, `'audio/pcma'`, `'audio/x-f32le;rate=48000'`
    /// (stereo formats append `;channels=2`).
    pub fn mime_type(self) -> Cow<'static, str> {
        let ch = if self.channels == 2 {
            ";channels=2"
        } else {
            ""
        };
        match self.encoding {
            Encoding::MuLaw => Cow::Borrowed("audio/pcmu"),
            Encoding::ALaw => Cow::Borrowed("audio/pcma"),
            Encoding::Pcm16Le => Cow::Owned(format!("audio/pcm;rate={}{ch}", self.rate.hz())),
            Encoding::F32Le => Cow::Owned(format!("audio/x-f32le;rate={}{ch}", self.rate.hz())),
        }
    }

    /// The OpenAI session format object as `(type, rate)`: `("audio/pcm", Some(R))` for mono PCM16
    /// at 16/24 kHz, `("audio/pcmu", None)`, `("audio/pcma", None)`; `None` for anything else.
    pub fn openai_format(self) -> Option<(&'static str, Option<u32>)> {
        match (self.encoding, self.rate, self.channels) {
            (Encoding::Pcm16Le, SampleRate::Hz16000 | SampleRate::Hz24000, 1) => {
                Some(("audio/pcm", Some(self.rate.hz())))
            }
            (Encoding::MuLaw, _, _) => Some(("audio/pcmu", None)),
            (Encoding::ALaw, _, _) => Some(("audio/pcma", None)),
            _ => None,
        }
    }

    /// Appends `ms` of digital silence. `check(SILENCE_MS)` before any arithmetic, then `try_reserve`
    /// (`Alloc` → `MemoryError`).
    pub fn silence(self, ms: u64, out: &mut Vec<u8>) -> Result<(), CoreError> {
        let ms = check("ms", ms, SILENCE_MS)?;
        let n = mul_checked("ms", ms, self.bytes_per_ms() as u64)?;
        let n = usize::try_from(n).map_err(|_| CoreError::Alloc)?;
        out.try_reserve_exact(n)?;
        out.resize(out.len() + n, self.silence_byte());
        Ok(())
    }
}

// ── PcmBuf ───────────────────────────────────────────────────────────────────────────────────────────

/// A byte buffer backed by `u32` words, so typed `i16`/`f32` views are always aligned.
pub struct PcmBuf {
    words: Box<[u32]>,
    len: usize,
}

impl std::fmt::Debug for PcmBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PcmBuf").field("len", &self.len).finish()
    }
}

impl PcmBuf {
    /// `len` zero bytes (`try_reserve`: `Alloc` on failure).
    pub fn zeroed(len: usize) -> Result<Self, CoreError> {
        let n = len.div_ceil(4);
        let mut v: Vec<u32> = Vec::new();
        v.try_reserve_exact(n)?;
        v.resize(n, 0);
        Ok(PcmBuf {
            words: v.into_boxed_slice(),
            len,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        bytemuck::cast_slice::<u32, u8>(&self.words)
            .get(..self.len)
            .unwrap_or(&[])
    }

    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        let len = self.len;
        bytemuck::cast_slice_mut::<u32, u8>(&mut self.words)
            .get_mut(..len)
            .unwrap_or(&mut [])
    }

    /// The whole `i16` samples (aligned view).
    pub fn as_i16(&self) -> &[i16] {
        bytemuck::cast_slice::<u32, i16>(&self.words)
            .get(..self.len / 2)
            .unwrap_or(&[])
    }

    pub fn as_i16_mut(&mut self) -> &mut [i16] {
        let n = self.len / 2;
        bytemuck::cast_slice_mut::<u32, i16>(&mut self.words)
            .get_mut(..n)
            .unwrap_or(&mut [])
    }

    /// The whole `f32` samples (aligned view).
    pub fn as_f32(&self) -> &[f32] {
        bytemuck::cast_slice::<u32, f32>(&self.words)
            .get(..self.len / 4)
            .unwrap_or(&[])
    }

    pub fn as_f32_mut(&mut self) -> &mut [f32] {
        let n = self.len / 4;
        bytemuck::cast_slice_mut::<u32, f32>(&mut self.words)
            .get_mut(..n)
            .unwrap_or(&mut [])
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Start of the payload (4-byte aligned; stable for the buffer's lifetime).
    pub fn as_ptr(&self) -> *const u8 {
        self.words.as_ptr().cast::<u8>()
    }
}

// ── PcmParser / Carry (§7.3) ─────────────────────────────────────────────────────────────────────────

/// Stateless converter from wire bytes to interleaved `i16`. Runs OUTSIDE every lock (§7.3).
#[derive(Clone, Copy, Debug)]
pub struct PcmParser {
    fmt: Format,
}

fn extend_samples(enc: Encoding, src: &[u8], out: &mut Vec<i16>) {
    match enc {
        Encoding::Pcm16Le => {
            let (chunks, _) = src.as_chunks::<2>();
            out.extend(chunks.iter().map(|c| i16::from_le_bytes(*c)));
        }
        Encoding::F32Le => {
            let (chunks, _) = src.as_chunks::<4>();
            out.extend(
                chunks
                    .iter()
                    .map(|c| mix::f32_sample_to_i16(f32::from_le_bytes(*c))),
            );
        }
        Encoding::MuLaw => out.extend(src.iter().map(|b| g711::ulaw_decode(*b))),
        Encoding::ALaw => out.extend(src.iter().map(|b| g711::alaw_decode(*b))),
    }
}

impl PcmParser {
    pub fn new(fmt: Format) -> Self {
        PcmParser { fmt }
    }

    pub fn format(&self) -> Format {
        self.fmt
    }

    /// Bytes needed to complete a carried partial frame of `carry_len` bytes (0 if none).
    pub fn head_len(&self, carry_len: usize) -> usize {
        let fb = self.fmt.frame_bytes();
        (fb - carry_len % fb) % fb
    }

    /// Converts the whole frames of `src` to interleaved i16 (`try_reserve`); returns
    /// `(frames, trailing partial bytes)`. pcm16le is an LE copy, f32le is clamp + round, µ-law/A-law
    /// use the g711 tables. Odd byte counts never fail.
    pub fn parse_frames(
        &self,
        src: &[u8],
        out: &mut Vec<i16>,
    ) -> Result<(usize, usize), CoreError> {
        let fb = self.fmt.frame_bytes();
        let frames = src.len() / fb;
        let trailing = src.len() % fb;
        out.try_reserve_exact(frames * usize::from(self.fmt.channels))?;
        extend_samples(
            self.fmt.encoding,
            src.get(..frames * fb).unwrap_or(&[]),
            out,
        );
        Ok((frames, trailing))
    }

    /// Converts exactly one frame (`frame.len() == frame_bytes`) into `out[..channels]`.
    pub fn convert_frame(&self, frame: &[u8], out: &mut [i16]) {
        let ch = usize::from(self.fmt.channels);
        match self.fmt.encoding {
            Encoding::Pcm16Le => {
                let (chunks, _) = frame.as_chunks::<2>();
                for (o, c) in out.iter_mut().zip(chunks.iter()).take(ch) {
                    *o = i16::from_le_bytes(*c);
                }
            }
            Encoding::F32Le => {
                let (chunks, _) = frame.as_chunks::<4>();
                for (o, c) in out.iter_mut().zip(chunks.iter()).take(ch) {
                    *o = mix::f32_sample_to_i16(f32::from_le_bytes(*c));
                }
            }
            Encoding::MuLaw => {
                for (o, b) in out.iter_mut().zip(frame.iter()).take(ch) {
                    *o = g711::ulaw_decode(*b);
                }
            }
            Encoding::ALaw => {
                for (o, b) in out.iter_mut().zip(frame.iter()).take(ch) {
                    *o = g711::alaw_decode(*b);
                }
            }
        }
    }
}

/// The partial-frame carry of one stream: the only state under L6w (§7.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Carry {
    bytes: [u8; 8],
    len: u8,
    generation: u64,
}

impl Carry {
    pub const fn new() -> Self {
        Carry {
            bytes: [0; 8],
            len: 0,
            generation: 0,
        }
    }

    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The carried bytes.
    pub fn bytes(&self) -> &[u8] {
        self.bytes.get(..self.len()).unwrap_or(&[])
    }

    /// Completes the carried frame with `head` (≤ 7 bytes; at most `head_len(len)` are used) into
    /// `out[..channels]`. Returns `true` iff a complete frame was produced (the carry is then empty).
    /// Returns `false` if no frame was carried, or if carry + head is still partial (then `head` is
    /// appended to the carry).
    pub fn splice(&mut self, p: &PcmParser, head: &[u8], out: &mut [i16]) -> bool {
        if self.len == 0 {
            return false;
        }
        let fb = p.format().frame_bytes().min(self.bytes.len());
        let have = self.len();
        let take = head.len().min(fb.saturating_sub(have));
        if let (Some(dst), Some(src)) = (self.bytes.get_mut(have..have + take), head.get(..take)) {
            dst.copy_from_slice(src);
        }
        self.len = u8::try_from(have + take).unwrap_or(0);
        if self.len() < fb {
            return false;
        }
        p.convert_frame(self.bytes.get(..fb).unwrap_or(&[]), out);
        self.len = 0;
        true
    }

    /// Stores a trailing partial frame (at most 7 bytes are kept: the last ones) with its generation.
    pub fn set(&mut self, tail: &[u8], generation: u64) {
        let n = tail.len().min(self.bytes.len() - 1);
        let src = tail.get(tail.len() - n..).unwrap_or(&[]);
        if let Some(dst) = self.bytes.get_mut(..n) {
            dst.copy_from_slice(src);
        }
        self.len = u8::try_from(n).unwrap_or(0);
        self.generation = generation;
    }

    pub fn reset(&mut self) {
        self.len = 0;
    }

    /// The generation the carried bytes belong to (`gen` is reserved in edition 2024, hence `r#gen`).
    pub fn r#gen(&self) -> u64 {
        self.generation
    }

    /// Alias of [`Carry::r#gen`].
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Drops the carry if it belongs to another generation (a `clear()` happened: pre-clear bytes
    /// must never prefix post-clear audio). Returns `true` if bytes were dropped.
    pub fn sync_gen(&mut self, generation: u64) -> bool {
        if self.generation == generation {
            return false;
        }
        let dropped = self.len != 0;
        self.len = 0;
        self.generation = generation;
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::LimitNum;

    fn all_formats() -> Vec<Format> {
        let mut v = Vec::new();
        for r in SampleRate::ALL {
            for ch in 1..=2u8 {
                for e in [
                    Encoding::Pcm16Le,
                    Encoding::F32Le,
                    Encoding::MuLaw,
                    Encoding::ALaw,
                ] {
                    if let Ok(f) = Format::new(r.hz(), ch, e) {
                        v.push(f);
                    }
                }
            }
        }
        v
    }

    #[test]
    fn new_accepts_exactly_the_valid_combinations() {
        let mut valid = 0;
        for hz in [
            0, 7_999, 8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 44_100, 48_000, 96_000,
        ] {
            for ch in 0..=3u8 {
                for e in [
                    Encoding::Pcm16Le,
                    Encoding::F32Le,
                    Encoding::MuLaw,
                    Encoding::ALaw,
                ] {
                    let ok = Format::new(hz, ch, e).is_ok();
                    let rate_ok = SampleRate::from_hz(hz).is_some();
                    let expect = rate_ok
                        && (1..=2).contains(&ch)
                        && (!e.is_g711() || (hz == 8_000 && ch == 1));
                    assert_eq!(ok, expect, "{hz} {ch} {e:?}");
                    valid += usize::from(ok);
                }
            }
        }
        assert_eq!(valid, 4 * 2 * 2 + 2);
        assert_eq!(all_formats().len(), 18);
        assert!(matches!(
            Format::new(16_000, 1, Encoding::MuLaw),
            Err(CoreError::InvalidAudio(_))
        ));
    }

    #[test]
    fn parse_specs() {
        let ok = [
            ("pcm16_24k", PCM16_24K),
            ("PCM16_24K", PCM16_24K),
            ("pcm16_48k_stereo", PCM16_48K_STEREO),
            ("pcm16_48k_mono", PCM16_48K),
            ("f32_48k", F32_48K),
            ("f32le_48k_stereo", F32_48K_STEREO),
            ("mulaw", MULAW_8K),
            ("alaw", ALAW_8K),
            ("mulaw_8k", MULAW_8K),
            ("g711_ulaw", MULAW_8K),
            ("pcm16le/24000/1", PCM16_24K),
            ("pcm16le/48000/2", PCM16_48K_STEREO),
            ("f32le/48000", F32_48K),
            ("alaw/8000/1", ALAW_8K),
            ("  Pcm16_16k ", PCM16_16K),
            ("gpt_live", GPT_LIVE),
            ("GEMINI_LIVE_INPUT", PCM16_16K),
            ("audio/pcm;rate=24000", PCM16_24K),
            ("audio/pcmu", MULAW_8K),
            ("audio/x-f32le;rate=48000;channels=2", F32_48K_STEREO),
        ];
        for (s, f) in ok {
            assert_eq!(Format::parse(s).unwrap(), f, "{s}");
        }
        let bad = [
            "",
            "pcm16",
            "pcm16_12k",
            "pcm16_44.1k",
            "pcm16_24",
            "pcm16_24k_quad",
            "pcm16_24k_stereo_x",
            "mulaw_16k",
            "alaw/8000/2",
            "pcm16le/24000/3",
            "pcm16le/24000/1/1",
            "opus/48000/2",
            "wav",
            "audio/ogg",
            "audio/pcm",
            "pcm16_99999999999k",
        ];
        for s in bad {
            assert!(
                matches!(Format::parse(s), Err(CoreError::InvalidAudio(_))),
                "{s}"
            );
        }
    }

    #[test]
    fn mime_round_trips() {
        for f in all_formats() {
            assert_eq!(
                Format::parse(&f.mime_type()).unwrap(),
                f,
                "{}",
                f.mime_type()
            );
        }
        assert_eq!(PCM16_24K.mime_type(), "audio/pcm;rate=24000");
        assert_eq!(MULAW_8K.mime_type(), "audio/pcmu");
        assert_eq!(ALAW_8K.mime_type(), "audio/pcma");
        assert_eq!(F32_48K.mime_type(), "audio/x-f32le;rate=48000");
    }

    #[test]
    fn sizes() {
        assert_eq!(PCM16_24K.bytes_per_ms(), 48);
        assert_eq!(PCM16_24K.frame_bytes(), 2);
        assert_eq!(F32_48K_STEREO.frame_bytes(), 8);
        assert_eq!(F32_48K_STEREO.bytes_per_ms(), 384);
        assert_eq!(MULAW_8K.bytes_per_ms(), 8);
        assert_eq!(PCM16_24K.frames_per_tick(), 480);
        assert_eq!(PCM16_24K.frames_for_ms(20), 480);
        assert_eq!(PCM16_24K.bytes_for_ms(1_000).unwrap(), 48_000);
        assert_eq!(
            F32_48K_STEREO.bytes_for_ms(u32::MAX).unwrap(),
            384 * u32::MAX as usize
        );
        assert_eq!(PCM16_24K.ms_for_frames(480), 20);
        assert_eq!(PCM16_24K.ms_for_frames(479), 19);
        assert_eq!(PCM16_8K.ms_for_frames(u64::MAX), u64::MAX / 8_000);
        for r in SampleRate::ALL {
            assert_eq!(r.frames_per_tick() * 50, r.hz() as usize);
            assert_eq!(SampleRate::from_hz(r.hz()), Some(r));
        }
    }

    #[test]
    fn openai_formats() {
        assert_eq!(PCM16_24K.openai_format(), Some(("audio/pcm", Some(24_000))));
        assert_eq!(PCM16_16K.openai_format(), Some(("audio/pcm", Some(16_000))));
        assert_eq!(MULAW_8K.openai_format(), Some(("audio/pcmu", None)));
        assert_eq!(ALAW_8K.openai_format(), Some(("audio/pcma", None)));
        assert_eq!(PCM16_48K.openai_format(), None);
        assert_eq!(F32_48K.openai_format(), None);
        assert_eq!(
            Format::new(24_000, 2, Encoding::Pcm16Le)
                .unwrap()
                .openai_format(),
            None
        );
    }

    #[test]
    fn silence_bytes_and_limits() {
        let mut v = Vec::new();
        MULAW_8K.silence(10, &mut v).unwrap();
        assert_eq!(v, vec![0xFF; 80]);
        v.clear();
        ALAW_8K.silence(1, &mut v).unwrap();
        assert_eq!(v, vec![0xD5; 8]);
        v.clear();
        PCM16_24K.silence(20, &mut v).unwrap();
        assert_eq!(v, vec![0; 960]);
        v.clear();
        PCM16_8K.silence(600_000, &mut v).unwrap();
        assert_eq!(v.len(), 600_000 * 16);
        let mut v = Vec::new();
        let e = PCM16_48K.silence(600_001, &mut v).unwrap_err();
        match e {
            CoreError::Limit(l) => {
                assert_eq!(l.what, "ms");
                assert_eq!(l.max, LimitNum::Int(600_000));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            v.is_empty() && v.capacity() == 0,
            "nothing allocated on a limit error"
        );
        assert!(PCM16_48K.silence(1_000_000_000_000, &mut v).is_err());
    }

    #[test]
    fn pcmbuf_alignment_and_views() {
        let mut b = PcmBuf::zeroed(10).unwrap();
        assert_eq!(b.len(), 10);
        assert_eq!(b.as_ptr() as usize % 4, 0);
        assert_eq!(b.as_bytes(), &[0u8; 10]);
        b.as_mut_bytes()[0] = 1;
        assert_eq!(b.as_i16().len(), 5);
        assert_eq!(b.as_i16()[0], 1);
        assert_eq!(b.as_f32().len(), 2);
        b.as_f32_mut()[1] = 0.5;
        assert_eq!(b.as_f32()[1], 0.5);
        b.as_i16_mut()[4] = -2;
        assert_eq!(&b.as_bytes()[8..10], &(-2i16).to_le_bytes());
        let e = PcmBuf::zeroed(0).unwrap();
        assert!(e.is_empty() && e.as_bytes().is_empty());
        assert!(matches!(PcmBuf::zeroed(usize::MAX), Err(CoreError::Alloc)));
    }

    fn encode_samples(f: Format, samples: &[i16]) -> Vec<u8> {
        let mut out = Vec::new();
        for &s in samples {
            match f.encoding {
                Encoding::Pcm16Le => out.extend_from_slice(&s.to_le_bytes()),
                Encoding::F32Le => out.extend_from_slice(&(f32::from(s) / 32768.0).to_le_bytes()),
                Encoding::MuLaw => out.push(g711::ulaw_encode(s)),
                Encoding::ALaw => out.push(g711::alaw_encode(s)),
            }
        }
        out
    }

    /// The W4 write protocol in miniature: splice the carry, parse whole frames, keep the tail.
    fn write(p: &PcmParser, carry: &mut Carry, data: &[u8], generation: u64, out: &mut Vec<i16>) {
        carry.sync_gen(generation);
        let k = p.head_len(carry.len());
        let mut rest = data;
        if !carry.is_empty() {
            let mut frame = [0i16; 2];
            let head = &data[..k.min(data.len())];
            if carry.splice(p, head, &mut frame) {
                out.extend_from_slice(&frame[..usize::from(p.format().channels)]);
            } else {
                return; // still partial: everything went into the carry
            }
            rest = &data[k..];
        }
        let (_, trailing) = p.parse_frames(rest, out).unwrap();
        carry.set(&rest[rest.len() - trailing..], generation);
    }

    #[test]
    fn carry_across_one_byte_writes_for_every_format() {
        for f in all_formats() {
            let samples: Vec<i16> = (0..(37 * usize::from(f.channels)))
                .map(|i| ((i as i32 * 977) % 65_536 - 32_768) as i16)
                .collect();
            let bytes = encode_samples(f, &samples);
            let p = PcmParser::new(f);
            let mut expected = Vec::new();
            let (frames, trailing) = p.parse_frames(&bytes, &mut expected).unwrap();
            assert_eq!((frames, trailing), (37, 0));
            for step in [1usize, 2, 3, 5, 7, 11] {
                let mut carry = Carry::new();
                let mut got = Vec::new();
                for chunk in bytes.chunks(step) {
                    write(&p, &mut carry, chunk, 7, &mut got);
                }
                assert!(carry.is_empty(), "{f:?} step {step}");
                assert_eq!(got, expected, "{f:?} step {step}");
            }
        }
    }

    #[test]
    fn stale_generation_carry_is_dropped() {
        let p = PcmParser::new(PCM16_48K_STEREO);
        let mut carry = Carry::new();
        let mut out = Vec::new();
        write(&p, &mut carry, &[1, 2, 3], 1, &mut out);
        assert_eq!(carry.len(), 3);
        assert_eq!(carry.r#gen(), 1);
        assert_eq!(carry.generation(), 1);
        // A clear() bumped the generation: the 3 stale bytes must not prefix new audio.
        let frame: Vec<u8> = [100i16, -100]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        write(&p, &mut carry, &frame, 2, &mut out);
        assert_eq!(out, vec![100, -100]);
        assert!(carry.is_empty());
        assert!(!carry.sync_gen(2));
    }

    #[test]
    fn f32_clamp_and_round() {
        let p = PcmParser::new(F32_48K);
        let vals = [
            1.5f32,
            -1.5,
            1.0,
            -1.0,
            0.5,
            0.25 / 32768.0,
            0.75 / 32768.0,
            f32::NAN,
            f32::INFINITY,
            -0.0,
        ];
        let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut out = Vec::new();
        p.parse_frames(&bytes, &mut out).unwrap();
        assert_eq!(
            out,
            vec![32_767, -32_768, 32_767, -32_768, 16_384, 0, 1, 0, 32_767, 0]
        );
    }

    #[test]
    fn odd_byte_counts_never_fail() {
        let p = PcmParser::new(PCM16_24K);
        let mut out = Vec::new();
        assert_eq!(p.parse_frames(&[1, 0, 2], &mut out).unwrap(), (1, 1));
        assert_eq!(out, vec![1]);
        assert_eq!(p.head_len(0), 0);
        assert_eq!(p.head_len(1), 1);
        let p8 = PcmParser::new(F32_48K_STEREO);
        assert_eq!(p8.head_len(3), 5);
        assert_eq!(p8.head_len(7), 1);
        let mut c = Carry::new();
        c.set(&[9; 12], 0);
        assert_eq!(c.len(), 7, "at most 7 bytes are kept");
    }
}
