//! Deterministic `PcmQueue` tests (§13.1 `tx::queue`/`writer`/`anchor`/`tags`) on a manual clock.

use super::*;
use crate::clock::ManualClock;
use crate::format::{ALAW_8K, F32_48K_STEREO, MULAW_8K, PCM16_24K, PCM16_48K_STEREO};
use crate::runtime::{Role, set_role};
use crate::wake::{Doorbell, WakeHub};
use std::os::unix::net::UnixStream;

/// A recording `TrackControl` (the sbcore-internal twin of `FakeTrackControl`).
#[derive(Clone, Default)]
pub(crate) struct Rec(pub(crate) Arc<Mutex<Vec<&'static str>>>);

impl TrackControl for Rec {
    fn pause(&self) {
        self.0.lock().push("pause");
    }
    fn play(&self) {
        self.0.lock().push("play");
    }
    fn stop(&self) {
        self.0.lock().push("stop");
    }
}

impl Rec {
    pub(crate) fn calls(&self) -> Vec<&'static str> {
        self.0.lock().clone()
    }
}

/// A wake hub on a socketpair (the read end is kept alive by the caller).
pub(crate) fn test_hub() -> (Arc<WakeHub>, UnixStream) {
    let (r, w) = UnixStream::pair().unwrap();
    (WakeHub::new(Doorbell::from_unix_stream(w).unwrap()), r)
}

pub(crate) fn spec(fmt: Format) -> PlaybackSpec {
    PlaybackSpec {
        format: fmt,
        max_buffer_ms: 4_000,
        prebuffer_ms: 0,
        idle_ms: 250,
        auto_pause: true,
        max_gap_ms: 2_000,
        output_latency_ms: 40,
        volume: 1.0,
        when_disconnected: DisconnectPolicy::Buffer,
    }
}

pub(crate) struct Rig {
    pub(crate) q: Arc<PcmQueue>,
    pub(crate) hub: Arc<WakeHub>,
    pub(crate) clock: Arc<ManualClock>,
    pub(crate) log: Rec,
    pub(crate) muted: Arc<AtomicBool>,
    pub(crate) events: Vec<EventData>,
    _bell: UnixStream,
}

impl Rig {
    pub(crate) fn new(spec: PlaybackSpec) -> Rig {
        set_role(Role::Harness);
        let (hub, r) = test_hub();
        let (clock, handle) = Clock::manual();
        let muted = Arc::new(AtomicBool::new(false));
        let q = PcmQueue::new(7, &spec, hub.alloc().unwrap(), clock, muted.clone()).unwrap();
        let log = Rec::default();
        q.attach_track(Box::new(log.clone()));
        Rig {
            q,
            hub,
            clock: handle,
            log,
            muted,
            events: Vec::new(),
            _bell: r,
        }
    }

    pub(crate) fn fmt(&self) -> Format {
        self.q.fmt
    }

    pub(crate) fn write(&self, b: &[u8]) -> Result<WriteOutcome, CoreError> {
        self.q.write(b, WriteMeta::default())
    }

    pub(crate) fn write_at(
        &self,
        b: &[u8],
        tag: Option<&str>,
        at_ms: Option<i64>,
    ) -> Result<WriteOutcome, CoreError> {
        self.q.write(b, WriteMeta { tag, at_ms })
    }

    /// `ms` of a non-zero signal in the stream format.
    pub(crate) fn audio(&self, ms: u32) -> Vec<u8> {
        let fmt = self.fmt();
        let frames = fmt.frames_for_ms(ms);
        let mut v = Vec::with_capacity(frames * fmt.frame_bytes());
        for i in 0..frames * usize::from(fmt.channels) {
            let s = 1_000 + (i % 100) as i16;
            match fmt.encoding {
                crate::format::Encoding::Pcm16Le => v.extend_from_slice(&s.to_le_bytes()),
                crate::format::Encoding::F32Le => {
                    v.extend_from_slice(&(f32::from(s) / 32_768.0).to_le_bytes())
                }
                _ => v.push(0x10 + (i % 7) as u8),
            }
        }
        v
    }

    pub(crate) fn pull(&self) -> (Vec<i16>, PullReport) {
        let fmt = self.fmt();
        let mut buf = vec![0i16; fmt.frames_per_tick() * usize::from(fmt.channels)];
        let r = PcmProvider::pull(&*self.q, &mut buf);
        (buf, r)
    }

    pub(crate) fn house(&mut self) -> HouseStatus {
        let mut evs = SmallVec::new();
        let s = self.q.housekeep_into(self.clock.now(), &mut evs);
        self.events.extend(evs);
        s
    }

    pub(crate) fn advance(&self, ms: u64) {
        self.clock.advance(Duration::from_millis(ms));
    }

    /// One mixer tick: pull, 20 ms pass, housekeeping.
    pub(crate) fn tick(&mut self) -> (Vec<i16>, PullReport) {
        let r = self.pull();
        self.advance(20);
        self.house();
        r
    }

    /// 20 ms pass without a pull (paused / parked track), then housekeeping.
    pub(crate) fn idle_tick(&mut self) {
        self.advance(20);
        self.house();
    }

