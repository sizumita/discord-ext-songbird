//! Unit tests of the synchronous receive pipeline (§13.1 `rx::pipeline`): synthetic `RxInput`s with
//! real Opus from `songbird::driver::opus::Encoder`, a manual timeline and no runtime or songbird.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::Mutex;
use smallvec::SmallVec;

use super::bus::{MixFilter, MixPolicy};
use super::opus::SILENT_FRAME;
use super::pipeline::{MAX_SYNTH_BURST, RxConfig, RxPipeline, RxTimer};
use super::queue::{Delivered, Overflow, RxCmd, RxSource, SubQueue, SubSpec, Subscription};
use super::raw::{RawTick, RawVoice, RxInput};
use super::router::{RouterQueue, RouterSpec};
use super::test_util::{RtpBuild, hub, opus_frames, sine48, token};
use crate::ctrl::CtrlQueue;
use crate::error::CloseReason;
use crate::events::{EventData, EventHub, Pop};
use crate::format::{Encoding, Format, PCM16_24K, PCM16_48K, SampleRate};
use crate::wake::WakeHub;

const A: u64 = 1_001;
const B: u64 = 1_002;
const SA: u32 = 0xA;
const SB: u32 = 0xB;

struct Rig {
    p: RxPipeline,
    hub: Arc<WakeHub>,
    #[cfg(unix)]
    _reader: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    _reader: std::net::TcpStream,
    t0: Instant,
    now: Instant,
    unclassified: Arc<Mutex<Vec<u64>>>,
    next_id: u64,
    seqs: HashMap<u32, u16>,
}

fn cfg(t0: Instant) -> RxConfig {
    RxConfig {
        enabled: true,
        hangover_ms: 200,
        voice_threshold_dbfs: None,
        max_decoded_users: 64,
        decode_state_timeout: Duration::from_secs(60),
        stall_timeout: Duration::from_millis(2_000),
        t0,
    }
}

impl Rig {
    fn new() -> Self {
        Self::with(|_| {})
    }

    fn with(f: impl FnOnce(&mut RxConfig)) -> Self {
        let (hub, reader) = hub();
        let t0 = Instant::now();
        let mut c = cfg(t0);
        f(&mut c);
        let unclassified = Arc::new(Mutex::new(Vec::new()));
        let p = RxPipeline::new(
            c,
            EventHub::new(),
            CtrlQueue::new(hub.clone()),
            unclassified.clone(),
        );
        Rig {
            p,
            hub,
            _reader: reader,
            t0,
            now: t0,
            unclassified,
            next_id: 1,
            seqs: HashMap::new(),
        }
    }

    fn subscribe(
        &mut self,
        source: RxSource,
        format: Format,
        chunk_ms: u32,
        max_queue_ms: u32,
        overflow: Overflow,
    ) -> Subscription {
        let spec = SubSpec {
            source,
            format,
            chunk_ms,
            max_queue_ms,
            overflow,
        };
        spec.validate().unwrap();
        let queue = SubQueue::new(&spec, token(&self.hub));
        let id = self.next_id;
        self.next_id += 1;
        self.p.apply(RxCmd::AddSub {
            id,
            spec,
            queue: queue.clone(),
        });
        Subscription::detached(id, queue)
    }

    fn mixed(&mut self, f: MixFilter, format: Format, chunk_ms: u32) -> Subscription {
        self.subscribe(
            RxSource::Mixed(f),
            format,
            chunk_ms,
            60_000,
            Overflow::DropOldest,
        )
    }

    fn user(&mut self, u: u64, format: Format, chunk_ms: u32) -> Subscription {
        self.subscribe(
            RxSource::User(u),
            format,
            chunk_ms,
            60_000,
            Overflow::DropOldest,
        )
    }

    fn advance(&mut self, ms: u64) {
        self.now += Duration::from_millis(ms);
    }

    fn timer(&mut self) -> RxTimer {
        self.p.on_timer(self.now)
    }

    /// Advances `ms` in 20 ms steps, running the timer each step (no real ticks).
    fn idle(&mut self, ms: u64) {
        for _ in 0..ms / 20 {
            self.advance(20);
            self.timer();
        }
    }

    fn map(&mut self, ssrc: u32, user: u64) {
        self.p.on_input(
            RxInput::Map {
                ssrc,
                user_id: user,
            },
            self.now,
        );
    }

    fn packet(&mut self, ssrc: u32, opus: &[u8]) -> RawVoice {
        let seq = self.seqs.entry(ssrc).or_insert(1_000);
        *seq = seq.wrapping_add(1);
        let (pkt, payload_offset, payload_end) = RtpBuild::new(*seq, ssrc).build(opus, 16);
        RawVoice::Packet {
            ssrc,
            packet: Bytes::from(pkt),
            payload_offset,
            payload_end,
        }
    }

    /// One real tick now (no time advance).
    fn tick_now(&mut self, voices: Vec<RawVoice>) {
        let raw = RawTick {
            arrived: self.now,
            voices: voices.into_iter().collect(),
            silent: SmallVec::new(),
        };
        self.p.on_input(RxInput::Tick(raw), self.now);
    }

    /// Advances 20 ms, delivers one real tick and runs the timer.
    fn tick(&mut self, voices: Vec<RawVoice>) {
        self.advance(20);
        self.tick_now(voices);
        self.timer();
    }

    fn events(&mut self) -> Vec<EventData> {
        self.p.take_emitted()
    }
}

fn drain(sub: &Subscription) -> Vec<Delivered> {
    let mut v = Vec::new();
    while let Pop::Item(d) = sub.queue.try_pop() {
        v.push(d);
    }
    v
}

fn timelines(v: &[Delivered]) -> Vec<u64> {
    v.iter().map(|d| d.chunk.meta.timeline_ms).collect()
}

/// `n` 20 ms Opus packets of a sine.
fn voice(freq: f32, amp: f32, n: usize) -> Vec<Vec<u8>> {
    opus_frames(&sine48(freq, amp, 960 * n))
}

fn filter_excluding(users: &[u64]) -> MixFilter {
    MixFilter {
        exclude: users.iter().copied().collect(),
        ..MixFilter::everyone()
    }
}

#[test]
fn continuous_output_from_t0_without_any_tick() {
    let mut r = Rig::new();
    let sub = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    r.timer();
    r.idle(2_000);
    let v = drain(&sub);
    assert!((95..=98).contains(&v.len()), "{}", v.len());
    for (i, d) in v.iter().enumerate() {
        let i = i as u64;
        assert_eq!(d.chunk.meta.timeline_ms, i * 20);
        assert_eq!((d.start_ms, d.seq, d.dropped_ms), (i * 20, i, 0));
        assert_eq!(
            d.chunk.meta.gap_ms, 20,
            "synthetic silence is reported as gap"
        );
        assert!(!d.chunk.meta.speaking);
        assert_eq!(d.chunk.data.len(), 960);
        assert!(d.chunk.data.as_bytes().iter().all(|b| *b == 0));
    }
    assert_eq!(r.p.counts().ticks_synthetic, v.len() as u64);
    assert_eq!(r.p.timeline_ms(), v.len() as u64 * 20);
}

