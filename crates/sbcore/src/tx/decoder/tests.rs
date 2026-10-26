//! `LiveReader` + `PcmDecoder` driven exactly as songbird's `mix_symph_indiv` does
//! (`next_packet` → `decode`), on the test thread (§13.1 `tx::reader`/`decoder`).

use std::sync::atomic::{AtomicU8, AtomicU64};

use songbird::input::core::audio::{AudioBufferRef, Signal};
use songbird::input::core::formats::{SeekMode, SeekTo};
use songbird::input::core::io::ReadBytes;
use songbird::input::core::units::Time;
use songbird::input::{Input, LiveInput, Parsed};

use super::*;
use crate::format::{Format, PCM16_8K, PCM16_16K, PCM16_24K, PCM16_48K, PCM16_48K_STEREO};
use crate::runtime::{Role, role, set_role};
use crate::tracks::flags::{DoneGuard, EOF_SEEN, TrackFlags, TrackKind};
use crate::tx::queue::tests::{Rig, spec};
use crate::tx::queue::{ClearOpts, PcmQueue};
use crate::tx::reader::live_pcm_input;
use crate::tx::{CloseMode, LiveSource, PullReport};

pub(crate) fn guard() -> (DoneGuard, Arc<TrackFlags>) {
    let flags = Arc::new(TrackFlags::new(
        1,
        TrackKind::Playback,
        Arc::new(AtomicBool::new(false)),
    ));
    (DoneGuard::new(flags.clone()), flags)
}

pub(crate) fn parsed(input: Input) -> Parsed {
    match input {
        Input::Live(LiveInput::Parsed(p), None) => p,
        _ => panic!("expected a parsed live input"),
    }
}

/// The songbird side of one track.
pub(crate) struct Mixer {
    pub(crate) p: Parsed,
}

impl Mixer {
    pub(crate) fn new(q: Arc<PcmQueue>) -> (Mixer, Arc<TrackFlags>) {
        let (done, flags) = guard();
        let p = parsed(live_pcm_input(q, done, false).unwrap());
        (Mixer { p }, flags)
    }

    /// One tick; `None` on EOF. Returns interleaved 48 kHz samples.
    pub(crate) fn tick(&mut self) -> Option<Vec<f32>> {
        let pkt = self.p.format.next_packet().ok()?;
        assert!(pkt.buf().is_empty(), "the packet buffer is empty");
        assert_eq!(pkt.dur, 960);
        let out = self.p.decoder.decode(&pkt).unwrap();
        assert_eq!(out.frames(), 960);
        assert_eq!(out.spec().rate, 48_000);
        let AudioBufferRef::F32(b) = out else {
            panic!("f32 output expected")
        };
        let ch = b.spec().channels.count();
        let mut v = Vec::with_capacity(960 * ch);
        for f in 0..960 {
            for c in 0..ch {
                v.push(b.chan(c)[f]);
            }
        }
        Some(v)
    }

    /// One tick without collecting the output (allocation-free).
    pub(crate) fn tick_raw(&mut self) -> bool {
        match self.p.format.next_packet() {
            Ok(pkt) => {
                let _ = self.p.decoder.decode(&pkt);
                true
            }
            Err(_) => false,
        }
    }
}

fn dc(fmt: Format, value: i16, ms: u32) -> Vec<u8> {
    let n = fmt.frames_for_ms(ms) * usize::from(fmt.channels);
    (0..n).flat_map(|_| value.to_le_bytes()).collect()
}

fn sine(fmt: Format, hz: f64, amp: f64, ms: u32) -> Vec<u8> {
    let rate = f64::from(fmt.rate.hz());
    let frames = fmt.frames_for_ms(ms);
    let mut v = Vec::new();
    for i in 0..frames {
        let s = (amp * (2.0 * std::f64::consts::PI * hz * i as f64 / rate).sin() * 32_767.0) as i16;
        for _ in 0..fmt.channels {
            v.extend_from_slice(&s.to_le_bytes());
        }
    }
    v
}

#[test]
fn always_960_frames_for_every_rate_and_channel_count() {
    for fmt in [PCM16_8K, PCM16_16K, PCM16_24K, PCM16_48K, PCM16_48K_STEREO] {
        for ch in [1u8, 2] {
            let fmt = Format::new(fmt.rate.hz(), ch, fmt.encoding).unwrap();
            let r = Rig::new(spec(fmt));
            r.write(&sine(fmt, 1_000.0, 0.5, 200)).unwrap();
            let (mut m, _) = Mixer::new(r.q.clone());
            let mut energy = 0.0f64;
            for t in 0..15 {
                let out = m.tick().unwrap();
                assert_eq!(out.len(), 960 * usize::from(ch));
                if (2..9).contains(&t) {
                    energy +=
                        out.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / out.len() as f64;
                }
            }
            // 1 kHz at 0.5 FS → RMS² = 0.125 per tick, after resampling too (±0.2 dB).
            let db = 10.0 * (energy / 7.0 / 0.125).log10();
            assert!(db.abs() < 0.2, "{fmt:?}: {db} dB");
        }
    }
}