    pub(crate) fn ready(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.hub.take_ready(&mut v);
        v
    }

    pub(crate) fn log(&self) -> Vec<&'static str> {
        self.log.calls()
    }

    pub(crate) fn queued(&self) -> u64 {
        self.q.queued_frames()
    }
}

fn le16(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Pulls until the queue is empty; returns the real samples pulled.
fn drain_samples(r: &mut Rig) -> Vec<i16> {
    let ch = usize::from(r.fmt().channels);
    let mut out = Vec::new();
    for _ in 0..100_000 {
        if r.queued() == 0 {
            break;
        }
        let (buf, rep) = r.tick();
        out.extend_from_slice(&buf[..rep.real as usize * ch]);
    }
    out
}

fn small(mut s: PlaybackSpec, max_buffer_ms: u32) -> PlaybackSpec {
    s.max_buffer_ms = max_buffer_ms;
    s.max_gap_ms = s.max_gap_ms.min(max_buffer_ms / 2);
    s
}

#[test]
fn validate_rejects_cross_field_limits() {
    let mut s = spec(PCM16_24K);
    s.max_buffer_ms = 1_000;
    s.prebuffer_ms = 501;
    assert!(matches!(s.validate(), Err(CoreError::Limit(_))));
    s.prebuffer_ms = 500;
    s.max_gap_ms = 501;
    assert!(matches!(s.validate(), Err(CoreError::Limit(_))));
    s.max_gap_ms = 500;
    assert!(s.validate().is_ok());
    s.volume = f32::NAN;
    assert!(s.validate().is_err());
}

#[test]
fn odd_and_partial_frame_writes_round_trip() {
    for fmt in [
        PCM16_24K,
        PCM16_48K_STEREO,
        F32_48K_STEREO,
        MULAW_8K,
        ALAW_8K,
    ] {
        let mut r = Rig::new(spec(fmt));
        let bytes = r.audio(200);
        let mut expected = Vec::new();
        PcmParser::new(fmt)
            .parse_frames(&bytes, &mut expected)
            .unwrap();
        let sizes = [1usize, 2, 3, 5, 7, 11, 13, 1, 1, 64, 3];
        let mut pos = 0;
        let mut k = 0;
        while pos < bytes.len() {
            let n = sizes[k % sizes.len()].min(bytes.len() - pos);
            r.write(&bytes[pos..pos + n]).unwrap();
            pos += n;
            k += 1;
        }
        let got = drain_samples(&mut r);
        assert_eq!(got, expected, "{fmt:?}");
        assert_eq!(r.q.stats().written_ms, 200);
    }
}

#[test]
fn playback_full_is_all_or_nothing() {
    let r = Rig::new(small(spec(PCM16_24K), 100)); // 2400 frames
    let samples: Vec<i16> = (0..4_000).map(|i| i as i16 + 1).collect();
    let bytes = le16(&samples);
    // 1 byte of carry, then 1000 frames (head + 999 + 1-byte tail).
    r.write(&bytes[..1]).unwrap();
    r.write(&bytes[1..2_001]).unwrap();
    assert_eq!(r.queued(), 1_000);
    assert_eq!(r.q.st.carry_len.load(Ordering::Acquire), 1);
    // 1600 more frames do not fit: nothing is consumed and the carry survives.
    match r.write(&bytes[2_001..5_201]) {
        Err(CoreError::PlaybackFull { needed_ms, free_ms }) => {
            assert_eq!(needed_ms, 66);
            assert_eq!(free_ms, 58);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.queued(), 1_000);
    assert_eq!(r.q.st.carry_len.load(Ordering::Acquire), 1);
    assert_eq!(r.q.st.written.load(Ordering::Acquire), 1_000);
    let wanted = r.q.inner.lock().space_wanted;
    assert!(wanted > 0 && wanted <= r.q.lim.cap_frames);
}

#[test]
fn exact_stream_survives_full_and_retry() {
    let mut r = Rig::new(small(spec(PCM16_24K), 100));
    let samples: Vec<i16> = (0..6_000).map(|i| (i % 30_000) as i16 + 1).collect();
    let bytes = le16(&samples);
    let mut out = Vec::new();
    let mut pos = 0;
    let mut k = 0usize;
    let mut fulls = 0;
    while pos < bytes.len() {
        let n = [1usize, 777, 1_501, 3, 2_049][k % 5].min(bytes.len() - pos);
        match r.write(&bytes[pos..pos + n]) {
            Ok(_) => {
                pos += n;
                k += 1;
            }
            Err(CoreError::PlaybackFull { .. }) => {
                fulls += 1;
                let (buf, rep) = r.tick();
                out.extend_from_slice(&buf[..rep.real as usize]);
            }
            Err(e) => panic!("{e:?}"),
        }
    }
    out.extend(drain_samples(&mut r));
    assert!(fulls > 0);
    assert_eq!(out, samples);
}

#[test]
fn rejected_writes_consume_nothing_and_cap_is_checked_first() {
    let r = Rig::new(small(spec(PCM16_24K), 100)); // 4800 bytes
    r.write(&[0x34]).unwrap();
    let data = vec![0u8; 6_000]; // > 4800
    let guard = crate::alloc_count::CountGuard::new();
    let before = crate::alloc_count::count();
    let res = r.q.write(&data, WriteMeta::default());
    let after = crate::alloc_count::count();
    drop(guard);
    assert_eq!(after, before, "no allocation before the cap check");
    assert!(matches!(res, Err(CoreError::Limit(_))));
    assert_eq!(r.q.st.carry_len.load(Ordering::Acquire), 1);
    // [0x12, 0x00]: completes the carried byte → 0x1234, then carries 0x00.
    r.q.write(&[0x12, 0x00], WriteMeta::default()).unwrap();
    assert_eq!(r.queued(), 1);
    let (buf, _) = r.pull();
    assert_eq!(buf[0], 0x1234);
}

#[test]
fn tag_longer_than_128_bytes_is_invalid() {
    let r = Rig::new(spec(PCM16_24K));
    let tag = "x".repeat(129);
    assert!(matches!(
        r.write_at(&[0, 0], Some(&tag), None),
        Err(CoreError::InvalidAudio(_))
    ));
    assert!(r.write_at(&[0, 0], Some(&tag[..128]), None).is_ok());
}

#[test]
fn concurrent_writers_never_lose_or_duplicate_bytes() {
    let r = Rig::new(small(spec(PCM16_24K), 120_000));
    let threads: Vec<_> = (0..4usize)
        .map(|t| {
            let q = r.q.clone();
            std::thread::spawn(move || {
                for i in 0..3_000usize {
                    let data = vec![t as u8; 1 + (i + t) % 5];
                    q.write(&data, WriteMeta::default()).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let total: u64 = (0..4u64)
        .map(|t| (0..3_000u64).map(|i| 1 + (i + t) % 5).sum::<u64>())
        .sum();
    let frames = r.q.st.written.load(Ordering::Acquire);
    let carry = u64::from(r.q.st.carry_len.load(Ordering::Acquire));
    assert_eq!(frames * 2 + carry, total);
    assert_eq!(r.queued(), frames);
}

#[test]
fn clear_and_close_never_wait_for_a_parsing_writer() {
    let r = Rig::new(small(spec(PCM16_48K_STEREO), 120_000));
    let q = r.q.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let writer = std::thread::spawn(move || {
        let data = vec![7u8; 4 << 20];
        let mut n = 0;
        while !stop2.load(Ordering::Acquire) && n < 20 {
            let _ = q.write(&data, WriteMeta::default());
            n += 1;
        }
    });
    let mut worst = Duration::ZERO;
    for _ in 0..200 {
        let t = std::time::Instant::now();
        r.q.clear(ClearOpts::default()).unwrap();
        worst = worst.max(t.elapsed());
        std::thread::sleep(Duration::from_micros(200));
    }
    let t = std::time::Instant::now();
    LiveSource::close(&*r.q, CloseMode::Abort);
    worst = worst.max(t.elapsed());
    stop.store(true, Ordering::Release);
    writer.join().unwrap();
    assert!(
        worst < Duration::from_millis(50),
        "clear/close took {worst:?}"
    );
    assert!(matches!(
        r.write(&[0, 0, 0, 0]),
        Err(CoreError::StreamClosed(CloseReason::Closed))
    ));
}

#[test]
fn gap_fill_regression_idle_pause_then_on_time_delta() {
    // 1000 ms played, 250 ms idle with auto-pause, then a delta at at_ms = 2500 arriving at wall
    // t0 + 2500: at most one tick of silence.
    let mut r = Rig::new(spec(PCM16_24K));
    let t0 = r.clock.now();
    let a = r.audio(1_000);
    r.write_at(&a, Some("r1"), Some(0)).unwrap();
    for _ in 0..50 {
        r.tick();
    }
    assert_eq!(r.q.stats().played_ms, 1_000);
    for _ in 0..14 {
        r.tick();
    }
    assert!(r.q.stats().idle);
    assert_eq!(r.log(), vec!["play", "pause"]);
    let elapsed = (r.clock.now() - t0).as_millis() as u64;
    r.advance(2_500 - elapsed);
    let d = r.audio(100);
    match r.write_at(&d, Some("r2"), Some(2_500)).unwrap() {
        WriteOutcome::Accepted { gap_frames, .. } => {
            assert!(gap_frames <= 480, "{gap_frames}")
        }
        o => panic!("{o:?}"),
    }
    assert_eq!(r.log(), vec!["play", "pause", "play"]);
}

#[test]
fn gap_fill_regression_early_delta_waits() {
    let mut r = Rig::new(spec(PCM16_24K));
    let a = r.audio(1_000);
    r.write_at(&a, None, Some(0)).unwrap();
    for _ in 0..50 {
        r.tick();
    }
    r.advance(100); // wall t0 + 1100
    let d = r.audio(100);
    match r.write_at(&d, None, Some(2_500)).unwrap() {
        WriteOutcome::Accepted { gap_frames, .. } => {
            let ms = gap_frames * 1_000 / 24_000;
            assert!((1_380..=1_420).contains(&ms), "{ms}");
        }
        o => panic!("{o:?}"),
    }
    assert!(r.q.stats().gap_filled_ms >= 1_380);
}

#[test]
fn late_writes_append_and_reanchor_never_trim() {
    let mut s = small(spec(PCM16_24K), 2_000);
    s.max_gap_ms = 1_000;
    let r = Rig::new(s);
    let half = r.audio(500);
    r.write_at(&half, None, Some(0)).unwrap();
    // Overlapping/late: appended whole, re-anchored at (200, t0 + 500).
    assert_eq!(
        r.write_at(&half, None, Some(200)).unwrap(),
        WriteOutcome::Accepted {
            frames: 12_000,
            gap_frames: 0
        }
    );
    // On time against the new anchor.
    let tenth = r.audio(100);
    assert_eq!(
        r.write_at(&tenth, None, Some(700)).unwrap(),
        WriteOutcome::Accepted {
            frames: 2_400,
            gap_frames: 0
        }
    );
    // Beyond max_gap: discontinuity, no silence.
    assert_eq!(
        r.write_at(&tenth, None, Some(9_000)).unwrap(),
        WriteOutcome::Accepted {
            frames: 2_400,
            gap_frames: 0
        }
    );
    assert_eq!(r.q.stats().gap_filled_ms, 0);
    assert_eq!(r.q.stats().buffered_ms, 1_200);
}

#[test]
fn gap_that_can_never_fit_reanchors_instead_of_raising() {
    let mut s = small(spec(PCM16_24K), 2_000);
    s.max_gap_ms = 1_000;
    let r = Rig::new(s);
    r.write_at(&r.audio(100), None, Some(0)).unwrap();
    // Desired start t0 + 900 → 800 ms gap; 800 + 1250 > 2000 → re-anchor.
    let big = r.audio(1_250);
    assert_eq!(
        r.write_at(&big, None, Some(900)).unwrap(),
        WriteOutcome::Accepted {
            frames: 30_000,
            gap_frames: 0
        }
    );
    // A gap that fits is inserted.
    let r = Rig::new(s);
    r.write_at(&r.audio(100), None, Some(0)).unwrap();
    match r.write_at(&r.audio(100), None, Some(600)).unwrap() {
        WriteOutcome::Accepted { gap_frames, .. } => assert_eq!(gap_frames, 12_000),
        o => panic!("{o:?}"),
    }
}

#[test]
fn extreme_timestamps_never_panic() {
    let r = Rig::new(spec(PCM16_24K));
    let a = r.audio(20);
    for at in [i64::MIN, i64::MAX, 0, i64::MIN + 1, -1, i64::MAX - 1] {
        assert!(r.write_at(&a, Some("t"), Some(at)).is_ok());
    }
    r.q.clear(ClearOpts {
        reject_late: false,
        hold_ms: 0,
        reject_until_gap_ms: Some(10_000),
    })
    .unwrap();
    for at in [i64::MIN, i64::MAX] {
        assert!(r.write_at(&a, None, Some(at)).is_ok());
    }
}

#[test]
fn gap_silence_is_excluded_from_tags_and_played() {
    let mut r = Rig::new(spec(PCM16_24K));
    r.write_at(&r.audio(100), Some("x"), Some(0)).unwrap();
    match r.write_at(&r.audio(100), Some("x"), Some(500)).unwrap() {
        WriteOutcome::Accepted { gap_frames, .. } => assert_eq!(gap_frames, 9_600),
        o => panic!("{o:?}"),
    }
    assert_eq!(r.q.item("x").unwrap().written_ms, 200);
    assert_eq!(r.q.stats().buffered_ms, 600);
    while r.queued() > 0 {
        r.tick();
    }
    let it = r.q.item("x").unwrap();
    assert_eq!(it.played_ms, 200);
    assert_eq!(r.q.stats().played_ms, 200);
    assert_eq!(r.q.stats().gap_filled_ms, 400);
}

#[test]
fn prebuffer_gate() {
    let mut s = spec(PCM16_24K);
    s.prebuffer_ms = 100; // 2400 frames, 5 ticks
    // A reply shorter than the prebuffer plays after prebuffer_ticks.
    let mut r = Rig::new(s);
    r.write(&r.audio(40)).unwrap();
    for _ in 0..5 {
        assert_eq!(r.tick().1.consumed, 0);
    }
    assert_eq!(r.tick().1.consumed, 480);
    // Enough audio opens it at once.
    let mut r = Rig::new(s);
    r.write(&r.audio(40)).unwrap();
    r.write(&r.audio(80)).unwrap();
    assert_eq!(r.tick().1.consumed, 480);
    // drain() and end_item() release it.
    let mut r = Rig::new(s);
    r.write(&r.audio(40)).unwrap();
    assert!(!r.q.arm_drain());
    assert_eq!(r.tick().1.consumed, 480);
    let mut r = Rig::new(s);
    r.write_at(&r.audio(40), Some("i"), None).unwrap();
    r.q.end_item("i");
    assert_eq!(r.tick().1.consumed, 480);
    // A later write in the same turn does not re-arm it.
    r.write(&r.audio(40)).unwrap();
    assert_eq!(r.tick().1.consumed, 480);
}

#[test]
fn clear_frees_capacity_immediately_without_a_pull() {
    let r = Rig::new(small(spec(PCM16_24K), 100));
    r.write(&r.audio(100)).unwrap();
    assert!(matches!(
        r.write(&r.audio(20)),
        Err(CoreError::PlaybackFull { .. })
    ));
    let res = r.q.clear(ClearOpts::default()).unwrap();
    assert_eq!(res.discarded_ms, 100);
    assert_eq!(r.q.stats().buffered_ms, 0);
    assert_eq!(r.q.stats().discarded_ms, 100);
    r.write(&r.audio(100)).unwrap();
    assert_eq!(
        r.ready(),
        vec![r.q.wake_id().0],
        "the waiting writer is woken"
    );
}

#[test]
fn clear_reports_an_underrun_item_and_rejects_its_late_deltas() {
    let mut r = Rig::new(spec(PCM16_24K));
    r.write_at(&r.audio(40), Some("item1"), None).unwrap();
    for _ in 0..3 {
        r.tick(); // 2 real ticks + 1 underrun tick; the tag is still open
    }
    assert!(r.q.stats().underrun_ms >= 20);
    r.advance(100); // well past output_latency: only "open" keeps it in `items`
    let res = r.q.clear(ClearOpts::default()).unwrap();
    let it = res.items.iter().find(|i| &*i.tag == "item1").unwrap();
    assert_eq!(it.written_ms, 40);
    assert_eq!(it.played_ms, 40);
    assert_eq!(it.heard_ms, 40);
    assert!(!it.complete);
    assert_eq!(
        r.write_at(&r.audio(20), Some("item1"), None).unwrap(),
        WriteOutcome::Rejected { frames: 480 }
    );
    assert_eq!(r.q.stats().rejected_ms, 20);
    // Other tags and untagged writes still play.
    assert!(matches!(
        r.write_at(&r.audio(20), Some("item2"), None).unwrap(),
        WriteOutcome::Accepted { .. }
    ));
}

#[test]
fn clear_reports_recently_pulled_closed_items_with_heard_ms() {
    let mut r = Rig::new(spec(PCM16_24K));
    r.write_at(&r.audio(40), Some("a"), None).unwrap();
    r.q.end_item("a"); // closed, but pulled within output_latency_ms
    r.tick();
    r.tick();
    let res = r.q.clear(ClearOpts::default()).unwrap();
    let it = res.items.iter().find(|i| &*i.tag == "a").unwrap();
    assert!(it.complete);
    assert!(it.heard_ms < it.played_ms, "{it:?}");
    assert!(it.heard_ms <= it.written_ms);
}

#[test]
fn tags_close_at_turn_end_and_are_not_reported_later() {
    let mut r = Rig::new(spec(PCM16_24K));
    r.write_at(&r.audio(40), Some("t1"), None).unwrap();
    for _ in 0..20 {
        r.tick();
    }
    assert!(r.q.stats().idle);
    let res = r.q.clear(ClearOpts::default()).unwrap();
    assert!(res.items.is_empty(), "{:?}", res.items);
    // Not rejected: the turn was over before the clear.
    assert!(matches!(
        r.write_at(&r.audio(20), Some("t1"), None).unwrap(),
        WriteOutcome::Accepted { .. }
    ));
}

#[test]
fn end_item_completes_once_played_and_played_never_exceeds_written() {
    let mut r = Rig::new(spec(PCM16_24K));
    r.write_at(&r.audio(40), Some("e"), None).unwrap();
    r.q.end_item("e");
    assert!(!r.q.item("e").unwrap().complete);
    r.tick();
    assert!(!r.q.item("e").unwrap().complete);
    r.tick();
    let it = r.q.item("e").unwrap();
    assert!(it.complete);
    assert_eq!(it.played_ms, it.written_ms);
    for _ in 0..5 {
        r.tick();
        let it = r.q.item("e").unwrap();
        assert!(it.played_ms <= it.written_ms);
    }
    assert!(r.q.item("missing").is_none());
}

#[test]
fn reject_until_gap_ms_drops_contiguous_timed_deltas() {
    let r = Rig::new(spec(PCM16_24K));
    let d = r.audio(100);
    for at in [0, 100, 200] {
        r.write_at(&d, None, Some(at)).unwrap();
    }
    r.q.clear(ClearOpts {
        reject_late: false,
        hold_ms: 0,
        reject_until_gap_ms: Some(250),
    })
    .unwrap();
    for at in [300, 400, 500, 600] {
        assert!(
            matches!(
                r.write_at(&d, None, Some(at)).unwrap(),
                WriteOutcome::Rejected { .. }
            ),
            "{at}"
        );
    }
    // Untimed writes are unaffected.
    assert!(matches!(
        r.write(&d).unwrap(),
        WriteOutcome::Accepted { .. }
    ));
    // 700 + 250 = 950 > 900: still contiguous with the interrupted utterance.
    assert!(matches!(
        r.write_at(&d, None, Some(900)).unwrap(),
        WriteOutcome::Rejected { .. }
    ));
    // The first delta after a real gap is accepted and ends the window.
    assert!(matches!(
        r.write_at(&d, None, Some(1_300)).unwrap(),
        WriteOutcome::Accepted { .. }
    ));
    assert!(matches!(
        r.write_at(&d, None, Some(1_400)).unwrap(),
        WriteOutcome::Accepted { .. }
    ));
}

#[test]
fn hold_ms_drops_every_write_for_a_while() {
    let r = Rig::new(spec(PCM16_24K));
    r.q.clear(ClearOpts {
        reject_late: true,
        hold_ms: 100,
        reject_until_gap_ms: None,
    })
    .unwrap();
    assert!(matches!(
        r.write(&r.audio(20)).unwrap(),
        WriteOutcome::Rejected { .. }
    ));
    r.advance(99);
    assert!(matches!(
        r.write(&r.audio(20)).unwrap(),
        WriteOutcome::Rejected { .. }
    ));
    r.advance(1);
    assert!(matches!(
        r.write(&r.audio(20)).unwrap(),
        WriteOutcome::Accepted { .. }
    ));
    assert!(matches!(
        r.q.clear(ClearOpts {
            reject_late: true,
            hold_ms: 10_001,
            reject_until_gap_ms: None
        }),
        Err(CoreError::Limit(_))
    ));
}

#[test]
fn clear_drops_the_carry() {
    let r = Rig::new(spec(PCM16_24K));
    r.write(&[0xAA]).unwrap();
    let res = r.q.clear(ClearOpts::default()).unwrap();
    assert_eq!(res.generation, 1);
    r.write(&[0x01, 0x02]).unwrap();
    assert_eq!(r.queued(), 1);
    let (buf, _) = r.pull();
    assert_eq!(buf[0], 0x0201);
    assert_eq!(r.q.st.carry_len.load(Ordering::Acquire), 0);
}

#[test]
fn the_257th_tag_evicts_one_entry() {
    let r = Rig::new(spec(PCM16_24K));
    for i in 0..=256 {
        r.write_at(&[1, 0], Some(&format!("tag{i}")), None).unwrap();
    }
    assert_eq!(r.q.stats().tags_evicted, 1);
    assert!(r.q.item("tag0").is_none());
    assert!(r.q.item("tag256").is_some());
}

#[test]
fn heard_ms_converges_after_output_latency() {
    let mut r = Rig::new(spec(PCM16_24K));
    r.write_at(&r.audio(100), Some("h"), None).unwrap();
    for _ in 0..5 {
        r.tick();
    }
    let s = r.q.stats();
    assert_eq!(s.played_ms, 100);
    assert!(s.heard_ms < 100 && s.heard_ms >= 60, "{}", s.heard_ms);
    assert!(r.q.item("h").unwrap().heard_ms < 100);
    r.advance(40);
    assert_eq!(r.q.stats().heard_ms, 100);
    assert_eq!(r.q.item("h").unwrap().heard_ms, 100);
    // ... and it stays there through idle and auto-pause.
    for _ in 0..20 {
        r.idle_tick();
    }
    assert!(r.q.stats().idle);
    assert_eq!(r.q.item("h").unwrap().heard_ms, 100);
    assert_eq!(r.q.stats().heard_ms, 100);
}

#[test]
fn discard_policy_drops_audio_while_the_link_is_down() {
    let mut s = spec(PCM16_24K);
    s.when_disconnected = DisconnectPolicy::Discard;
    let r = Rig::new(s);
    r.write(&r.audio(100)).unwrap();
    LiveSource::on_link(&*r.q, false);
    assert_eq!(r.q.stats().buffered_ms, 0);
    assert_eq!(r.q.stats().discarded_ms, 100);
    assert!(matches!(
        r.write(&r.audio(20)).unwrap(),
        WriteOutcome::Rejected { .. }
    ));
    LiveSource::on_link(&*r.q, true);
    assert!(matches!(
        r.write(&r.audio(20)).unwrap(),
        WriteOutcome::Accepted { .. }
    ));
    // The default policy buffers.
    let r = Rig::new(spec(PCM16_24K));
    LiveSource::on_link(&*r.q, false);
    assert!(matches!(
        r.write(&r.audio(20)).unwrap(),
        WriteOutcome::Accepted { .. }
    ));
}

#[test]
fn starts_paused_resumes_on_write_and_auto_pauses_on_idle() {
    let mut r = Rig::new(spec(PCM16_24K));
    assert!(r.log().is_empty(), "created paused: no track calls");
    for _ in 0..30 {
        r.idle_tick();
    }
    assert!(r.log().is_empty());
    r.write(&r.audio(40)).unwrap();
    assert_eq!(r.log(), vec!["play"]);
    r.tick();
    r.tick();
    assert!(
        r.events
            .iter()
            .any(|e| matches!(e, EventData::PlaybackStarted { playback_id: 7 }))
    );
    for _ in 0..13 {
        r.tick();
    }
    assert_eq!(r.log(), vec!["play", "pause"]);
    assert!(r.events.iter().any(|e| matches!(
        e,
        EventData::PlaybackIdle {
            playback_id: 7,
            played_ms: 40
        }
    )));
    r.write(&r.audio(20)).unwrap();
    assert_eq!(r.log(), vec!["play", "pause", "play"]);
    // User pause/resume.
    r.q.pause(true);
    assert_eq!(r.tick().1.consumed, 0);
    r.q.pause(false);
    assert_eq!(r.log(), vec!["play", "pause", "play", "pause", "play"]);
    assert_eq!(r.tick().1.consumed, 480);
}

#[test]
fn arm_space_drain_and_idle_ring_from_the_housekeeper() {
    let mut r = Rig::new(small(spec(PCM16_24K), 100));
    let id = r.q.wake_id().0;
    r.write(&r.audio(100)).unwrap();
    assert!(!r.q.arm_space(960)); // 20 ms
    assert!(r.ready().is_empty());
    r.tick();
    assert_eq!(r.ready(), vec![id]);
    assert!(r.q.arm_space(960));
    assert!(!r.q.arm_drain());
    while r.queued() > 0 {
        r.tick();
    }
    assert_eq!(r.ready(), vec![id]);
    assert!(r.q.arm_drain());
    assert!(!r.q.arm_idle());
    for _ in 0..15 {
        r.tick();
    }
    assert_eq!(r.ready(), vec![id]);
    assert!(r.q.arm_idle());
}

#[test]
fn self_mute_holds_the_position() {
    let mut r = Rig::new(spec(PCM16_24K));
    r.write(&r.audio(40)).unwrap();
    r.muted.store(true, Ordering::Release);
    for _ in 0..3 {
        let (buf, rep) = r.tick();
        assert_eq!(rep.consumed, 0);
        assert!(buf.iter().all(|s| *s == 0));
    }
    assert_eq!(r.q.stats().played_ms, 0);
    assert_eq!(r.q.stats().buffered_ms, 40);
    assert!(!r.q.arm_drain(), "drain keeps waiting while muted");
    r.muted.store(false, Ordering::Release);
    assert_eq!(r.tick().1.real, 480);
    assert_eq!(r.q.stats().played_ms, 20);
}

#[test]
fn contention_gives_silence_and_is_counted() {
    let r = Rig::new(spec(PCM16_24K));
    r.write(&r.audio(40)).unwrap();
    {
        let _held = r.q.inner.lock();
        let (buf, rep) = r.pull();
        assert_eq!(rep.consumed, 0);
        assert!(buf.iter().all(|s| *s == 0));
    }
    assert_eq!(r.q.stats().contended_ticks, 1);
    assert_eq!(r.pull().1.consumed, 480);
}

#[test]
fn close_modes_and_reader_end() {
    let mut r = Rig::new(spec(PCM16_24K));
    r.write(&r.audio(40)).unwrap();
    LiveSource::close(&*r.q, CloseMode::Drain);
    assert!(!PcmProvider::should_end(&*r.q));
    assert!(matches!(
        r.write(&r.audio(20)),
        Err(CoreError::StreamClosed(CloseReason::Closed))
    ));
    r.tick();
    assert!(!PcmProvider::should_end(&*r.q));
    r.tick();
    assert!(PcmProvider::should_end(&*r.q));
    assert_eq!(r.log(), vec!["play"], "a playing track drains to EOF");
    PcmProvider::on_reader_drop(&*r.q);
    assert!(r.q.arm_end());
    assert_eq!(r.house(), HouseStatus::Finished);
    assert!(r.events.iter().any(|e| matches!(
        e,
        EventData::PlaybackEnded {
            playback_id: 7,
            reason: "drained"
        }
    )));
    // Abort frees now and stops the track.
    let r = Rig::new(spec(PCM16_24K));
    r.write(&r.audio(40)).unwrap();
    LiveSource::close(&*r.q, CloseMode::Abort);
    assert!(PcmProvider::should_end(&*r.q));
    assert_eq!(r.q.stats().buffered_ms, 0);
    assert_eq!(r.log(), vec!["play", "stop"]);
    LiveSource::close(&*r.q, CloseMode::Abort);
    assert_eq!(r.log(), vec!["play", "stop"], "idempotent");
    // Connection close.
    let r = Rig::new(spec(PCM16_24K));
    r.q.conn_closed();
    assert!(matches!(
        r.write(&r.audio(20)),
        Err(CoreError::StreamClosed(CloseReason::ConnectionClosed))
    ));
    // A drain-close of an idle (paused) stream stops it at once.
    let r = Rig::new(spec(PCM16_24K));
    LiveSource::close(&*r.q, CloseMode::Drain);
    assert_eq!(r.log(), vec!["stop"]);
}

#[test]
fn reader_gone_without_close_ends_as_stopped_and_closes_writers() {
    let mut r = Rig::new(spec(PCM16_24K));
    r.write(&r.audio(40)).unwrap();
    PcmProvider::on_reader_drop(&*r.q);
    assert_eq!(r.house(), HouseStatus::Finished);
    assert!(r.events.iter().any(|e| matches!(
        e,
        EventData::PlaybackEnded {
            reason: "stopped",
            ..
        }
    )));
    assert!(r.write(&r.audio(20)).is_err());
    assert_eq!(r.ready(), vec![r.q.wake_id().0]);
}

#[test]
fn mixer_pull_does_not_allocate() {
    let mut r = Rig::new(spec(PCM16_24K));
    for i in 0..40 {
        r.write_at(&r.audio(20), Some(&format!("t{}", i % 3)), Some(i * 20))
            .unwrap();
    }
    let mut buf = vec![0i16; 480];
    let guard = crate::alloc_count::CountGuard::new();
    let before = crate::alloc_count::count();
    for _ in 0..30 {
        let _ = PcmProvider::pull(&*r.q, &mut buf);
    }
    let after = crate::alloc_count::count();
    drop(guard);
    assert_eq!(after, before);
    // The housekeeper frees the consumed segments.
    r.house();
    assert!(r.q.inner.lock().dead.is_empty());
}

/// A queue with no track attached yet (W2 attaches right after `driver.play`).
fn unattached(spec: PlaybackSpec) -> (Arc<PcmQueue>, Arc<WakeHub>, UnixStream) {
    set_role(Role::Harness);
    let (hub, bell) = test_hub();
    let (clock, _) = Clock::manual();
    let q = PcmQueue::new(
        9,
        &spec,
        hub.alloc().unwrap(),
        clock,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    (q, hub, bell)
}

#[test]
fn a_close_before_attach_is_applied_when_the_track_attaches() {
    // Never written (paused), drain-closed before the attach: it can never drain, so it stops.
    let (q, _hub, _bell) = unattached(spec(PCM16_24K));
    LiveSource::close(&*q, CloseMode::Drain);
    let log = Rec::default();
    q.attach_track(Box::new(log.clone()));
    assert_eq!(log.calls(), vec!["stop"]);
    // Closed by T5 before the attach.
    let (q, _hub, _bell) = unattached(spec(PCM16_24K));
    q.conn_closed();
    let log = Rec::default();
    q.attach_track(Box::new(log.clone()));
    assert_eq!(log.calls(), vec!["stop"]);
    // Written, then drain-closed before the attach: it plays out (EOF once empty), no stop.
    let (q, _hub, _bell) = unattached(spec(PCM16_24K));
    q.write(&[1, 0, 2, 0], WriteMeta::default()).unwrap();
    LiveSource::close(&*q, CloseMode::Drain);
    let log = Rec::default();
    q.attach_track(Box::new(log.clone()));
    assert_eq!(log.calls(), vec!["play"]);
    assert!(!PcmProvider::should_end(&*q));
}

#[test]
fn connection_close_wakes_and_resolves_an_end_waiter() {
    let r = Rig::new(spec(PCM16_24K));
    r.write(&r.audio(40)).unwrap();
    LiveSource::close(&*r.q, CloseMode::Drain);
    assert!(!r.q.arm_end(), "draining: the reader is still there");
    let _ = r.ready();
    // T5: the housekeeper stops and the reader goes with the Driver later.
    r.q.conn_closed();
    assert_eq!(r.ready(), vec![r.q.wake_id().0]);
    assert!(r.q.arm_end());
}

#[test]
fn link_down_discard_starts_a_new_turn() {
    let mut s = spec(PCM16_24K);
    s.when_disconnected = DisconnectPolicy::Discard;
    s.prebuffer_ms = 100;
    let r = Rig::new(s);
    r.write(&r.audio(200)).unwrap();
    assert_eq!(
        r.pull().1.consumed,
        480,
        "the gate opened at prebuffer_frames"
    );
    LiveSource::on_link(&*r.q, false);
    LiveSource::on_link(&*r.q, true);
    // The next reply prebuffers again, as after clear().
    r.write(&r.audio(40)).unwrap();
    assert_eq!(r.pull().1.consumed, 0);
}

#[test]
fn abandon_aborts_an_open_stream_but_lets_a_drain_finish() {
    let r = Rig::new(spec(PCM16_24K));
    r.write(&r.audio(40)).unwrap();
    r.q.abandon();
    assert!(PcmProvider::should_end(&*r.q));
    assert_eq!(r.log(), vec!["play", "stop"]);
    let mut r = Rig::new(spec(PCM16_24K));
    r.write(&r.audio(40)).unwrap();
    LiveSource::close(&*r.q, CloseMode::Drain);
    r.q.abandon();
    assert_eq!(r.log(), vec!["play"], "the drain continues");
    assert_eq!(r.q.stats().buffered_ms, 40);
    r.tick();
    r.tick();
    assert!(PcmProvider::should_end(&*r.q));
    assert_eq!(r.q.stats().played_ms, 40);
}
