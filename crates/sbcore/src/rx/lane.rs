//! Lanes (W3, §8.7): one encoder + chunk accumulator per `(SourceKey, Format, chunk_ms)`, emitting
//! `Arc<ChunkData>` at timeline-aligned boundaries; every subscriber of a lane gets the same `Arc`.
//!
//! A chunk starts where its first sample's position on the connection timeline is ≡ 0 mod
//! `chunk_frames` (`chunk_frames = rate · chunk_ms / 1000`, an integer for every supported rate and
//! `chunk_ms`), so boundaries of lanes with the same `chunk_ms` coincide and may fall inside a tick
//! (e.g. 10 or 30 ms chunks).

use std::sync::Arc;
use std::time::Instant;

use smallvec::SmallVec;

use super::chunk::{ChunkData, ChunkMeta};
use super::convert::SourceKey;
use crate::dsp::level::LEVEL_FLOOR_DBFS;
use crate::dsp::{Level, encode_from_f32};
use crate::error::CoreError;
use crate::format::{Format, PcmBuf};
use crate::limits::{CHUNK_MS, check};

/// Per-tick metadata a lane folds into its chunk (§8.7 `ChunkMeta`).
///
/// Not named in §12.4 (which only references it from `Lane::feed`); see `docs/deviations/W3.md`.
#[derive(Clone, Debug)]
pub struct TickMeta {
    /// Any voiced audio in this tick (per-user: that user; mixed: any selected user).
    pub voiced: bool,
    /// The tick (or part of it) came from packet-loss concealment.
    pub concealed: bool,
    /// A synthetic (silence-filled) tick: counted in `gap_ms`.
    pub synthetic: bool,
    /// Voiced users contributing to this tick.
    pub speakers: SmallVec<[u64; 8]>,
    /// Arrival of the tick.
    pub captured_at: Instant,
}

/// Metadata accumulated over the chunk in progress.
#[derive(Debug, Default)]
struct ChunkAcc {
    voiced_frames: u64,
    concealed_frames: u64,
    gap_frames: u64,
    speakers: SmallVec<[u64; 8]>,
    sum_sq: f64,
    peak: f32,
    samples: u64,
    captured_at: Option<Instant>,
}

impl ChunkAcc {
    fn level(&self) -> Level {
        if self.samples == 0 || self.sum_sq <= 0.0 {
            return Level {
                rms_dbfs: LEVEL_FLOOR_DBFS,
                peak: self.peak,
            };
        }
        let rms = (self.sum_sq / self.samples as f64).sqrt();
        Level {
            rms_dbfs: ((20.0 * rms.log10()) as f32).max(LEVEL_FLOOR_DBFS),
            peak: self.peak,
        }
    }
}

/// One lane.
#[derive(Debug)]
pub struct Lane {
    key: SourceKey,
    fmt: Format,
    chunk_ms: u32,
    chunk_frames: usize,
    chunk_bytes: usize,
    /// 4-byte-aligned accumulator of the chunk in progress (`None` after a failed allocation).
    acc: Option<PcmBuf>,
    meta: ChunkAcc,
    /// Timeline sample index of the chunk in progress.
    chunk_start: u64,
    filled_frames: usize,
    /// The chunk in progress has no buffer (allocation failed): it is dropped at its boundary.
    lost: bool,
    /// The next tick this lane expects (contiguity check).
    next_tick: u64,
    chunks: u64,
    alloc_failures: u64,
    realigns: u64,
}

impl Lane {
    /// A lane whose first chunk starts at the first timeline-aligned boundary at or after the
    /// first sample of `start_tick` (§8.7: "a new lane starts at the next aligned boundary").
    pub fn new(
        key: SourceKey,
        fmt: Format,
        chunk_ms: u32,
        start_tick: u64,
    ) -> Result<Self, CoreError> {
        check("chunk_ms", chunk_ms, CHUNK_MS)?;
        let chunk_frames = fmt.frames_for_ms(chunk_ms);
        if chunk_frames == 0 {
            return Err(CoreError::invalid("empty chunk"));
        }
        let chunk_bytes = chunk_frames
            .checked_mul(fmt.frame_bytes())
            .ok_or(CoreError::Alloc)?;
        let acc = PcmBuf::zeroed(chunk_bytes)?;
        let cf = chunk_frames as u64;
        let first = start_tick.saturating_mul(fmt.frames_per_tick() as u64);
        Ok(Lane {
            key,
            fmt,
            chunk_ms,
            chunk_frames,
            chunk_bytes,
            acc: Some(acc),
            meta: ChunkAcc::default(),
            chunk_start: first.div_ceil(cf).saturating_mul(cf),
            filled_frames: 0,
            lost: false,
            next_tick: start_tick,
            chunks: 0,
            alloc_failures: 0,
            realigns: 0,
        })
    }