#[test]
fn synthesis_triggers_only_beyond_100_ms() {
    let mut r = Rig::new();
    let sub = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    r.timer();
    r.advance(100);
    let t = r.timer();
    assert!(drain(&sub).is_empty(), "100 ms behind is within the slack");
    assert_eq!(t.next_wake, Some(r.t0 + Duration::from_millis(120)));
    r.advance(20);
    let t = r.timer();
    assert_eq!(drain(&sub).len(), 4, "synthesised up to expected − 2");
    assert_eq!(t.next_wake, Some(r.t0 + Duration::from_millis(200)));
}

#[test]
fn idle_connection_fast_forwards_lazily() {
    let mut r = Rig::new();
    r.advance(10_000);
    r.timer();
    assert_eq!(r.p.timeline_ms(), 0, "no consumer: no synthesis");
    let t = r.timer();
    assert_eq!(t.next_wake, None, "idle connections cost no wake-ups");
    let sub = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    r.timer();
    assert_eq!(
        r.p.timeline_ms(),
        9_960,
        "fast-forwarded to expected − 2 on subscribe"
    );
    r.idle(200);
    let v = drain(&sub);
    assert!(!v.is_empty());
    assert_eq!(v[0].chunk.meta.timeline_ms, 9_960);
    assert_eq!(v[0].start_ms, 0);
    assert_eq!(sub.queue.origin_timeline_ms(), Some(9_960));
}

#[test]
fn long_outage_synthesises_at_most_one_burst() {
    let mut r = Rig::new();
    let sub = r.subscribe(
        RxSource::Mixed(MixFilter::everyone()),
        PCM16_24K,
        20,
        60_000,
        Overflow::DropOldest,
    );
    r.timer();
    r.advance(200_000);
    r.timer();
    assert_eq!(r.p.counts().ticks_synthetic, MAX_SYNTH_BURST);
    assert_eq!(r.p.timeline_ms(), 200_000 - 40);
    let v = drain(&sub);
    assert_eq!(v.len() as u64, MAX_SYNTH_BURST);
    assert_eq!(
        v[0].chunk.meta.timeline_ms,
        200_000 - 40 - MAX_SYNTH_BURST * 20
    );
}

#[test]
fn real_audio_per_user_and_mixed() {
    let mut r = Rig::new();
    r.map(SA, A);
    r.map(SB, B);
    let ev = r.events();
    assert!(matches!(
        ev[0],
        EventData::UserMapped {
            user_id: A,
            ssrc: SA
        }
    ));
    assert!(matches!(
        ev[1],
        EventData::UserMapped {
            user_id: B,
            ssrc: SB
        }
    ));
    assert_eq!(*r.unclassified.lock(), [A, B]);
    let ua = r.user(A, PCM16_48K, 20);
    let ub = r.user(B, PCM16_48K, 20);
    let ub2 = r.user(B, PCM16_48K, 20);
    let mix = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    let pa = voice(440.0, 0.3, 60);
    let pb = voice(660.0, 0.05, 60);
    for i in 0..60 {
        let voices = vec![r.packet(SA, &pa[i]), r.packet(SB, &pb[i])];
        r.tick(voices);
    }
    let (va, vb, vb2, vm) = (drain(&ua), drain(&ub), drain(&ub2), drain(&mix));
    assert!(va.len() >= 58, "{}", va.len());
    assert_eq!(va.len(), vb.len());
    // One decode, shared chunks: both subscribers of B's lane get the same Arc.
    for (x, y) in vb.iter().zip(&vb2) {
        assert!(Arc::ptr_eq(&x.chunk, &y.chunk));
    }
    // Per-user chunks contain only that user's samples.
    for d in va.iter().skip(5) {
        let m = &d.chunk.meta;
        assert_eq!(m.user_id, Some(A));
        assert!(m.speaking && m.voiced_ms == 20 && m.gap_ms == 0);
        assert_eq!(&*m.speakers, &[A]);
        // 0.3 peak sine: −13.5 dBFS RMS.
        assert!(
            (m.level.rms_dbfs + 13.5).abs() < 2.0,
            "A {}",
            m.level.rms_dbfs
        );
        assert_eq!(d.chunk.data.len(), 1_920);
    }
    for d in vb.iter().skip(5) {
        let m = &d.chunk.meta;
        assert!(
            (m.level.rms_dbfs + 29.0).abs() < 2.0,
            "B {}",
            m.level.rms_dbfs
        );
    }
    for d in vm.iter().skip(5) {
        let m = &d.chunk.meta;
        assert_eq!(m.user_id, None);
        assert_eq!(&*m.speakers, &[A, B]);
        assert!(
            (m.level.rms_dbfs + 13.4).abs() < 2.0,
            "mix {}",
            m.level.rms_dbfs
        );
        assert_eq!(d.chunk.data.len(), 960);
    }
    // Timelines of every stream line up (same clock).
    assert_eq!(timelines(&va), timelines(&vb));
    let ev = r.events();
    let starts: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            EventData::SpeakingStarted { user_id, .. } => Some(*user_id),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 2);
    assert!(starts.contains(&A) && starts.contains(&B));
    // Silence ends the spurts after the hangover.
    for _ in 0..12 {
        r.tick(vec![]);
    }
    let stops = r
        .events()
        .iter()
        .filter(|e| matches!(e, EventData::SpeakingStopped { .. }))
        .count();
    assert_eq!(stops, 2);
    let c = r.p.counts();
    assert_eq!(c.ticks_real, 72);
    assert_eq!(c.bad_packets, 0);
    assert_eq!(c.decoders_active, 2);
}