#[test]
fn zero_allocations_per_pull() {
    for fmt in [PCM16_24K, PCM16_48K_STEREO, PCM16_8K] {
        let r = Rig::new(spec(fmt));
        for i in 0..30 {
            r.write_at(
                &sine(fmt, 440.0, 0.3, 20),
                Some(&format!("t{}", i % 4)),
                None,
            )
            .unwrap();
        }
        let (mut m, _) = Mixer::new(r.q.clone());
        assert!(m.tick_raw());
        r.q.set_volume(0.5).unwrap();
        let g = crate::alloc_count::CountGuard::new();
        let before = crate::alloc_count::count();
        for _ in 0..40 {
            // 29 ticks of audio, then underrun ticks.
            assert!(m.tick_raw());
        }
        let mid = crate::alloc_count::count();
        drop(g);
        r.q.clear(ClearOpts::default()).unwrap();
        r.write(&sine(fmt, 440.0, 0.3, 100)).unwrap();
        let g = crate::alloc_count::CountGuard::new();
        let mid2 = crate::alloc_count::count();
        for _ in 0..5 {
            assert!(m.tick_raw());
        }
        let after = crate::alloc_count::count();
        drop(g);
        assert_eq!(mid, before, "{fmt:?}: pulls allocated");
        assert_eq!(after, mid2, "{fmt:?}: pulls after clear allocated");
    }
}

#[test]
fn underrun_is_silence_and_counted() {
    let mut r = Rig::new(spec(PCM16_24K));
    r.write(&dc(PCM16_24K, 8_000, 20)).unwrap();
    let (mut m, _) = Mixer::new(r.q.clone());
    let out = m.tick().unwrap();
    assert!(out[500].abs() > 0.1);
    r.house();
    let out = m.tick().unwrap();
    // The resampler tail (≤ 4.3 ms) may still carry the end of the audio; the rest is silent.
    assert!(out[400..].iter().all(|s| s.abs() < 1e-3));
    assert!(r.q.stats().underrun_ms >= 20);
}

#[test]
fn held_lock_gives_silence_and_counts_contention() {
    let r = Rig::new(spec(PCM16_48K));
    r.write(&dc(PCM16_48K, 8_000, 100)).unwrap();
    let (mut m, _) = Mixer::new(r.q.clone());
    let out = r.q.with_inner_locked(|| m.tick().unwrap());
    assert!(out.iter().all(|s| *s == 0.0));
    assert_eq!(r.q.stats().contended_ticks, 1);
    assert!(m.tick().unwrap()[500] > 0.2);
}

#[test]
fn send_muted_holds_the_position() {
    let r = Rig::new(spec(PCM16_48K));
    r.write(&dc(PCM16_48K, 8_000, 60)).unwrap();
    let (mut m, _) = Mixer::new(r.q.clone());
    r.muted.store(true, std::sync::atomic::Ordering::Release);
    for _ in 0..3 {
        assert!(m.tick().unwrap().iter().all(|s| *s == 0.0));
    }
    assert_eq!(r.q.stats().played_ms, 0);
    assert_eq!(r.q.stats().buffered_ms, 60);
    r.muted.store(false, std::sync::atomic::Ordering::Release);
    m.tick().unwrap();
    assert_eq!(r.q.stats().played_ms, 20);
}

#[test]
fn eof_only_after_drain_and_empty_or_abort() {
    let r = Rig::new(spec(PCM16_48K));
    r.write(&dc(PCM16_48K, 1_000, 40)).unwrap();
    let (mut m, flags) = Mixer::new(r.q.clone());
    for _ in 0..5 {
        assert!(m.tick().is_some(), "underrun is not EOF");
    }
    r.write(&dc(PCM16_48K, 1_000, 40)).unwrap();
    LiveSource::close(&*r.q, CloseMode::Drain);
    assert!(m.tick().is_some());
    assert!(m.tick().is_some());
    assert!(m.tick().is_none(), "EOF once drained");
    assert_ne!(flags.hints() & EOF_SEEN, 0);

    let r = Rig::new(spec(PCM16_48K));
    r.write(&dc(PCM16_48K, 1_000, 400)).unwrap();
    let (mut m, _) = Mixer::new(r.q.clone());
    assert!(m.tick().is_some());
    LiveSource::close(&*r.q, CloseMode::Abort);
    assert!(m.tick().is_none(), "EOF at once on abort");
}

#[test]
fn declick_after_clear() {
    let r = Rig::new(spec(PCM16_48K));
    r.write(&dc(PCM16_48K, 16_384, 100)).unwrap();
    let (mut m, _) = Mixer::new(r.q.clone());
    let out = m.tick().unwrap();
    assert!((out[10] - 0.5).abs() < 1e-3);
    r.q.clear(ClearOpts::default()).unwrap();
    r.write(&dc(PCM16_48K, 16_384, 100)).unwrap();
    let out = m.tick().unwrap();
    assert!(out[0] < 0.01, "{}", out[0]);
    assert!(out[60] > 0.2 && out[60] < 0.3, "{}", out[60]);
    assert!((out[119] - 0.5).abs() < 1e-3);
    assert!((out[300] - 0.5).abs() < 1e-3);
}