    /// A lane whose first chunk is the one **containing** `tick`'s first sample; the part of that
    /// chunk before `tick` is digital silence. Used by the `listen_users` router so a user's first
    /// voiced tick is never cut off by a long `chunk_ms`.
    pub fn covering(
        key: SourceKey,
        fmt: Format,
        chunk_ms: u32,
        tick: u64,
    ) -> Result<Self, CoreError> {
        let mut lane = Lane::new(key, fmt, chunk_ms, tick)?;
        let cf = lane.chunk_frames as u64;
        let first = tick.saturating_mul(fmt.frames_per_tick() as u64);
        lane.chunk_start = (first / cf).saturating_mul(cf);
        let prefix = usize::try_from(first - lane.chunk_start).unwrap_or(0);
        if prefix > 0 {
            let silence = fmt.silence_byte();
            if let Some(dst) = lane
                .acc
                .as_mut()
                .and_then(|a| a.as_mut_bytes().get_mut(..prefix * fmt.frame_bytes()))
            {
                dst.fill(silence);
            }
            lane.filled_frames = prefix;
            lane.meta.samples = (prefix * usize::from(fmt.channels)) as u64;
        }
        Ok(lane)
    }

    pub fn key(&self) -> &SourceKey {
        &self.key
    }

    pub fn format(&self) -> Format {
        self.fmt
    }

    pub fn chunk_ms(&self) -> u32 {
        self.chunk_ms
    }

    /// Bytes of one chunk.
    pub fn chunk_bytes(&self) -> usize {
        self.chunk_bytes
    }

    /// `timeline_ms` of the next chunk this lane will emit (the chunk in progress).
    pub fn next_chunk_start_ms(&self) -> u64 {
        self.sample_ms(self.chunk_start)
    }

    /// Chunks emitted so far.
    pub fn chunks(&self) -> u64 {
        self.chunks
    }

    /// Chunks dropped because their buffer could not be allocated.
    pub fn alloc_failures(&self) -> u64 {
        self.alloc_failures
    }

    /// Times a tick gap forced the lane to drop its partial chunk and realign.
    pub fn realigns(&self) -> u64 {
        self.realigns
    }

    fn sample_ms(&self, sample: u64) -> u64 {
        sample.saturating_mul(1000) / u64::from(self.fmt.rate.hz())
    }

    fn frames_ms(&self, frames: u64) -> u32 {
        u32::try_from(self.sample_ms(frames)).unwrap_or(u32::MAX)
    }

    /// Encodes one converted tick (`rate/50 · channels` interleaved samples); pushes each completed
    /// chunk to `out`.
    ///
    /// Ticks must be fed contiguously. A tick older than expected is ignored; a gap (missed ticks)
    /// drops the partial chunk and realigns to the next boundary.
    pub fn feed(
        &mut self,
        tick: u64,
        converted: &[f32],
        meta: &TickMeta,
        out: &mut Vec<Arc<ChunkData>>,
    ) {
        if tick < self.next_tick {
            return;
        }
        let fpt = self.fmt.frames_per_tick();
        let ch = usize::from(self.fmt.channels);
        let fb = self.fmt.frame_bytes();
        let cf = self.chunk_frames as u64;
        let tick_start = tick.saturating_mul(fpt as u64);
        if tick > self.next_tick {
            // Missed ticks: the partial chunk can never be completed contiguously.
            let resume = tick_start.div_ceil(cf).saturating_mul(cf);
            if self.chunk_start.saturating_add(self.filled_frames as u64) < tick_start {
                self.realigns += 1;
                self.chunk_start = resume;
                self.filled_frames = 0;
                self.meta = ChunkAcc::default();
                self.lost = self.acc.is_none();
            }
        }
        self.next_tick = tick.saturating_add(1);
        // Frames of this tick before the lane's next sample are not part of any chunk.
        let next_sample = self.chunk_start.saturating_add(self.filled_frames as u64);
        let mut pos = usize::try_from(next_sample.saturating_sub(tick_start))
            .unwrap_or(usize::MAX)
            .min(fpt);
        while pos < fpt {
            if self.filled_frames == 0 && self.acc.is_none() {
                match PcmBuf::zeroed(self.chunk_bytes) {
                    Ok(b) => {
                        self.acc = Some(b);
                        self.lost = false;
                    }
                    Err(_) => self.lost = true,
                }
            }
            let take = (fpt - pos).min(self.chunk_frames - self.filled_frames);
            let src = converted.get(pos * ch..(pos + take) * ch).unwrap_or(&[]);
            if let Some(dst) = self.acc.as_mut().and_then(|a| {
                a.as_mut_bytes()
                    .get_mut(self.filled_frames * fb..(self.filled_frames + take) * fb)
            }) {
                if src.len() == take * ch {
                    encode_from_f32(self.fmt, src, dst);
                } else {
                    dst.fill(self.fmt.silence_byte());
                }
            }
            self.accumulate(src, take as u64, meta);
            self.filled_frames += take;
            pos += take;
            if self.filled_frames == self.chunk_frames {
                self.emit(meta.captured_at, out);
                self.chunk_start = self.chunk_start.saturating_add(cf);
                self.filled_frames = 0;
            }
        }
    }