#[test]
fn every_format_for_per_user_and_mixed_lanes() {
    let mut formats = Vec::new();
    for rate in SampleRate::ALL {
        for ch in [1u8, 2] {
            for enc in [Encoding::Pcm16Le, Encoding::F32Le] {
                formats.push(Format::new(rate.hz(), ch, enc).unwrap());
            }
        }
    }
    formats.push(Format::new(8_000, 1, Encoding::MuLaw).unwrap());
    formats.push(Format::new(8_000, 1, Encoding::ALaw).unwrap());
    let mut r = Rig::new();
    r.map(SA, A);
    r.timer();
    let mut subs = Vec::new();
    for (i, fmt) in formats.iter().enumerate() {
        let chunk_ms = [10u32, 20, 30, 40][i % 4];
        subs.push((r.user(A, *fmt, chunk_ms), *fmt, chunk_ms, true));
        subs.push((
            r.mixed(MixFilter::everyone(), *fmt, chunk_ms),
            *fmt,
            chunk_ms,
            false,
        ));
    }
    let pa = voice(500.0, 0.25, 30);
    for p in &pa {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    for (sub, fmt, chunk_ms, per_user) in &subs {
        let v = drain(sub);
        assert!(v.len() as u64 >= 500 / u64::from(*chunk_ms), "{fmt:?}");
        let want = fmt.bytes_for_ms(*chunk_ms).unwrap();
        for d in &v {
            assert_eq!(d.chunk.data.len(), want, "{fmt:?} {chunk_ms}");
            assert_eq!(d.chunk.meta.format, *fmt);
            assert_eq!(d.chunk.meta.timeline_ms % u64::from(*chunk_ms), 0);
            assert_eq!(d.chunk.meta.user_id, per_user.then_some(A));
        }
        let loud = v
            .iter()
            .filter(|d| d.chunk.meta.level.rms_dbfs > -25.0)
            .count();
        assert!(loud >= v.len() / 2, "{fmt:?}: audio reaches every format");
    }
    let c = r.p.counts();
    assert_eq!(c.lanes as usize, subs.len());
    assert_eq!(c.buses, 1);
    // One converter per (source, rate, channels): 8 per source.
    assert_eq!(c.converters, 16);
}

#[test]
fn silent_ticks_are_skipped_after_a_burst_but_voiced_never() {
    let mut r = Rig::new();
    r.map(SA, A);
    let sub = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    r.timer();
    r.idle(2_000);
    let synthesised = r.p.counts().ticks_synthetic;
    assert!(synthesised >= 95);
    drain(&sub);
    // A burst of delayed real ticks at one instant: 20 silent, then 10 voiced.
    let pa = voice(300.0, 0.3, 10);
    for _ in 0..20 {
        let v = vec![r.packet(SA, &SILENT_FRAME)];
        r.tick_now(v);
    }
    for p in &pa {
        let v = vec![r.packet(SA, p)];
        r.tick_now(v);
    }
    let c = r.p.counts();
    assert!(c.ticks_skipped >= 10, "{c:?}");
    assert_eq!(c.ticks_real + c.ticks_skipped, 30);
    let v = drain(&sub);
    let voiced = v.iter().filter(|d| d.chunk.meta.speaking).count();
    assert_eq!(voiced, 10, "voiced ticks are never dropped");
    // The timeline stays contiguous (no holes, no repeats).
    let t = timelines(&v);
    assert!(t.windows(2).all(|w| w[1] == w[0] + 20));
    // Once real time catches up, synthesis resumes from the advanced timeline.
    let before = r.p.timeline_ms();
    r.idle(1_000);
    assert!(r.p.timeline_ms() > before);
}

#[test]
fn users_are_decoded_only_when_needed() {
    let mut r = Rig::new();
    r.map(SA, A);
    r.map(SB, B);
    let pa = voice(440.0, 0.3, 40);
    for p in pa.iter().take(5) {
        let v = vec![r.packet(SA, p), r.packet(SB, p)];
        r.tick(v);
    }
    assert_eq!(
        r.p.counts().decoders_active,
        0,
        "speaking needs packet presence only"
    );
    let starts = r
        .events()
        .iter()
        .filter(|e| matches!(e, EventData::SpeakingStarted { .. }))
        .count();
    assert_eq!(starts, 2);
    let _ua = r.user(A, PCM16_24K, 20);
    for p in pa.iter().skip(5).take(5) {
        let v = vec![r.packet(SA, p), r.packet(SB, p)];
        r.tick(v);
    }
    assert_eq!(r.p.counts().decoders_active, 1);
    let _mb = r.mixed(
        MixFilter {
            include: Some([B].into_iter().collect()),
            ..MixFilter::everyone()
        },
        PCM16_24K,
        20,
    );
    for p in pa.iter().skip(10).take(5) {
        let v = vec![r.packet(SA, p), r.packet(SB, p)];
        r.tick(v);
    }
    assert_eq!(r.p.counts().decoders_active, 2);
    // Unmapped audio is decoded only for a bus that takes it (include is None).
    let pc = voice(200.0, 0.3, 5);
    for p in &pc {
        let v = vec![r.packet(0xC, p)];
        r.tick(v);
    }
    assert_eq!(r.p.counts().decoders_active, 2);
    let mix = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    for p in &pc {
        let v = vec![r.packet(0xC, p)];
        r.tick(v);
    }
    assert_eq!(r.p.counts().decoders_active, 3);
    assert!(
        drain(&mix)
            .iter()
            .any(|d| d.chunk.meta.level.rms_dbfs > -30.0)
    );
}

#[test]
fn max_decoded_users_evicts_least_recently_seen() {
    let mut r = Rig::with(|c| c.max_decoded_users = 2);
    let _mix = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    let pa = voice(440.0, 0.3, 10);
    for (i, ssrc) in [1u32, 2, 3].into_iter().enumerate() {
        r.map(ssrc, 100 + i as u64);
    }
    for p in &pa {
        let v = vec![r.packet(1, p), r.packet(2, p), r.packet(3, p)];
        r.tick(v);
        assert!(r.p.counts().decoders_active <= 2);
    }
}

#[test]
fn set_filter_rehomes_at_a_boundary_without_gap_or_duplicate() {
    let mut r = Rig::new();
    r.map(SA, A);
    r.map(SB, B);
    let sub = r.mixed(MixFilter::everyone(), PCM16_24K, 30);
    r.timer();
    let pa = voice(440.0, 0.3, 80);
    let pb = voice(700.0, 0.3, 80);
    let mut switched_at = None;
    for i in 0..80 {
        if i == 31 {
            // Tick 31 starts at 620 ms: not a 30 ms boundary.
            r.p.apply(RxCmd::SetFilter(sub.id, filter_excluding(&[A])));
            switched_at = Some(r.p.timeline_ms());
        }
        let v = vec![r.packet(SA, &pa[i]), r.packet(SB, &pb[i])];
        r.tick(v);
    }
    let v = drain(&sub);
    let t = timelines(&v);
    assert!(t.windows(2).all(|w| w[1] == w[0] + 30), "{t:?}");
    let switch_ms = switched_at.unwrap().div_ceil(30) * 30;
    for d in &v {
        let m = &d.chunk.meta;
        if m.timeline_ms + 30 <= switch_ms && m.timeline_ms >= 120 {
            assert_eq!(&*m.speakers, &[A, B], "{}", m.timeline_ms);
        }
        if m.timeline_ms >= switch_ms {
            assert_eq!(&*m.speakers, &[B], "{}", m.timeline_ms);
        }
    }
    let c = r.p.counts();
    assert_eq!(
        (c.lanes, c.buses, c.converters),
        (1, 1, 1),
        "old lane released"
    );
    // Back to everyone at a boundary-aligned tick (tick 81 → 1620 ms is a 30 ms boundary).
    r.tick(vec![]);
    r.p.apply(RxCmd::SetFilter(sub.id, MixFilter::everyone()));
    r.tick(vec![]);
    assert_eq!(r.p.counts().lanes, 1);
}

#[test]
fn watchdog_reports_a_stall_on_every_timer_call_while_it_lasts() {
    let mut r = Rig::new();
    r.p.on_input(RxInput::LinkUp { ssrc: Some(1) }, r.now);
    r.advance(1_999);
    let t = r.timer();
    assert_eq!(t.stalled_since, None);
    assert_eq!(t.next_wake, Some(r.t0 + Duration::from_millis(2_000)));
    r.advance(1);
    let t = r.timer();
    assert_eq!(t.stalled_since, Some(r.t0));
    r.advance(1_000);
    assert_eq!(r.timer().stalled_since, Some(r.t0));
    // The ConnWorker emits `ReceiveStalled` (once per stall); the pipeline never does.
    assert!(
        !r.events()
            .iter()
            .any(|e| matches!(e, EventData::ReceiveStalled { .. }))
    );
    r.tick(vec![]);
    assert_eq!(r.timer().stalled_since, None);
    // Link down: no stall detection.
    r.p.on_input(RxInput::LinkDown, r.now);
    r.advance(10_000);
    assert_eq!(r.timer().stalled_since, None);
}

#[test]
fn removing_subscriptions_frees_lanes_converters_and_buses() {
    let mut r = Rig::new();
    let m1 = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    let m2 = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    let m3 = r.mixed(MixFilter::everyone(), PCM16_24K, 40);
    let u1 = r.user(A, PCM16_24K, 20);
    r.timer();
    let c = r.p.counts();
    assert_eq!(
        (c.subscriptions, c.lanes, c.converters, c.buses),
        (4, 3, 2, 1)
    );
    r.idle(200);
    let (v1, v2) = (drain(&m1), drain(&m2));
    assert!(!v1.is_empty());
    for (x, y) in v1.iter().zip(&v2) {
        assert!(Arc::ptr_eq(&x.chunk, &y.chunk), "one lane, one Arc");
    }
    drop(m1);
    r.timer();
    let c = r.p.counts();
    assert_eq!(
        (c.subscriptions, c.lanes, c.converters, c.buses),
        (3, 3, 2, 1)
    );
    drop(m2);
    drop(m3);
    r.timer();
    let c = r.p.counts();
    assert_eq!(
        (c.subscriptions, c.lanes, c.converters, c.buses),
        (1, 1, 1, 0)
    );
    u1.close();
    r.p.apply(RxCmd::RemoveSub(u1.id));
    r.timer();
    let c = r.p.counts();
    assert_eq!(
        (c.subscriptions, c.lanes, c.converters, c.buses),
        (0, 0, 0, 0)
    );
}

#[test]
fn overflow_policies_through_the_pipeline() {
    let mut r = Rig::new();
    let fmt = PCM16_24K;
    let oldest = r.subscribe(RxSource::User(A), fmt, 20, 100, Overflow::DropOldest);
    let newest = r.subscribe(RxSource::User(A), fmt, 20, 100, Overflow::DropNewest);
    let error = r.subscribe(RxSource::User(A), fmt, 20, 100, Overflow::Error);
    r.timer();
    r.idle(1_000);
    // drop_oldest: the newest 5 chunks; the first carries the whole hole before it.
    let v = drain(&oldest);
    assert_eq!(v.len(), 5);
    assert_eq!(v[0].dropped_ms, v[0].start_ms);
    let s = oldest.queue.stats();
    assert_eq!(s.dropped_ms, v[0].dropped_ms);
    // drop_newest: the first 5 chunks; the next accepted chunk reports the hole.
    let v = drain(&newest);
    assert_eq!(timelines(&v), [0, 20, 40, 60, 80]);
    let held = newest.queue.stats().dropped_ms;
    assert!(held > 0);
    r.idle(100);
    let v = drain(&newest);
    assert_eq!(v[0].dropped_ms, held);
    assert_eq!(v[0].start_ms, 100 + held);
    // error: closes, is unregistered, delivers its 5 chunks, then StreamOverflow once.
    assert!(error.queue.is_closed());
    assert_eq!(r.p.counts().subscriptions, 2);
    let mut n = 0;
    loop {
        match error.queue.try_pop_or_arm() {
            Pop::Item(_) => n += 1,
            Pop::Closed(r) => {
                assert_eq!(r, CloseReason::Overflow);
                break;
            }
            Pop::Empty => panic!("closed queue must not arm"),
        }
    }
    assert_eq!(n, 5);
    assert!(matches!(
        error.queue.try_pop_or_arm(),
        Pop::Closed(CloseReason::Closed)
    ));
}

/// §13.1 soak: 180 000 ticks (1 h) with 3 subscriptions and no consumer; the queued bytes never
/// exceed the subscriptions' budgets.
#[test]
fn soak_one_hour_without_consumer_stays_within_budget() {
    let mut r = Rig::new();
    r.map(SA, A);
    let specs = [
        (
            RxSource::User(A),
            PCM16_24K,
            20u32,
            2_000u32,
            Overflow::DropOldest,
        ),
        (
            RxSource::Mixed(MixFilter::everyone()),
            Format::new(48_000, 2, Encoding::F32Le).unwrap(),
            100,
            1_000,
            Overflow::DropNewest,
        ),
        (
            RxSource::Mixed(filter_excluding(&[B])),
            Format::new(8_000, 1, Encoding::MuLaw).unwrap(),
            40,
            500,
            Overflow::DropOldest,
        ),
    ];
    let mut budget = 0usize;
    let mut subs = Vec::new();
    for (src, fmt, chunk, maxq, of) in specs {
        let spec = SubSpec {
            source: src.clone(),
            format: fmt,
            chunk_ms: chunk,
            max_queue_ms: maxq,
            overflow: of,
        };
        budget += spec.budget_bytes().unwrap();
        subs.push(r.subscribe(src, fmt, chunk, maxq, of));
    }
    let pa = voice(440.0, 0.3, 50);
    for i in 0..180_000usize {
        let voices = if i % 500 < 50 {
            vec![r.packet(SA, &pa[i % 500])]
        } else {
            vec![r.packet(SA, &SILENT_FRAME)]
        };
        r.tick(voices);
        let queued: usize = subs.iter().map(|s| s.queue.queued_bytes()).sum();
        assert!(queued <= budget, "tick {i}: {queued} > {budget}");
    }
    for s in &subs {
        assert!(s.queue.queued_chunks() as usize <= s.queue.max_chunks());
        assert!(!s.queue.is_closed());
    }
    let c = r.p.counts();
    assert_eq!(c.ticks_real, 180_000);
    assert_eq!(c.ticks_skipped, 0);
    assert_eq!(c.decoders_active, 1);
}

#[test]
fn router_creates_streams_on_first_voice_and_closes_on_leave() {
    let mut r = Rig::new();
    r.map(SA, A);
    r.map(SB, B);
    let rq = RouterQueue::new(token(&r.hub));
    let spec = RouterSpec {
        format: PCM16_48K,
        chunk_ms: 100,
        max_queue_ms: 2_000,
        overflow: Overflow::DropOldest,
        include: None,
        ignore_bots: true,
        close_on_leave: true,
    };
    r.p.apply(RxCmd::AddRouter {
        id: 1,
        spec,
        queue: rq.clone(),
    });
    r.p.apply(RxCmd::SetBots(vec![(B, true)]));
    r.timer();
    let pa = voice(440.0, 0.3, 40);
    for _ in 0..3 {
        let v = vec![r.packet(SA, &SILENT_FRAME), r.packet(SB, &pa[0])];
        r.tick(v);
    }
    assert!(
        matches!(rq.try_pop_or_arm(), Pop::Empty),
        "bots are ignored"
    );
    let first_voice_ms = r.p.timeline_ms();
    for p in pa.iter().take(20) {
        let v = vec![r.packet(SA, p), r.packet(SB, p)];
        r.tick(v);
    }
    let sa = match rq.try_pop_or_arm() {
        Pop::Item(s) => s,
        other => panic!("{other:?}"),
    };
    assert_eq!(sa.queue.user_id(), Some(A));
    assert!(
        matches!(rq.try_pop_or_arm(), Pop::Empty),
        "one stream per user"
    );
    let v = drain(&sa);
    assert!(v.len() >= 3);
    // The first chunk covers the first voiced tick (no cut-off by the 100 ms chunk).
    assert!(v[0].chunk.meta.timeline_ms <= first_voice_ms);
    assert!(v[0].chunk.meta.timeline_ms + 100 > first_voice_ms);
    assert!(v[0].chunk.meta.speaking);
    r.p.on_input(RxInput::UserLeft { user_id: A }, r.now);
    assert_eq!(sa.queue.close_reason(), Some(CloseReason::UserLeft));
    let ev = r.events();
    let tail: Vec<_> = ev
        .iter()
        .filter(|e| {
            matches!(
                e,
                EventData::SpeakingStopped { user_id: A, .. } | EventData::UserLeft { user_id: A }
            )
        })
        .collect();
    assert!(matches!(tail[0], EventData::SpeakingStopped { .. }));
    assert!(matches!(tail[1], EventData::UserLeft { .. }));
    r.tick(vec![]);
    assert_eq!(r.p.counts().subscriptions, 0);
    // A user who returns gets a new stream.
    r.map(SA, A);
    for p in pa.iter().skip(20).take(5) {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    match rq.try_pop_or_arm() {
        Pop::Item(s) => assert_eq!(s.queue.user_id(), Some(A)),
        other => panic!("{other:?}"),
    }
    assert_eq!(r.p.counts().router_streams, 2);
    assert_eq!(rq.dropped(), 0);
    // Closing the router unregisters it.
    rq.close(true);
    r.tick(vec![]);
    let c = r.p.counts();
    assert_eq!((c.routers, c.subscriptions), (0, 0));
}

#[test]
fn user_left_prunes_ssrc_state_after_one_second() {
    let mut r = Rig::new();
    r.map(SA, A);
    let ua = r.user(A, PCM16_24K, 20);
    let pa = voice(440.0, 0.3, 30);
    for p in pa.iter().take(10) {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    assert_eq!(r.p.counts().decoders_active, 1);
    r.p.on_input(RxInput::UserLeft { user_id: A }, r.now);
    let t = r.timer();
    assert!(
        t.next_wake
            .is_some_and(|w| w <= r.now + Duration::from_secs(1))
    );
    r.idle(980);
    assert_eq!(
        r.p.counts().decoders_active,
        1,
        "kept while the jitter buffer drains"
    );
    r.idle(40);
    assert_eq!(r.p.counts().decoders_active, 0);
    drain(&ua);
    // The mapping is gone: packets of the old SSRC are unmapped now.
    for p in pa.iter().skip(10).take(10) {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    assert!(drain(&ua).iter().all(|d| !d.chunk.meta.speaking));
    assert!(
        !ua.queue.is_closed(),
        "a per-user stream never ends on its own"
    );
}

#[test]
fn idle_decoder_state_is_pruned_after_the_timeout() {
    let mut r = Rig::with(|c| c.decode_state_timeout = Duration::from_secs(2));
    r.map(SA, A);
    let _ua = r.user(A, PCM16_24K, 20);
    let pa = voice(440.0, 0.3, 5);
    for p in &pa {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    assert_eq!(r.p.counts().decoders_active, 1);
    r.idle(3_100);
    assert_eq!(r.p.counts().decoders_active, 0);
}

#[test]
fn voice_threshold_gates_speaking() {
    let mut r = Rig::with(|c| c.voice_threshold_dbfs = Some(-40.0));
    r.map(SA, A);
    r.map(SB, B);
    let quiet = voice(440.0, 0.001, 10);
    let loud = voice(440.0, 0.3, 10);
    for i in 0..10 {
        let v = vec![r.packet(SA, &quiet[i]), r.packet(SB, &loud[i])];
        r.tick(v);
    }
    let starts: Vec<u64> = r
        .events()
        .iter()
        .filter_map(|e| match e {
            EventData::SpeakingStarted { user_id, .. } => Some(*user_id),
            _ => None,
        })
        .collect();
    assert_eq!(starts, [B]);
    assert_eq!(
        r.p.counts().decoders_active,
        2,
        "a threshold decodes everyone"
    );
}

#[test]
fn floor_policy_mixes_only_the_holder() {
    let mut r = Rig::new();
    r.map(SA, A);
    r.map(SB, B);
    let f = MixFilter {
        policy: MixPolicy::Floor { release_ms: 200 },
        ..MixFilter::everyone()
    };
    let sub = r.mixed(f, PCM16_24K, 20);
    let pa = voice(440.0, 0.3, 40);
    let pb = voice(660.0, 0.3, 40);
    for i in 0..20 {
        let mut v = vec![r.packet(SA, &pa[i])];
        if i >= 5 {
            v.push(r.packet(SB, &pb[i]));
        }
        r.tick(v);
    }
    let v = drain(&sub);
    assert!(
        v.iter()
            .all(|d| d.chunk.meta.speakers.iter().all(|s| *s == A))
    );
    assert!(v.iter().filter(|d| d.chunk.meta.speaking).count() >= 15);
}

#[test]
fn bad_packets_and_losses_are_counted() {
    let mut r = Rig::new();
    r.map(SA, A);
    let ua = r.user(A, PCM16_24K, 20);
    let pa = voice(440.0, 0.3, 10);
    for p in pa.iter().take(5) {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    r.tick(vec![RawVoice::Lost { ssrc: SA }]);
    r.tick(vec![RawVoice::Packet {
        ssrc: SA,
        packet: Bytes::from_static(&[0x80, 1, 2]),
        payload_offset: 0,
        payload_end: 99,
    }]);
    let c = r.p.counts();
    assert_eq!(c.bad_packets, 1);
    assert_eq!(c.plc_frames, 2 * 960);
    let v = drain(&ua);
    assert!(v.iter().any(|d| d.chunk.meta.concealed_ms == 20));
}

#[test]
fn close_all_closes_every_queue() {
    let mut r = Rig::new();
    let s1 = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    r.timer();
    let s2 = r.user(A, PCM16_24K, 20); // still pending
    let rq = RouterQueue::new(token(&r.hub));
    r.p.apply(RxCmd::AddRouter {
        id: 9,
        spec: RouterSpec {
            format: PCM16_24K,
            chunk_ms: 20,
            max_queue_ms: 200,
            overflow: Overflow::DropOldest,
            include: None,
            ignore_bots: false,
            close_on_leave: false,
        },
        queue: rq.clone(),
    });
    r.p.close_all(CloseReason::InternalError);
    assert_eq!(s1.queue.close_reason(), Some(CloseReason::InternalError));
    assert_eq!(s2.queue.close_reason(), Some(CloseReason::InternalError));
    assert!(rq.is_closed());
    let c = r.p.counts();
    assert_eq!(
        (c.subscriptions, c.lanes, c.converters, c.buses),
        (0, 0, 0, 0)
    );
}

#[test]
fn disabled_receive_ignores_ticks() {
    let mut r = Rig::with(|c| c.enabled = false);
    let sub = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    r.timer();
    r.idle(1_000);
    r.tick(vec![]);
    assert!(drain(&sub).is_empty());
    assert_eq!(r.p.counts().ticks_real, 0);
}

// ── review regressions ──────────────────────────────────────────────────────────────────────────

fn router_spec(chunk_ms: u32, close_on_leave: bool) -> RouterSpec {
    RouterSpec {
        format: PCM16_48K,
        chunk_ms,
        max_queue_ms: 2_000,
        overflow: Overflow::DropOldest,
        include: None,
        ignore_bots: true,
        close_on_leave,
    }
}

fn pop_stream(rq: &RouterQueue) -> Option<Subscription> {
    match rq.try_pop_or_arm() {
        Pop::Item(s) => Some(s),
        Pop::Empty => None,
        Pop::Closed(r) => panic!("router closed: {r:?}"),
    }
}

/// Lanes keep no reference to emitted chunks: once delivered, the subscriber's queue (and then
/// the consumer) is the only owner, so `skip_to_live`/close/eviction free a chunk at once.
#[test]
fn delivered_chunks_are_owned_by_the_subscribers_only() {
    let mut r = Rig::new();
    let s1 = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    let s2 = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    r.timer();
    r.idle(200);
    let v1 = drain(&s1);
    assert!(!v1.is_empty());
    let last = v1.last().unwrap().chunk.clone();
    assert_eq!(
        Arc::strong_count(&last),
        3,
        "s1's copy, `last` and s2's queue"
    );
    assert_eq!(s2.queue.skip_to_live(true), v1.len() as u64 * 20);
    drop(v1);
    assert_eq!(Arc::strong_count(&last), 1, "no lane keeps the last chunk");
}

/// A reused subscription id must not leave the second stream open with no chunks forever.
#[test]
fn duplicate_subscription_id_closes_the_new_stream() {
    let mut r = Rig::new();
    let first = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    let spec = SubSpec {
        source: RxSource::User(A),
        format: PCM16_24K,
        chunk_ms: 20,
        max_queue_ms: 2_000,
        overflow: Overflow::DropOldest,
    };
    let dup = SubQueue::new(&spec, token(&r.hub));
    r.p.apply(RxCmd::AddSub {
        id: first.id,
        spec,
        queue: dup.clone(),
    });
    r.timer();
    assert_eq!(dup.close_reason(), Some(CloseReason::InternalError));
    assert!(!first.queue.is_closed());
    let c = r.p.counts();
    assert_eq!((c.subscriptions, c.lanes), (1, 1));
}

/// Bus ids wrap after 2³² buses; a wrapped id still in use must not overwrite that bus.
#[test]
fn wrapped_bus_ids_never_overwrite_a_live_bus() {
    let mut r = Rig::new();
    r.map(SA, A);
    r.map(SB, B);
    r.p.set_next_bus_id(u32::MAX);
    let everyone = r.mixed(MixFilter::everyone(), PCM16_24K, 20);
    r.timer();
    r.p.set_next_bus_id(u32::MAX); // the counter wrapped onto the live bus
    let only_b = r.mixed(filter_excluding(&[A]), PCM16_24K, 20);
    r.timer();
    let c = r.p.counts();
    assert_eq!((c.buses, c.lanes), (2, 2));
    let pa = voice(440.0, 0.3, 10);
    let pb = voice(660.0, 0.3, 10);
    for i in 0..10 {
        let v = vec![r.packet(SA, &pa[i]), r.packet(SB, &pb[i])];
        r.tick(v);
    }
    let (ve, vb) = (drain(&everyone), drain(&only_b));
    assert!(ve.iter().any(|d| *d.chunk.meta.speakers == [A, B]));
    assert!(vb.iter().all(|d| !d.chunk.meta.speakers.contains(&A)));
    assert!(vb.iter().any(|d| *d.chunk.meta.speakers == [B]));
    drop(everyone);
    drop(only_b);
    r.timer();
    let c = r.p.counts();
    assert_eq!((c.buses, c.lanes, c.converters), (0, 0, 0));
}

/// `close_on_leave=False`: a live stream survives the user's leave and is reused when they come
/// back; a stream the consumer closed is forgotten on leave (bounded bookkeeping), so the returning
/// user starts a new presence with a new stream.
#[test]
fn router_without_close_on_leave_forgets_dead_streams_on_leave() {
    let mut r = Rig::new();
    r.map(SA, A);
    r.map(SB, B);
    let rq = RouterQueue::new(token(&r.hub));
    r.p.apply(RxCmd::AddRouter {
        id: 1,
        spec: router_spec(20, false),
        queue: rq.clone(),
    });
    r.timer();
    let pa = voice(440.0, 0.3, 40);
    for p in pa.iter().take(5) {
        let v = vec![r.packet(SA, p), r.packet(SB, p)];
        r.tick(v);
    }
    let mut got: HashMap<u64, Subscription> = HashMap::new();
    while let Some(s) = pop_stream(&rq) {
        got.insert(s.queue.user_id().unwrap(), s);
    }
    assert_eq!(got.len(), 2);
    // The consumer closes A's stream; B's stays open.
    got.remove(&A).unwrap().close();
    r.timer();
    for u in [A, B] {
        r.p.on_input(RxInput::UserLeft { user_id: u }, r.now);
    }
    assert!(!got[&B].queue.is_closed(), "close_on_leave=False keeps it");
    r.idle(1_100); // SSRC state and mappings pruned
    r.map(SA, A);
    r.map(SB, B);
    for p in pa.iter().skip(5).take(5) {
        let v = vec![r.packet(SA, p), r.packet(SB, p)];
        r.tick(v);
    }
    let again = pop_stream(&rq).expect("A starts a new presence");
    assert_eq!(again.queue.user_id(), Some(A));
    assert!(pop_stream(&rq).is_none(), "B is still on its first stream");
    assert!(drain(&got[&B]).iter().any(|d| d.chunk.meta.speaking));
    assert_eq!(r.p.counts().router_streams, 3);
}

/// `close_all` (tick-panic recovery) drops consumers but keeps bot flags queued before it.
#[test]
fn close_all_keeps_pending_bot_flags() {
    let mut r = Rig::new();
    r.map(SA, A);
    r.map(SB, B);
    r.p.apply(RxCmd::SetBots(vec![(B, true)]));
    r.p.close_all(CloseReason::InternalError);
    let mix = r.mixed(MixFilter::everyone(), PCM16_24K, 20); // ignore_bots
    r.timer();
    let pa = voice(440.0, 0.3, 10);
    for p in &pa {
        let v = vec![r.packet(SA, p), r.packet(SB, p)];
        r.tick(v);
    }
    let v = drain(&mix);
    assert!(v.iter().any(|d| d.chunk.meta.speaking));
    assert!(v.iter().all(|d| !d.chunk.meta.speakers.contains(&B)));
}

fn speaking_events(ev: &[EventData]) -> Vec<String> {
    ev.iter()
        .filter_map(|e| match e {
            EventData::SpeakingStarted { user_id, .. } => Some(format!("start {user_id}")),
            EventData::SpeakingStopped { user_id, .. } => Some(format!("stop {user_id}")),
            EventData::UserLeft { user_id } => Some(format!("left {user_id}")),
            _ => None,
        })
        .collect()
}

/// songbird keeps delivering a departed user's playout buffer for a while after
/// `ClientDisconnect` (the mapping is kept 1 s for it): that tail is audio, never speech. It
/// neither restarts a talk spurt after `UserLeft` nor opens a second router stream for the ended
/// presence (which no further `UserLeft` would ever close). A returning user is voiced again.
#[test]
fn playout_tail_after_user_left_opens_no_stream_and_restarts_no_spurt() {
    let mut r = Rig::new();
    r.map(SA, A);
    r.map(SB, B);
    let closing = RouterQueue::new(token(&r.hub));
    r.p.apply(RxCmd::AddRouter {
        id: 1,
        spec: router_spec(20, true),
        queue: closing.clone(),
    });
    let keeping = RouterQueue::new(token(&r.hub));
    r.p.apply(RxCmd::AddRouter {
        id: 2,
        spec: router_spec(20, false),
        queue: keeping.clone(),
    });
    let ua = r.user(A, PCM16_24K, 20);
    r.timer();
    let pa = voice(440.0, 0.3, 60);
    for p in pa.iter().take(10) {
        let v = vec![r.packet(SA, p), r.packet(SB, p)];
        r.tick(v);
    }
    let first = pop_stream(&closing).expect("A's stream");
    let _first_b = pop_stream(&closing).expect("B's stream");
    let kept_a = pop_stream(&keeping).expect("A's stream");
    let _kept_b = pop_stream(&keeping).expect("B's stream");
    let kept_a = if kept_a.queue.user_id() == Some(A) {
        kept_a
    } else {
        _kept_b
    };
    // The consumer of the `close_on_leave=False` router closes A's stream.
    kept_a.close();
    let v = vec![r.packet(SB, &pa[10])];
    r.tick(v);
    drain(&ua);
    r.events();
    r.p.on_input(RxInput::UserLeft { user_id: A }, r.now);
    assert_eq!(first.queue.close_reason(), Some(CloseReason::UserLeft));
    // The playout tail: three more voiced packets of the departed user.
    for p in pa.iter().skip(11).take(3) {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    r.idle(500);
    assert!(
        pop_stream(&closing).is_none(),
        "no stream for an ended presence"
    );
    assert!(pop_stream(&keeping).is_none(), "nor a re-created one");
    assert_eq!(r.p.counts().router_streams, 4);
    let ev = speaking_events(&r.events());
    let of_a: Vec<&String> = ev.iter().filter(|e| e.ends_with("1001")).collect();
    assert_eq!(of_a, ["stop 1001", "left 1001"]);
    let tail = drain(&ua);
    assert!(
        tail.iter()
            .any(|d| d.chunk.data.as_bytes().iter().any(|b| *b != 0)),
        "the tail is still delivered as audio"
    );
    assert!(tail.iter().all(|d| !d.chunk.meta.speaking));
    // The user comes back: voiced again, with a new presence and stream.
    r.idle(1_000);
    r.map(SA, A);
    for p in pa.iter().skip(20).take(5) {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    let again = pop_stream(&closing).expect("a returning user gets a new stream");
    assert_eq!(again.queue.user_id(), Some(A));
    assert_eq!(
        pop_stream(&keeping).map(|s| s.queue.user_id()),
        Some(Some(A))
    );
    assert_eq!(speaking_events(&r.events()), ["start 1001"]);
    assert!(drain(&ua).iter().any(|d| d.chunk.meta.speaking));
}

/// A returning user's `Map` (before the prune) ends the departed state at once.
#[test]
fn a_map_before_the_prune_makes_a_departed_user_voiced_again() {
    let mut r = Rig::new();
    r.map(SA, A);
    let rq = RouterQueue::new(token(&r.hub));
    r.p.apply(RxCmd::AddRouter {
        id: 1,
        spec: router_spec(20, true),
        queue: rq.clone(),
    });
    r.timer();
    let pa = voice(440.0, 0.3, 20);
    for p in pa.iter().take(3) {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    let first = pop_stream(&rq).unwrap();
    r.p.on_input(RxInput::UserLeft { user_id: A }, r.now);
    assert!(first.queue.is_closed());
    r.tick(vec![]);
    r.map(SA, A); // back within the second (same SSRC: no `UserMapped`)
    for p in pa.iter().skip(3).take(3) {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    assert_eq!(pop_stream(&rq).unwrap().queue.user_id(), Some(A));
    r.idle(2_000);
    // The cancelled prune kept the mapping.
    let v = vec![r.packet(SA, &pa[10])];
    r.tick(v);
    assert!(
        speaking_events(&r.events())
            .iter()
            .filter(|e| *e == "start 1001")
            .count()
            >= 2
    );
}

/// When the media session moves to another channel, Discord sends no `ClientDisconnect` for the
/// users left behind: every present user leaves (events, `close_on_leave` streams), the SSRC map,
/// decoders and classification start empty, and an old SSRC is unmapped. Explicit subscriptions,
/// live `close_on_leave=False` streams and bot flags survive; the users heard on the new channel
/// get new streams.
#[test]
fn channel_change_ends_every_presence_and_the_ssrc_space() {
    let mut r = Rig::new();
    r.map(SA, A);
    r.map(SB, B);
    r.p.apply(RxCmd::SetBots(vec![(B, true)]));
    let closing = RouterQueue::new(token(&r.hub));
    r.p.apply(RxCmd::AddRouter {
        id: 1,
        spec: RouterSpec {
            ignore_bots: false,
            ..router_spec(20, true)
        },
        queue: closing.clone(),
    });
    let keeping = RouterQueue::new(token(&r.hub));
    r.p.apply(RxCmd::AddRouter {
        id: 2,
        spec: RouterSpec {
            ignore_bots: false,
            ..router_spec(20, false)
        },
        queue: keeping.clone(),
    });
    let ua = r.user(A, PCM16_24K, 20);
    let mix = r.mixed(MixFilter::everyone(), PCM16_24K, 20); // ignore_bots
    r.timer();
    let pa = voice(440.0, 0.3, 60);
    for p in pa.iter().take(5) {
        let v = vec![r.packet(SA, p), r.packet(SB, p)];
        r.tick(v);
    }
    let mut closing_streams: HashMap<u64, Subscription> = HashMap::new();
    while let Some(s) = pop_stream(&closing) {
        closing_streams.insert(s.queue.user_id().unwrap(), s);
    }
    let mut kept: HashMap<u64, Subscription> = HashMap::new();
    while let Some(s) = pop_stream(&keeping) {
        kept.insert(s.queue.user_id().unwrap(), s);
    }
    assert_eq!((closing_streams.len(), kept.len()), (2, 2));
    // A stream the consumer already closed is forgotten by the change.
    kept.remove(&B).unwrap().close();
    r.timer();
    let before = r.p.counts();
    assert_eq!((before.subscriptions, before.decoders_active), (5, 2));
    r.unclassified.lock().clear();
    r.events();

    r.p.on_input(RxInput::ChannelChanged, r.now);
    assert_eq!(
        speaking_events(&r.events()),
        ["stop 1001", "left 1001", "stop 1002", "left 1002"]
    );
    for s in closing_streams.values() {
        assert_eq!(s.queue.close_reason(), Some(CloseReason::UserLeft));
    }
    assert!(!kept[&A].queue.is_closed(), "close_on_leave=False keeps it");
    assert!(!ua.queue.is_closed(), "explicit subscriptions survive");
    r.tick(vec![]);
    let c = r.p.counts();
    assert_eq!((c.subscriptions, c.decoders_active), (3, 0));
    assert_eq!(c.router_streams, 4);
    drain(&ua);
    drain(&kept[&A]);

    // The old SSRC means nothing on the new session (no mapping yet): unmapped audio.
    for p in pa.iter().skip(5).take(3) {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    assert!(drain(&ua).iter().all(|d| !d.chunk.meta.speaking));
    assert!(pop_stream(&closing).is_none());
    assert!(speaking_events(&r.events()).is_empty());

    // On the new channel A transmits on another SSRC; B (a bot) too.
    const SA2: u32 = 0xA2;
    const SB2: u32 = 0xB2;
    r.map(SA2, A);
    r.map(SB2, B);
    assert_eq!(*r.unclassified.lock(), [A, B], "classified again");
    for p in pa.iter().skip(8).take(5) {
        let v = vec![r.packet(SA2, p), r.packet(SB2, p)];
        r.tick(v);
    }
    let mut again: Vec<u64> = std::iter::from_fn(|| pop_stream(&closing))
        .map(|s| s.queue.user_id().unwrap())
        .collect();
    again.sort_unstable();
    assert_eq!(again, [A, B], "new presences, new streams");
    let reopened: Vec<u64> = std::iter::from_fn(|| pop_stream(&keeping))
        .map(|s| s.queue.user_id().unwrap())
        .collect();
    assert_eq!(
        reopened,
        [B],
        "A keeps its live close_on_leave=False stream"
    );
    assert!(drain(&ua).iter().any(|d| d.chunk.meta.speaking));
    assert!(drain(&kept[&A]).iter().any(|d| d.chunk.meta.speaking));
    let m = drain(&mix);
    assert!(m.iter().any(|d| d.chunk.meta.speakers.contains(&A)));
    assert!(
        m.iter().all(|d| !d.chunk.meta.speakers.contains(&B)),
        "bot flags survive the change"
    );
}

/// A worker that runs late handles its tick backlog in one step: every queued tick is judged by
/// its own arrival, so ticks that arrived on time fill their own slots instead of being pushed
/// behind synthesised silence (no fake `gap_ms` in the middle of speech).
#[test]
fn a_late_worker_backlog_is_not_preceded_by_synthetic_silence() {
    let mut r = Rig::new();
    r.map(SA, A);
    let ua = r.user(A, PCM16_24K, 20);
    r.timer();
    let pa = voice(440.0, 0.3, 120);
    let mut it = pa.iter();
    for p in it.by_ref().take(50) {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    // Ten ticks arrive on time, 20 ms apart, while the worker is busy elsewhere for 205 ms.
    let mut backlog = Vec::new();
    for p in it.by_ref().take(10) {
        r.advance(20);
        backlog.push(RawTick {
            arrived: r.now,
            voices: std::iter::once(r.packet(SA, p)).collect(),
            silent: SmallVec::new(),
        });
    }
    r.advance(25);
    for raw in backlog {
        r.p.on_input(RxInput::Tick(raw), r.now);
    }
    r.timer();
    for p in it.by_ref().take(50) {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    let v = drain(&ua);
    assert!(
        v.iter().all(|d| d.chunk.meta.gap_ms == 0),
        "fake gap at {:?}",
        v.iter()
            .filter(|d| d.chunk.meta.gap_ms > 0)
            .map(|d| d.chunk.meta.timeline_ms)
            .collect::<Vec<_>>()
    );
    assert_eq!(r.p.counts().ticks_synthetic, 0);
    let t = timelines(&v);
    assert!(t.windows(2).all(|w| w[1] == w[0] + 20), "continuous");
    // The timeline still trails real time by the usual slack, not by the worker's delay.
    let expected = (r.now - r.t0).as_millis() as u64;
    assert!(r.p.timeline_ms() <= expected && r.p.timeline_ms() + 60 >= expected);
}

/// `ConnWorker::recover` moves the connection state into the rebuilt pipeline: mappings, bot
/// flags, departed users, speaking spurts, the timeline and the router stream ids.
#[test]
fn carry_moves_connection_state_into_a_fresh_pipeline() {
    let mut r = Rig::new();
    r.map(SA, A);
    r.map(SB, B);
    r.p.apply(RxCmd::SetBots(vec![(B, true)]));
    let _ua = r.user(A, PCM16_24K, 20);
    r.timer();
    let pa = voice(440.0, 0.3, 30);
    for p in pa.iter().take(5) {
        let v = vec![r.packet(SA, p)];
        r.tick(v);
    }
    let timeline = r.p.timeline_ms();
    r.events();
    r.p.close_all(CloseReason::InternalError);
    let carry = r.p.take_carry();
    let mut fresh = RxPipeline::new(
        cfg(r.t0),
        EventHub::new(),
        CtrlQueue::new(r.hub.clone()),
        r.unclassified.clone(),
    );
    fresh.restore_carry(carry);
    r.p = fresh;
    assert_eq!(r.p.timeline_ms(), timeline, "the timeline continues");
    let ua = r.user(A, PCM16_24K, 20);
    let mix = r.mixed(MixFilter::everyone(), PCM16_24K, 20); // ignore_bots
    r.timer();
    for p in pa.iter().skip(5).take(10) {
        let v = vec![r.packet(SA, p), r.packet(SB, p)];
        r.tick(v);
    }
    assert!(
        drain(&ua).iter().any(|d| d.chunk.meta.speaking),
        "A still mapped"
    );
    let m = drain(&mix);
    assert!(m.iter().any(|d| d.chunk.meta.speakers.contains(&A)));
    assert!(
        m.iter().all(|d| !d.chunk.meta.speakers.contains(&B)),
        "B still a bot"
    );
    // A's spurt carried over: no second `SpeakingStarted`; it ends after the hangover.
    r.idle(400);
    let ev = speaking_events(&r.events());
    let of_a: Vec<&String> = ev.iter().filter(|e| e.ends_with("1001")).collect();
    assert_eq!(of_a, ["stop 1001"]);
}