#[test]
fn volume_ramps_over_5_ms() {
    let r = Rig::new(spec(PCM16_48K));
    r.write(&dc(PCM16_48K, 16_384, 200)).unwrap();
    let (mut m, _) = Mixer::new(r.q.clone());
    m.tick().unwrap();
    r.q.set_volume(2.0).unwrap();
    let out = m.tick().unwrap();
    assert!(out[0] > 0.5 && out[0] < 0.51, "{}", out[0]);
    assert!((out[120] - 0.75).abs() < 0.01, "{}", out[120]);
    assert!((out[239] - 1.0).abs() < 1e-4, "{}", out[239]);
    assert!((out[500] - 1.0).abs() < 1e-4);
    assert!(r.q.set_volume(4.5).is_err());
}

/// A provider whose pull panics (§5.7 containment).
struct Panicky {
    flags: AtomicU8,
}

impl PcmProvider for Panicky {
    fn format(&self) -> Format {
        PCM16_48K
    }
    fn pull(&self, _out: &mut [i16]) -> PullReport {
        panic!("boom (expected by the test)");
    }
    fn should_end(&self) -> bool {
        false
    }
    fn generation(&self) -> u64 {
        0
    }
    fn volume(&self) -> f32 {
        1.0
    }
    fn on_poison(&self) {
        self.flags.fetch_or(1, std::sync::atomic::Ordering::AcqRel);
    }
    fn on_reader_drop(&self) {
        self.flags.fetch_or(2, std::sync::atomic::Ordering::AcqRel);
    }
}

#[test]
fn a_panic_in_pull_gives_silence_then_eof() {
    set_role(Role::Harness);
    let p = Arc::new(Panicky {
        flags: AtomicU8::new(0),
    });
    let (done, _) = guard();
    let mut m = Mixer {
        p: parsed(live_pcm_input(p.clone(), done, false).unwrap()),
    };
    let out = m.tick().unwrap();
    assert!(out.iter().all(|s| *s == 0.0));
    assert_eq!(p.flags.load(std::sync::atomic::Ordering::Acquire) & 1, 1);
    assert!(m.tick().is_none());
    drop(m);
    assert_eq!(p.flags.load(std::sync::atomic::Ordering::Acquire) & 2, 2);
}

#[test]
fn drop_sets_reader_gone_and_into_inner_returns_the_probe_stream() {
    let r = Rig::new(spec(PCM16_24K));
    let (m, _) = Mixer::new(r.q.clone());
    assert!(!r.q.reader_gone());
    let Mixer { p } = m;
    let Parsed {
        format, decoder, ..
    } = p;
    drop(decoder);
    assert!(r.q.reader_gone());
    let mss = format.into_inner();
    assert_eq!(mss.pos(), 16);
}

#[test]
fn readers_mark_the_thread_as_mixer() {
    let r = Rig::new(spec(PCM16_24K));
    let q = r.q.clone();
    let res = std::thread::spawn(move || {
        let (mut m, _) = Mixer::new(q);
        assert_eq!(role(), Role::Unknown);
        m.tick_raw();
        let role_after = role();
        let m2 = parking_lot::Mutex::new(());
        let lock_panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = crate::sync::lock_checked(&m2);
        }))
        .is_err();
        (role_after, lock_panicked)
    })
    .join()
    .unwrap();
    assert_eq!(res.0, Role::Mixer);
    assert_eq!(res.1, cfg!(debug_assertions));
}

#[test]
fn memory_pcm_seeks_and_resets_the_resampler() {
    set_role(Role::Harness);
    let samples: Vec<i16> = (0..48_000).map(|i| (i % 20_000) as i16).collect();
    let mem = Arc::new(crate::tx::memory::MemPcm::from_i16(PCM16_48K, samples.clone()).unwrap());
    let (done, flags) = guard();
    let mut m = Mixer {
        p: parsed(mem.to_input(done).unwrap()),
    };
    for _ in 0..3 {
        m.tick().unwrap();
    }
    let seeked =
        m.p.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::from(0.5),
                    track_id: None,
                },
            )
            .unwrap();
    assert_eq!(seeked.actual_ts, 24_000);
    m.p.decoder.reset();
    let out = m.tick().unwrap();
    let expect = f32::from(samples[24_000 + 300]) / 32_768.0;
    assert!((out[300] - expect).abs() < 1e-6, "{} vs {expect}", out[300]);
    // Plays to the end, then EOF.
    let mut ticks = 1;
    while m.tick().is_some() {
        ticks += 1;
    }
    assert_eq!(ticks, 25);
    assert_ne!(flags.hints() & EOF_SEEN, 0);
    let _ = AtomicU64::new(0);
}