    fn accumulate(&mut self, src: &[f32], frames: u64, meta: &TickMeta) {
        let m = &mut self.meta;
        if m.captured_at.is_none() {
            m.captured_at = Some(meta.captured_at);
        }
        if meta.voiced {
            m.voiced_frames += frames;
        }
        if meta.concealed {
            m.concealed_frames += frames;
        }
        if meta.synthetic {
            m.gap_frames += frames;
        }
        for s in &meta.speakers {
            if !m.speakers.contains(s) {
                m.speakers.push(*s);
            }
        }
        for s in src {
            m.sum_sq += f64::from(*s) * f64::from(*s);
            m.peak = m.peak.max(s.abs());
        }
        m.samples += src.len() as u64;
    }

    fn emit(&mut self, fallback: Instant, out: &mut Vec<Arc<ChunkData>>) {
        let acc = std::mem::take(&mut self.meta);
        let data = self.acc.take();
        let Some(data) = data.filter(|_| !self.lost) else {
            self.alloc_failures += 1;
            self.lost = false;
            return;
        };
        let mut speakers: Vec<u64> = acc.speakers.to_vec();
        speakers.sort_unstable();
        let meta = ChunkMeta {
            format: self.fmt,
            user_id: match self.key {
                SourceKey::User(u) => Some(u),
                SourceKey::Bus(_) => None,
            },
            timeline_ms: self.sample_ms(self.chunk_start),
            frames: u32::try_from(self.chunk_frames).unwrap_or(u32::MAX),
            speaking: acc.voiced_frames > 0,
            voiced_ms: self.frames_ms(acc.voiced_frames),
            concealed_ms: self.frames_ms(acc.concealed_frames),
            gap_ms: self.frames_ms(acc.gap_frames),
            speakers: speakers.into_boxed_slice(),
            level: acc.level(),
            captured_at: acc.captured_at.unwrap_or(fallback),
        };
        self.chunks += 1;
        out.push(Arc::new(ChunkData { meta, data }));
        // The next chunk's buffer is allocated now, off the consumer's path.
        match PcmBuf::zeroed(self.chunk_bytes) {
            Ok(b) => self.acc = Some(b),
            Err(_) => self.acc = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{Encoding, SampleRate};

    fn meta(voiced: bool, synthetic: bool, speakers: &[u64]) -> TickMeta {
        TickMeta {
            voiced,
            concealed: false,
            synthetic,
            speakers: speakers.iter().copied().collect(),
            captured_at: Instant::now(),
        }
    }

    fn all_formats() -> Vec<Format> {
        let mut v = Vec::new();
        for rate in SampleRate::ALL {
            for ch in [1u8, 2] {
                for enc in [Encoding::Pcm16Le, Encoding::F32Le] {
                    v.push(Format::new(rate.hz(), ch, enc).unwrap());
                }
            }
        }
        v.push(Format::new(8_000, 1, Encoding::MuLaw).unwrap());
        v.push(Format::new(8_000, 1, Encoding::ALaw).unwrap());
        v
    }

    /// §13.1: exact chunk sizes for every rate × channels × encoding × chunk_ms, at aligned
    /// boundaries, with per-chunk payload ownership.
    #[test]
    fn exact_chunks_for_every_format_and_chunk_ms() {
        for fmt in all_formats() {
            for chunk_ms in [10u32, 20, 30, 40, 100, 1000] {
                let start_tick = 7; // 140 ms: not aligned for 30/40/100/1000 ms
                let mut lane = Lane::new(SourceKey::User(5), fmt, chunk_ms, start_tick).unwrap();
                let first_ms = (140u64).div_ceil(u64::from(chunk_ms)) * u64::from(chunk_ms);
                assert_eq!(lane.next_chunk_start_ms(), first_ms);
                let n = fmt.frames_per_tick() * usize::from(fmt.channels);
                let tick_pcm: Vec<f32> = (0..n).map(|i| ((i % 7) as f32 - 3.0) * 0.05).collect();
                let mut out = Vec::new();
                let ticks = 150u64;
                for t in start_tick..start_tick + ticks {
                    lane.feed(t, &tick_pcm, &meta(t % 2 == 0, false, &[5]), &mut out);
                }
                let end_ms = (start_tick + ticks) * 20;
                let expect = (end_ms - first_ms) / u64::from(chunk_ms);
                assert_eq!(out.len() as u64, expect, "{fmt:?} {chunk_ms}");
                let want_bytes = fmt.bytes_for_ms(chunk_ms).unwrap();
                for (i, c) in out.iter().enumerate() {
                    assert_eq!(c.data.len(), want_bytes, "{fmt:?} {chunk_ms}");
                    assert_eq!(c.meta.frames as usize, fmt.frames_for_ms(chunk_ms));
                    assert_eq!(
                        c.meta.timeline_ms,
                        first_ms + i as u64 * u64::from(chunk_ms),
                        "aligned boundaries"
                    );
                    assert_eq!(c.meta.timeline_ms % u64::from(chunk_ms), 0);
                    assert_eq!(c.duration_ms(), u64::from(chunk_ms));
                    assert_eq!(c.meta.user_id, Some(5));
                    assert_eq!(c.data.as_ptr() as usize % 4, 0, "4-byte aligned payload");
                    assert!(c.meta.voiced_ms <= chunk_ms);
                    assert_eq!(c.meta.gap_ms, 0);
                }
            }
        }
    }

    #[test]
    fn metadata_accumulates_per_chunk() {
        let fmt = Format::new(16_000, 1, Encoding::Pcm16Le).unwrap();
        let mut lane = Lane::new(SourceKey::Bus(1), fmt, 100, 0).unwrap();
        let pcm = vec![0.5f32; 320];
        let silence = vec![0.0f32; 320];
        let mut out = Vec::new();
        // 5 ticks: voiced(1) synthetic, voiced(2), quiet, voiced(1,3), quiet.
        lane.feed(0, &pcm, &meta(true, true, &[1]), &mut out);
        lane.feed(1, &pcm, &meta(true, false, &[2]), &mut out);
        lane.feed(2, &silence, &meta(false, false, &[]), &mut out);
        lane.feed(3, &pcm, &meta(true, false, &[3, 1]), &mut out);
        lane.feed(4, &silence, &meta(false, false, &[]), &mut out);
        assert_eq!(out.len(), 1);
        let m = &out[0].meta;
        assert_eq!(m.user_id, None);
        assert!(m.speaking);
        assert_eq!(m.voiced_ms, 60);
        assert_eq!(m.gap_ms, 20);
        assert_eq!(&*m.speakers, &[1, 2, 3]);
        assert!((m.level.peak - 0.5).abs() < 1e-6);
        // 3/5 of the samples at 0.5: rms = 0.5·√0.6 → −8.24 dBFS.
        assert!(
            (m.level.rms_dbfs + 8.24).abs() < 0.05,
            "{}",
            m.level.rms_dbfs
        );
        let samples = out[0].data.as_i16();
        assert_eq!(samples.len(), 1600);
        assert_eq!(samples[0], 16_384);
        assert_eq!(samples[700], 0);
    }

    #[test]
    fn covering_lane_prefixes_silence() {
        let fmt = Format::new(8_000, 1, Encoding::MuLaw).unwrap();
        let mut lane = Lane::covering(SourceKey::User(1), fmt, 100, 7).unwrap();
        assert_eq!(lane.next_chunk_start_ms(), 100);
        let pcm = vec![0.25f32; 160];
        let mut out = Vec::new();
        for t in 7..10 {
            lane.feed(t, &pcm, &meta(true, false, &[1]), &mut out);
        }
        assert_eq!(out.len(), 1);
        let b = out[0].data.as_bytes();
        assert_eq!(b.len(), 800);
        assert!(
            b[..320].iter().all(|x| *x == 0xFF),
            "µ-law silence prefix (40 ms)"
        );
        assert!(b[320..].iter().all(|x| *x != 0xFF));
        assert_eq!(out[0].meta.timeline_ms, 100);
        assert_eq!(out[0].meta.voiced_ms, 60);
    }

    #[test]
    fn gaps_realign_and_old_ticks_are_ignored() {
        let fmt = Format::new(48_000, 2, Encoding::F32Le).unwrap();
        let mut lane = Lane::new(SourceKey::User(1), fmt, 40, 0).unwrap();
        let pcm = vec![0.1f32; 1920];
        let mut out = Vec::new();
        lane.feed(0, &pcm, &meta(false, false, &[]), &mut out);
        lane.feed(0, &pcm, &meta(false, false, &[]), &mut out); // duplicate: ignored
        assert!(out.is_empty());
        lane.feed(5, &pcm, &meta(false, false, &[]), &mut out); // gap: partial dropped, realign at 120
        assert_eq!(lane.realigns(), 1);
        lane.feed(6, &pcm, &meta(false, false, &[]), &mut out);
        lane.feed(7, &pcm, &meta(false, false, &[]), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].meta.timeline_ms, 120);
    }
}
