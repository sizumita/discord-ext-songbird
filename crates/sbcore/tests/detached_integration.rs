//! End-to-end tests of a full connection without Discord (§13.2): detached `ConnShared`s driven
//! by `testing::Harness` (manual clock, the real ConnWorker), receive input injected through the
//! same tick channel `Dispatch` feeds, and send output pulled through `MixerHarness` exactly as
//! songbird's mixer would.
//!
//! Every test holds `LOCK`: they assert the process-wide live-object counters.
#![cfg(feature = "test-hooks")]

use std::f64::consts::PI;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sbcore::conn::{ConnShared, RawVoiceSettings, VoiceSettings};
use sbcore::error::{CloseReason, CoreError};
use sbcore::events::{EventData, EventQueue, KindMask, Pop};
use sbcore::format::{Encoding, Format, MULAW_8K, PCM16_16K, PCM16_24K, PCM16_48K_STEREO};
use sbcore::rx::{Delivered, MixFilter, Overflow, RouterSpec, RxSource, SubQueue, SubSpec};
use sbcore::testing::{
    self, DriverEvent, Harness, MixerHarness, encode_opus, inject_driver_event, inject_map,
    inject_tick, inject_user_left,
};
use sbcore::tracks::{PlayOpts, TrackEndReason, TrackKind, TrackRef};
use sbcore::tx::encoded::{ENCODED_WORKERS, EncodedSpec};
use sbcore::tx::memory::MemPcm;
use sbcore::tx::opus_stream::OpusStreamSpec;
use sbcore::tx::queue::{ClearOpts, DisconnectPolicy, PlaybackSpec, WriteMeta};
use sbcore::tx::source::{EncodedFactory, SourceSpec};
use sbcore::tx::{CloseMode, LiveSource};

static LOCK: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn nz(v: u64) -> NonZeroU64 {
    NonZeroU64::new(v).unwrap()
}

const USER_A: u64 = 1_001;
const USER_B: u64 = 1_002;
const SSRC_A: u32 = 11;
const SSRC_B: u32 = 12;

/// A detached connection and its harness.
struct Rig {
    conn: Arc<ConnShared>,
    h: Harness,
}

impl Rig {
    fn new() -> Self {
        Self::with(RawVoiceSettings::default())
    }

    fn with(raw: RawVoiceSettings) -> Self {
        let settings = VoiceSettings::validate(raw).unwrap();
        let (conn, h) = Harness::detached(nz(1), nz(2), settings, None, true).unwrap();
        Rig { conn, h }
    }

    /// Joins channel 10; the connect attempt succeeds once the driver reports it (injected).
    fn connect(&self) {
        self.conn.request_join(nz(10), false, false);
        self.conn.on_voice_state("session", Some(10));
        self.conn
            .on_voice_server("token", Some("wss://voice.example.test/"));
        assert_eq!(self.conn.status().state, "connecting");
        inject_driver_event(&self.conn, DriverEvent::Connect { ssrc: 4_242 }).unwrap();
        let st = self.conn.status();
        assert_eq!((st.state, st.ssrc), ("connected", Some(4_242)));
        self.step(0);
    }

    fn step(&self, ms: u64) {
        self.h.step(ms).unwrap();
    }

    fn stat(&self, key: &str) -> u64 {
        self.conn
            .stats()
            .into_iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("no stat {key}"))
    }
}

fn drain_sub(q: &SubQueue) -> Vec<Delivered> {
    let mut out = Vec::new();
    while let Pop::Item(d) = q.try_pop() {
        out.push(d);
    }
    out
}

fn drain_events(q: &EventQueue) -> Vec<EventData> {
    let mut out = Vec::new();
    while let Pop::Item(e) = q.try_pop() {
        out.push(e);
    }
    out
}

/// `ms` of a sine at 48 kHz stereo, PCM16LE.
fn sine_48k_stereo(ms: u32, hz: f64, amp: f64) -> Vec<u8> {
    let frames = 48 * ms as usize;
    let mut out = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let s = (amp * 32_767.0 * (2.0 * PI * hz * i as f64 / 48_000.0).sin()).round() as i16;
        out.extend_from_slice(&s.to_le_bytes());
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// `ms` of a sine at `rate` Hz mono, PCM16LE.
fn sine_mono(rate: u32, ms: u32, hz: f64, amp: f64) -> Vec<u8> {
    let frames = (rate / 1_000 * ms) as usize;
    let mut out = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let s =
            (amp * 32_767.0 * (2.0 * PI * hz * i as f64 / f64::from(rate)).sin()).round() as i16;
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// A 16-bit PCM WAV file.
fn wav(rate: u32, channels: u16, pcm: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(44 + pcm.len());
    let block = channels * 2;
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    v.extend_from_slice(&block.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    v.extend_from_slice(pcm);
    v
}

fn rms(samples: &[f32]) -> f64 {
    let sum: f64 = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    (sum / samples.len().max(1) as f64).sqrt()
}

fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

fn playback_spec(format: Format) -> PlaybackSpec {
    PlaybackSpec {
        format,
        max_buffer_ms: 2_000,
        prebuffer_ms: 0,
        idle_ms: 100,
        auto_pause: true,
        max_gap_ms: 0,
        output_latency_ms: 40,
        volume: 1.0,
        when_disconnected: DisconnectPolicy::Buffer,
    }
}

fn pcm_write(q: &sbcore::tx::queue::PcmQueue, data: &[u8]) {
    q.write(data, WriteMeta::default()).unwrap();
}

fn pull(mh: &MixerHarness, ticks: usize) -> Vec<f32> {
    let mut out = Vec::new();
    mh.pull(ticks, &mut out).unwrap();
    out
}

fn wait_real(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    f()
}

/// Every process-wide live counter except songbird's scheduler.
fn live() -> Vec<(&'static str, u64)> {
    sbcore::runtime::live_counts()
        .into_iter()
        .filter(|(k, _)| *k != "songbird_live_tasks")
        .chain([(
            "EncodedWorkers",
            ENCODED_WORKERS.load(std::sync::atomic::Ordering::Acquire) as u64,
        )])
        .collect()
}

/// Waits until no connection object, queue, reader, track or decode thread is alive (an earlier
/// test's decode threads exit asynchronously once their ring closed). Tests in this binary are
/// serialised, so "nothing alive" is the baseline.
fn all_released() -> bool {
    wait_real(Duration::from_secs(5), || {
        live().iter().all(|(_, n)| *n == 0)
    })
}

fn sub_spec(source: RxSource, format: Format, chunk_ms: u32) -> SubSpec {
    SubSpec {
        source,
        format,
        chunk_ms,
        max_queue_ms: 5_000,
        overflow: Overflow::DropOldest,
    }
}

/// Checks size, contiguity and stream-clock fields of one subscription's chunks.
fn check_stream(name: &str, chunks: &[Delivered], format: Format, chunk_ms: u32) {
    assert!(!chunks.is_empty(), "{name}: no chunks");
    let bytes = format.bytes_for_ms(chunk_ms).unwrap();
    let origin = chunks[0].chunk.meta.timeline_ms;
    for (i, d) in chunks.iter().enumerate() {
        let m = &d.chunk.meta;
        assert_eq!(
            d.chunk.data.as_bytes().len(),
            bytes,
            "{name}: chunk {i} size"
        );
        assert_eq!(m.format, format, "{name}: format");
        assert_eq!(
            u64::from(m.frames),
            u64::from(format.rate.hz() / 1_000 * chunk_ms)
        );
        assert_eq!(
            m.timeline_ms,
            origin + i as u64 * u64::from(chunk_ms),
            "{name}: chunk {i} is not contiguous"
        );
        assert_eq!(d.start_ms, m.timeline_ms - origin, "{name}: stream clock");
        assert_eq!(d.seq, d.start_ms / u64::from(chunk_ms), "{name}: seq");
        assert_eq!(d.dropped_ms, 0, "{name}: nothing dropped");
    }
}

// ── receive ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn injected_rtp_reaches_per_user_and_mixed_streams_with_silence_fill() {
    let _serial = serial();
    let r = Rig::new();
    let events = r.conn.subscribe_events(256, KindMask::ALL).unwrap();
    r.connect();
    let user24 = r
        .conn
        .subscribe(sub_spec(RxSource::User(USER_A), PCM16_24K, 20))
        .unwrap();
    let mixed16 = r
        .conn
        .subscribe(sub_spec(
            RxSource::Mixed(MixFilter::everyone()),
            PCM16_16K,
            40,
        ))
        .unwrap();
    let mulaw8 = r
        .conn
        .subscribe(sub_spec(RxSource::User(USER_A), MULAW_8K, 20))
        .unwrap();
    let quiet = r
        .conn
        .subscribe(sub_spec(RxSource::User(USER_B), PCM16_24K, 20))
        .unwrap();
    // Every subscription is charged against the receive budget.
    let budget: usize = [&user24, &mixed16, &mulaw8, &quiet]
        .iter()
        .map(|s| {
            sub_spec(RxSource::User(0), s.queue.format(), s.queue.chunk_ms())
                .budget_bytes()
                .unwrap()
        })
        .sum();
    assert_eq!(r.stat("receive_budget_bytes"), budget as u64);
    inject_map(&r.conn, SSRC_A, USER_A).unwrap();
    inject_map(&r.conn, SSRC_B, USER_B).unwrap();
    r.step(0);
    assert_eq!(r.stat("subscriptions"), 4);

    // 1 s of a 440 Hz tone from user A (one RTP packet per 20 ms tick), B silent.
    let packets = encode_opus(&sine_48k_stereo(1_000, 440.0, 0.5)).unwrap();
    assert_eq!(packets.len(), 50);
    for p in &packets {
        inject_tick(&r.conn, vec![(SSRC_A, Some(p.clone()))], vec![SSRC_B]).unwrap();
        r.step(20);
    }
    // Then 1 s without any packet: the timeline is filled with synthetic silence.
    r.step(1_000);
    assert_eq!(r.stat("ticks_real"), 50);
    assert!(r.stat("ticks_synthetic") >= 40, "silence was synthesised");
    assert_eq!(r.stat("bad_packets"), 0);

    let a24 = drain_sub(&user24.queue);
    let m16 = drain_sub(&mixed16.queue);
    let a8 = drain_sub(&mulaw8.queue);
    let b24 = drain_sub(&quiet.queue);
    check_stream("user 24k", &a24, PCM16_24K, 20);
    check_stream("mixed 16k", &m16, PCM16_16K, 40);
    check_stream("user mulaw", &a8, MULAW_8K, 20);
    check_stream("quiet user", &b24, PCM16_24K, 20);
    // Subscribed before the first tick: every stream starts at the timeline origin and covers the
    // voiced second plus the synthesised one (synthesis lags real time by at most 140 ms).
    for (name, s, chunk) in [
        ("a24", &a24, 20),
        ("m16", &m16, 40),
        ("a8", &a8, 20),
        ("b24", &b24, 20),
    ] {
        assert_eq!(s[0].chunk.meta.timeline_ms, 0, "{name}");
        let end = s.last().unwrap().chunk.meta.timeline_ms + chunk;
        assert!((1_860..=2_000).contains(&end), "{name} ends at {end}");
    }

    // The voiced second: speaking chunks of user A at a sane level (after the decoder warms up).
    let voiced: Vec<&Delivered> = a24
        .iter()
        .filter(|d| d.chunk.meta.timeline_ms < 1_000)
        .collect();
    assert_eq!(voiced.len(), 50);
    for d in &voiced[2..] {
        let m = &d.chunk.meta;
        assert!(m.speaking && m.voiced_ms == 20, "{m:?}");
        assert_eq!(m.gap_ms, 0);
        assert_eq!(m.user_id, Some(USER_A));
        assert_eq!(&*m.speakers, &[USER_A]);
        assert!(
            m.level.rms_dbfs > -20.0 && m.level.rms_dbfs < -3.0,
            "{:?}",
            m.level
        );
    }
    for d in m16
        .iter()
        .filter(|d| (80..1_000).contains(&d.chunk.meta.timeline_ms))
    {
        assert!(d.chunk.meta.speaking && d.chunk.meta.speakers.contains(&USER_A));
        assert_eq!(d.chunk.meta.user_id, None);
        assert!(
            d.chunk.meta.level.rms_dbfs > -20.0,
            "{:?}",
            d.chunk.meta.level
        );
    }
    for d in a8
        .iter()
        .filter(|d| (40..1_000).contains(&d.chunk.meta.timeline_ms))
    {
        assert!(d.chunk.meta.speaking);
        assert!(
            d.chunk.data.as_bytes().iter().any(|b| *b != 0xFF),
            "µ-law audio"
        );
    }
    // The synthesised second: digital silence flagged as a gap (after the resamplers' tails).
    for (name, s, silence) in [
        ("a24", &a24, 0x00u8),
        ("a8", &a8, 0xFF),
        ("b24", &b24, 0x00),
    ] {
        let gaps: Vec<&Delivered> = s
            .iter()
            .filter(|d| d.chunk.meta.timeline_ms >= 1_060)
            .collect();
        assert!(gaps.len() >= 38, "{name}");
        for d in gaps {
            let m = &d.chunk.meta;
            assert_eq!(m.gap_ms, 20, "{name}: {m:?}");
            assert!(!m.speaking);
            assert!(
                d.chunk.data.as_bytes().iter().all(|b| *b == silence),
                "{name}: silence"
            );
        }
    }
    // User B never spoke: real (not synthetic) silent ticks while A spoke, then gaps.
    for d in b24.iter().filter(|d| d.chunk.meta.timeline_ms < 1_000) {
        assert!(!d.chunk.meta.speaking);
        assert_eq!(d.chunk.meta.gap_ms, 0);
        assert!(d.chunk.data.as_bytes().iter().all(|b| *b == 0));
    }

    // Events: the mapping, one spurt of A, nothing for B.
    let evs = drain_events(&events);
    assert!(evs.iter().any(|e| matches!(
        e,
        EventData::UserMapped {
            user_id: USER_A,
            ssrc: SSRC_A
        }
    )));
    let started: Vec<u64> = evs
        .iter()
        .filter_map(|e| match e {
            EventData::SpeakingStarted { user_id, .. } => Some(*user_id),
            _ => None,
        })
        .collect();
    assert_eq!(started, vec![USER_A]);
    assert!(evs.iter().any(|e| matches!(
        e,
        EventData::SpeakingStopped {
            user_id: USER_A,
            timeline_ms: 1_000,
            ..
        }
    )));

    // No packet for `stall_timeout` (2 s after the last one): `ReceiveStalled` exactly once, at
    // the timeline position of the last real tick.
    r.step(1_500);
    let stalls: Vec<u64> = drain_events(&events)
        .iter()
        .filter_map(|e| match e {
            EventData::ReceiveStalled { since_ms } => Some(*since_ms),
            _ => None,
        })
        .collect();
    assert_eq!(stalls, vec![980]);
    assert_eq!(r.stat("panics"), 0);

    // Closing a subscription unregisters it at the next tick and releases its budget.
    drop(quiet);
    r.step(20);
    assert_eq!(r.stat("subscriptions"), 3);
    assert_eq!(
        r.stat("receive_budget_bytes"),
        (budget
            - sub_spec(RxSource::User(0), PCM16_24K, 20)
                .budget_bytes()
                .unwrap()) as u64
    );
    r.conn.close(CloseReason::Requested);
    // T6: closed, but what is queued stays readable.
    assert_eq!(
        user24.queue.close_reason(),
        Some(CloseReason::ConnectionClosed)
    );
    assert!(!drain_sub(&user24.queue).is_empty());
    assert!(matches!(
        user24.queue.try_pop(),
        Pop::Closed(CloseReason::ConnectionClosed)
    ));
}

#[test]
fn router_creates_budgeted_streams_per_speaker() {
    let _serial = serial();
    let r = Rig::new();
    r.connect();
    let router = r
        .conn
        .subscribe_router(RouterSpec {
            format: PCM16_16K,
            chunk_ms: 20,
            max_queue_ms: 1_000,
            overflow: Overflow::DropOldest,
            include: None,
            ignore_bots: false,
            close_on_leave: true,
        })
        .unwrap();
    inject_map(&r.conn, SSRC_A, USER_A).unwrap();
    inject_map(&r.conn, SSRC_B, USER_B).unwrap();
    r.step(0);
    assert_eq!(
        r.stat("receive_budget_bytes"),
        0,
        "nothing until someone speaks"
    );
    let packets = encode_opus(&sine_48k_stereo(200, 300.0, 0.4)).unwrap();
    for (i, p) in packets.iter().enumerate() {
        let mut voices = vec![(SSRC_A, Some(p.clone()))];
        if i >= 5 {
            voices.push((SSRC_B, Some(p.clone())));
        }
        inject_tick(&r.conn, voices, vec![]).unwrap();
        r.step(20);
    }
    let mut streams = Vec::new();
    while let Pop::Item(s) = router.queue.try_pop_or_arm() {
        streams.push(s);
    }
    let users: Vec<Option<u64>> = streams.iter().map(|s| s.queue.user_id()).collect();
    assert_eq!(users, vec![Some(USER_A), Some(USER_B)]);
    // Router streams are real subscriptions: budgeted, registered and counted.
    let one = RouterSpec {
        format: PCM16_16K,
        chunk_ms: 20,
        max_queue_ms: 1_000,
        overflow: Overflow::DropOldest,
        include: None,
        ignore_bots: false,
        close_on_leave: true,
    }
    .stream_budget_bytes()
    .unwrap();
    assert_eq!(r.stat("receive_budget_bytes"), 2 * one as u64);
    assert_eq!(r.stat("subscriptions"), 2);
    assert_eq!(r.stat("routers"), 1);
    // Each stream starts with the chunk holding the user's first voiced tick.
    let a = drain_sub(&streams[0].queue);
    let b = drain_sub(&streams[1].queue);
    check_stream("router A", &a, PCM16_16K, 20);
    check_stream("router B", &b, PCM16_16K, 20);
    assert_eq!(a[0].chunk.meta.timeline_ms, 0);
    assert_eq!(b[0].chunk.meta.timeline_ms, 100);
    assert!(
        a.iter()
            .skip(2)
            .all(|d| d.chunk.meta.user_id == Some(USER_A))
    );
    // `close_on_leave`: A leaves, its stream closes with `user_left`; B's survives.
    inject_user_left(&r.conn, USER_A).unwrap();
    r.step(20);
    assert_eq!(streams[0].queue.close_reason(), Some(CloseReason::UserLeft));
    assert!(!streams[1].queue.is_closed());
    r.conn.close(CloseReason::Requested);
    // T6 closed the registered router stream and the router itself.
    assert_eq!(
        streams[1].queue.close_reason(),
        Some(CloseReason::ConnectionClosed)
    );
    assert!(matches!(router.queue.try_pop_or_arm(), Pop::Closed(_)));
}

// ── send ────────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn playback_is_pulled_exactly_at_48k_and_resampled_from_24k() {
    let _serial = serial();
    let r = Rig::new();
    r.connect();
    let events = r.conn.subscribe_events(64, KindMask::ALL).unwrap();

    // 48 kHz stereo: no resampling, so the mixer sees exactly the written samples.
    let (q, track) = r
        .conn
        .open_playback(playback_spec(PCM16_48K_STEREO))
        .unwrap();
    assert_eq!(track.flags.kind, TrackKind::Playback);
    let mh = MixerHarness::take(&q).unwrap();
    assert_eq!(mh.channels(), 2);
    let out = pull(&mh, 3);
    assert_eq!(out.len(), 3 * 960 * 2);
    assert!(out.iter().all(|s| *s == 0.0));
    assert_eq!(mh.pulls(), 0, "an unwritten playback is paused: not pulled");
    assert!(
        testing::track_log(&track).is_empty(),
        "created paused, nothing else"
    );

    let data = sine_48k_stereo(100, 1_000.0, 0.5);
    pcm_write(&q, &data);
    assert_eq!(
        testing::track_log(&track),
        vec!["play"],
        "the first write resumes it"
    );
    let out = pull(&mh, 5);
    assert_eq!(mh.pulls(), 5);
    assert_eq!(out.len(), 5 * 960 * 2);
    let (samples, _) = data.as_chunks::<2>();
    for (i, (o, raw)) in out.iter().zip(samples).enumerate() {
        let want = f32::from(i16::from_le_bytes(*raw)) / 32_768.0;
        assert!((o - want).abs() < 1e-6, "sample {i}: {o} != {want}");
    }
    let st = q.stats();
    assert_eq!((st.written_ms, st.played_ms, st.buffered_ms), (100, 100, 0));
    assert_eq!(st.position_ms, 100);
    // Underrun: silence, counted.
    let out = pull(&mh, 1);
    assert!(out.iter().all(|s| *s == 0.0));
    assert_eq!(q.stats().underrun_ms, 20);

    // Barge-in: 500 ms queued, 100 ms played, then `clear()` frees the rest at once and the very
    // next pulled tick is silent.
    pcm_write(&q, &sine_48k_stereo(500, 1_000.0, 0.5));
    let out = pull(&mh, 5);
    assert!(rms(&out) > 0.3);
    let cleared = q.clear(ClearOpts::default()).unwrap();
    assert_eq!(cleared.discarded_ms, 400);
    assert_eq!(cleared.played_ms, 200);
    assert_eq!(q.stats().buffered_ms, 0);
    assert_eq!(q.stats().clears, 1);
    let out = pull(&mh, 1);
    assert!(
        out.iter().all(|s| *s == 0.0),
        "clear() takes effect within one tick"
    );
    assert_eq!(q.stats().played_ms, 200);

    // The housekeeper reports the playback: started, then idle after `idle_ms`, and pauses the track.
    r.step(200);
    let evs = drain_events(&events);
    let id = LiveSource::id(&*q);
    assert!(
        evs.iter()
            .any(|e| matches!(e, EventData::PlaybackStarted { playback_id } if *playback_id == id))
    );
    assert!(evs.iter().any(|e| matches!(
        e,
        EventData::PlaybackIdle { playback_id, played_ms: 200 } if *playback_id == id
    )));
    assert_eq!(testing::track_log(&track).last(), Some(&"pause"));

    // 24 kHz mono: resampled to 48 kHz, level within 0.2 dB, the tone's frequency kept.
    let (q24, track24) = r.conn.open_playback(playback_spec(PCM16_24K)).unwrap();
    let mh24 = MixerHarness::take(&q24).unwrap();
    assert_eq!(mh24.channels(), 1);
    pcm_write(&q24, &sine_mono(24_000, 1_000, 1_000.0, 0.5));
    let out = pull(&mh24, 50);
    assert_eq!(out.len(), 50 * 960);
    let steady = &out[2 * 960..49 * 960];
    let want = db(0.5 * 32_767.0 / 32_768.0 / 2f64.sqrt());
    assert!(
        (db(rms(steady)) - want).abs() < 0.2,
        "{} vs {want}",
        db(rms(steady))
    );
    let crossings = steady
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    let seconds = steady.len() as f64 / 48_000.0;
    assert!(
        (crossings as f64 / seconds - 1_000.0).abs() < 10.0,
        "{crossings} crossings"
    );
    assert_eq!(q24.stats().played_ms, 1_000);

    // close(drain): the queued audio plays out, then the reader ends and the track finishes.
    pcm_write(&q24, &sine_mono(24_000, 60, 1_000.0, 0.5));
    LiveSource::close(&*q24, CloseMode::Drain);
    let _ = pull(&mh24, 3);
    assert!(!mh24.ended());
    let _ = pull(&mh24, 1);
    assert!(mh24.ended(), "EOF after the drain");
    r.step(80);
    let evs = drain_events(&events);
    let id24 = LiveSource::id(&*q24);
    assert!(evs.iter().any(|e| matches!(
        e,
        EventData::PlaybackEnded { playback_id, reason: "drained" } if *playback_id == id24
    )));
    assert!(evs.iter().any(|e| matches!(
        e,
        EventData::TrackEnded { track_id, reason: TrackEndReason::Finished } if *track_id == track24.flags.id
    )));
    assert_eq!(r.stat("panics"), 0);
    drop(mh);
    r.conn.close(CloseReason::Requested);
    assert!(matches!(
        q.write(&data, WriteMeta::default()),
        Err(CoreError::StreamClosed(CloseReason::ConnectionClosed))
    ));
}

#[test]
fn opus_stream_is_attached_and_resumed_by_its_first_packet() {
    let _serial = serial();
    let r = Rig::new();
    r.connect();
    let (q, track) = r
        .conn
        .open_opus_stream(OpusStreamSpec {
            max_packets: 50,
            volume: 0.5,
        })
        .unwrap();
    assert!(testing::track_log(&track).is_empty());
    let packets = encode_opus(&sine_48k_stereo(60, 440.0, 0.5)).unwrap();
    q.send(&packets[0]).unwrap();
    assert_eq!(testing::track_log(&track), vec!["play"]);
    let mh = MixerHarness::take_source(q.id()).unwrap();
    let out = pull(&mh, 3);
    assert_eq!(out.len(), 3 * 960 * 2);
    assert_eq!(mh.pulls(), 3);
    // Idle for `OPUS_IDLE_MS`: the stream pauses its track again (no RTP while unfed).
    r.step(300);
    assert_eq!(testing::track_log(&track).last(), Some(&"pause"));
    r.conn.close(CloseReason::Requested);
    assert!(matches!(
        q.send(&packets[1]),
        Err(CoreError::StreamClosed(CloseReason::ConnectionClosed))
    ));
}

// ── tracks and the queue ────────────────────────────────────────────────────────────────────────────

/// Drives a parked memory track's songbird input to its end, as the mixer would, then drops it.
fn play_to_end(r: &Rig, track: &TrackRef) -> usize {
    use songbird::input::{Input, LiveInput};
    let Some(Input::Live(LiveInput::Parsed(mut p), _)) = r.conn.take_parked_input(track) else {
        panic!("not a parsed live input");
    };
    // A test thread driving mixer callbacks (`lock_checked` would otherwise treat it as a mixer).
    let prev = sbcore::runtime::role();
    sbcore::runtime::set_role(sbcore::runtime::Role::Harness);
    let mut ticks = 0;
    while let Ok(pkt) = p.format.next_packet() {
        p.decoder.decode(&pkt).unwrap();
        ticks += 1;
        assert!(ticks < 10_000);
    }
    drop(p);
    sbcore::runtime::set_role(prev);
    ticks
}

fn pcm_source(ms: u32) -> SourceSpec {
    let mem = MemPcm::from_bytes(PCM16_48K_STEREO, &sine_48k_stereo(ms, 500.0, 0.3)).unwrap();
    SourceSpec::Pcm(Arc::new(mem))
}

fn ended_events(evs: &[EventData]) -> Vec<(u64, TrackEndReason)> {
    evs.iter()
        .filter_map(|e| match e {
            EventData::TrackEnded { track_id, reason } => Some((*track_id, *reason)),
            _ => None,
        })
        .collect()
}

#[test]
fn memory_tracks_and_the_queue_follow_their_lifecycle() {
    let _serial = serial();
    let r = Rig::new();
    r.connect();
    let events = r.conn.subscribe_events(256, KindMask::ALL).unwrap();

    // play(): started at once (fake control), controllable, finished once its reader hits EOF.
    let t = r.conn.play(&pcm_source(100), PlayOpts::default()).unwrap();
    assert!(t.is_started());
    assert_eq!(r.conn.tracks().playing(), 1);
    t.pause().unwrap();
    t.resume().unwrap();
    t.set_volume(0.5).unwrap();
    assert_eq!(testing::track_log(&t), vec!["pause", "play"]);
    assert_eq!(play_to_end(&r, &t), 5);
    r.step(60);
    assert_eq!(t.flags.ended(), Some(TrackEndReason::Finished));
    assert_eq!(
        ended_events(&drain_events(&events)),
        vec![(t.flags.id, TrackEndReason::Finished)]
    );
    assert_eq!(r.conn.tracks().playing(), 0);

    // The queue hands only its current entry to songbird.
    let entries: Vec<TrackRef> = (0..3)
        .map(|_| {
            r.conn
                .queue()
                .enqueue(pcm_source(100), PlayOpts::default(), r.conn.tracks())
                .unwrap()
        })
        .collect();
    assert!(entries.iter().all(|e| !e.is_started()));
    r.step(20);
    let cur = r.conn.queue().current().unwrap();
    assert!(cur.same(&entries[0]) && cur.is_started());
    assert_eq!(r.conn.queue().len(), 2);
    assert_eq!(r.h.parked(), 1);
    // A pending entry's options apply when it starts.
    r.conn
        .queue()
        .update_pending(&entries[1], sbcore::tracks::PendingChange::Paused(true))
        .unwrap();
    // The current entry finishes (its reader reaches EOF): the queue advances by itself.
    play_to_end(&r, &entries[0]);
    r.step(80);
    assert_eq!(entries[0].flags.ended(), Some(TrackEndReason::Finished));
    let cur = r.conn.queue().current().unwrap();
    assert!(cur.same(&entries[1]));
    assert_eq!(
        testing::track_log(&entries[1]),
        Vec::<&str>::new(),
        "handed over paused"
    );
    // skip() resolves the current entry `stopped` at once and advances.
    r.conn.queue().skip();
    r.step(20);
    assert_eq!(entries[1].flags.ended(), Some(TrackEndReason::Stopped));
    assert!(r.conn.queue().current().unwrap().same(&entries[2]));
    // stop() = clear + skip.
    let pending = r
        .conn
        .queue()
        .enqueue(pcm_source(100), PlayOpts::default(), r.conn.tracks())
        .unwrap();
    r.conn.queue().stop();
    r.step(60);
    assert_eq!(entries[2].flags.ended(), Some(TrackEndReason::Stopped));
    assert_eq!(pending.flags.ended(), Some(TrackEndReason::Stopped));
    assert!(r.conn.queue().is_empty());
    let ended = ended_events(&drain_events(&events));
    for e in [&entries[0], &entries[1], &entries[2], &pending] {
        assert_eq!(
            ended.iter().filter(|(id, _)| *id == e.flags.id).count(),
            1,
            "one TrackEnded per entry"
        );
    }
    r.step(20);
    assert_eq!(
        r.h.parked(),
        0,
        "the inputs of ended tracks were disposed of"
    );
    r.conn.close(CloseReason::Requested);
}

#[test]
fn a_lazy_encoded_queue_entry_decodes_resumes_and_finishes() {
    let _serial = serial();
    assert!(all_released(), "{:?}", live());
    let r = Rig::new();
    r.connect();
    let events = r.conn.subscribe_events(256, KindMask::ALL).unwrap();
    let entry = r
        .conn
        .queue()
        .enqueue(
            SourceSpec::Encoded(EncodedFactory {
                spec: EncodedSpec {
                    max_buffer_bytes: 1 << 20,
                    volume: 1.0,
                },
            }),
            PlayOpts::default(),
            r.conn.tracks(),
        )
        .unwrap();
    assert_eq!(
        ENCODED_WORKERS.load(std::sync::atomic::Ordering::Acquire),
        0,
        "nothing is spawned while pending"
    );
    r.step(20);
    assert!(entry.is_started(), "{:?}", entry.flags.ended());
    assert_eq!(entry.flags.kind, TrackKind::EncodedStream);
    let stream = r.conn.queue_take_stream(&entry).unwrap();
    assert!(r.conn.queue_take_stream(&entry).is_none(), "once");
    assert!(testing::track_log(&entry).is_empty(), "started paused");
    // The current encoded entry counts towards MAX_PLAYING_TRACKS_PER_CONN.
    let mut tracks = Vec::new();
    for _ in 0..31 {
        tracks.push(r.conn.play(&pcm_source(20), PlayOpts::default()).unwrap());
    }
    assert!(matches!(
        r.conn.play(&pcm_source(20), PlayOpts::default()),
        Err(CoreError::Limit(_))
    ));
    for t in &tracks {
        t.stop().unwrap();
    }

    // 200 ms of 44.1 kHz mono WAV: decoded on the decode thread, resampled to 48 kHz stereo; the
    // first decoded frame resumes the paused track.
    let pcm = sine_mono(44_100, 200, 1_000.0, 0.5);
    stream.write(&wav(44_100, 1, &pcm)).unwrap();
    stream.finish();
    assert!(wait_real(Duration::from_secs(5), || testing::track_log(
        &entry
    )
    .contains(&"play")));
    assert!(
        wait_real(Duration::from_secs(5), || stream.is_closed()),
        "fully decoded"
    );
    let mh = MixerHarness::take_source(stream.id()).unwrap();
    assert_eq!(mh.channels(), 2);
    let out = pull(&mh, 9);
    let level = db(rms(&out[960 * 2..8 * 960 * 2]));
    let want = db(0.5 / 2f64.sqrt());
    assert!((level - want).abs() < 0.5, "{level} vs {want}");
    let _ = pull(&mh, 3);
    assert!(mh.ended(), "EOF once the decoded audio played out");
    r.step(80);
    assert_eq!(entry.flags.ended(), Some(TrackEndReason::Finished));
    assert!(
        ended_events(&drain_events(&events)).contains(&(entry.flags.id, TrackEndReason::Finished))
    );
    assert!(r.conn.queue().current().is_none());
    r.conn.close(CloseReason::Requested);
    drop((events, entry, stream, mh, tracks, r));
    assert!(all_released(), "{:?}", live());
}

#[test]
fn a_paused_encoded_entry_holds_its_audio_until_resumed() {
    let _serial = serial();
    let r = Rig::new();
    r.connect();
    let entry = r
        .conn
        .queue()
        .enqueue(
            SourceSpec::Encoded(EncodedFactory {
                spec: EncodedSpec {
                    max_buffer_bytes: 1 << 20,
                    volume: 1.0,
                },
            }),
            PlayOpts {
                paused: true,
                ..PlayOpts::default()
            },
            r.conn.tracks(),
        )
        .unwrap();
    r.step(20);
    let stream = r.conn.queue_take_stream(&entry).unwrap();
    stream
        .write(&wav(48_000, 2, &sine_48k_stereo(100, 500.0, 0.5)))
        .unwrap();
    stream.finish();
    assert!(wait_real(Duration::from_secs(5), || stream.is_closed()));
    r.step(40);
    assert!(
        !testing::track_log(&entry).contains(&"play"),
        "a paused entry is not resumed by decoded audio"
    );
    // TrackHandle.resume() goes through the stream's own pause state.
    entry.resume().unwrap();
    assert_eq!(testing::track_log(&entry), vec!["play"]);
    r.conn.close(CloseReason::Requested);
}

// ── limits ──────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn live_sources_and_playing_tracks_stay_within_the_preallocated_tracks() {
    let _serial = serial();
    let r = Rig::new();
    let mut live_sources = Vec::new();
    for i in 0..16 {
        if i % 2 == 0 {
            live_sources.push(r.conn.open_playback(playback_spec(PCM16_24K)).unwrap().1);
        } else {
            live_sources.push(
                r.conn
                    .open_opus_stream(OpusStreamSpec {
                        max_packets: 10,
                        volume: 1.0,
                    })
                    .unwrap()
                    .1,
            );
        }
    }
    assert!(matches!(
        r.conn.open_playback(playback_spec(PCM16_24K)),
        Err(CoreError::Limit(_))
    ));
    // A lazy encoded queue entry is not an `open_*` source: it takes a playing-track slot.
    let entry = r
        .conn
        .queue()
        .enqueue(
            SourceSpec::Encoded(EncodedFactory {
                spec: EncodedSpec {
                    max_buffer_bytes: 1 << 16,
                    volume: 1.0,
                },
            }),
            PlayOpts::default(),
            r.conn.tracks(),
        )
        .unwrap();
    r.step(20);
    assert!(entry.is_started());
    let mut tracks = Vec::new();
    while let Ok(t) = r.conn.play(&pcm_source(20), PlayOpts::default()) {
        tracks.push(t);
        assert!(tracks.len() < 100);
    }
    assert_eq!(tracks.len(), 31);
    assert_eq!(16 + 1 + tracks.len(), 48, "songbird's preallocated tracks");
    r.conn.close(CloseReason::Requested);
}

// ── teardown ────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn close_during_activity_releases_everything() {
    let _serial = serial();
    assert!(all_released(), "{:?}", live());
    {
        let r = Rig::new();
        r.connect();
        let events = r.conn.subscribe_events(1_024, KindMask::ALL).unwrap();
        let sub = r
            .conn
            .subscribe(sub_spec(RxSource::User(USER_A), PCM16_24K, 20))
            .unwrap();
        let mixed = r
            .conn
            .subscribe(sub_spec(
                RxSource::Mixed(MixFilter::everyone()),
                MULAW_8K,
                20,
            ))
            .unwrap();
        let router = r
            .conn
            .subscribe_router(RouterSpec {
                format: PCM16_16K,
                chunk_ms: 20,
                max_queue_ms: 1_000,
                overflow: Overflow::DropOldest,
                include: None,
                ignore_bots: false,
                close_on_leave: false,
            })
            .unwrap();
        let (pb, pb_track) = r
            .conn
            .open_playback(playback_spec(PCM16_48K_STEREO))
            .unwrap();
        let mh = MixerHarness::take(&pb).unwrap();
        let (enc, enc_track) = r
            .conn
            .open_encoded(EncodedSpec {
                max_buffer_bytes: 1 << 20,
                volume: 1.0,
            })
            .unwrap();
        let (opus, opus_track) = r
            .conn
            .open_opus_stream(OpusStreamSpec {
                max_packets: 50,
                volume: 1.0,
            })
            .unwrap();
        let memory = r
            .conn
            .play(&pcm_source(5_000), PlayOpts::default())
            .unwrap();
        let current = r
            .conn
            .queue()
            .enqueue(
                SourceSpec::Encoded(EncodedFactory {
                    spec: EncodedSpec {
                        max_buffer_bytes: 1 << 20,
                        volume: 1.0,
                    },
                }),
                PlayOpts::default(),
                r.conn.tracks(),
            )
            .unwrap();
        let pending = r
            .conn
            .queue()
            .enqueue(pcm_source(100), PlayOpts::default(), r.conn.tracks())
            .unwrap();
        inject_map(&r.conn, SSRC_A, USER_A).unwrap();
        // Activity on every path.
        let packets = encode_opus(&sine_48k_stereo(200, 440.0, 0.5)).unwrap();
        let wav_bytes = wav(48_000, 2, &sine_48k_stereo(500, 300.0, 0.5));
        enc.write(&wav_bytes[..wav_bytes.len() / 2]).unwrap();
        for (i, p) in packets.iter().enumerate() {
            inject_tick(&r.conn, vec![(SSRC_A, Some(p.clone()))], vec![]).unwrap();
            pcm_write(&pb, &sine_48k_stereo(20, 1_000.0, 0.3));
            opus.send(p).unwrap();
            if i % 2 == 0 {
                let _ = pull(&mh, 1);
            }
            r.step(20);
        }
        let routed = match router.queue.try_pop_or_arm() {
            Pop::Item(s) => s,
            _ => panic!("no routed stream"),
        };
        assert!(current.is_started());
        assert!(r.conn.queue_take_stream(&current).is_some());
        assert!(!drain_sub(&sub.queue).is_empty());

        r.conn.close(CloseReason::Requested);

        // Everything observable is closed synchronously (T5–T9).
        for q in [&sub.queue, &mixed.queue, &routed.queue] {
            assert_eq!(q.close_reason(), Some(CloseReason::ConnectionClosed));
        }
        assert!(matches!(router.queue.try_pop_or_arm(), Pop::Closed(_)));
        let evs = drain_events(&events);
        assert!(matches!(
            evs.last(),
            Some(EventData::Disconnected {
                reason: "requested",
                will_retry: false,
                ..
            })
        ));
        let closed = |r: Result<(), CoreError>| {
            matches!(
                r,
                Err(CoreError::StreamClosed(CloseReason::ConnectionClosed))
            )
        };
        assert!(closed(pb.write(&[0; 4], WriteMeta::default()).map(|_| ())));
        assert!(closed(enc.write(&[0; 4])));
        assert!(closed(opus.send(&packets[0])));
        for t in [
            &pb_track,
            &enc_track,
            &opus_track,
            &memory,
            &current,
            &pending,
        ] {
            assert_eq!(
                t.flags.ended(),
                Some(TrackEndReason::Disconnected),
                "{:?}",
                t.flags.kind
            );
        }
        assert!(r.conn.queue().is_empty() && r.conn.queue().current().is_none());
        assert_eq!(r.h.parked(), 0, "every parked input was disposed of");
        assert!(
            r.conn
                .subscribe(sub_spec(RxSource::User(1), PCM16_24K, 20))
                .is_err()
        );
        assert_eq!(r.stat("panics"), 0);
        r.step(20); // the worker has stopped; stepping is harmless
        drop((sub, mixed, router, routed, events, mh));
        drop((
            pb, enc, opus, pb_track, enc_track, opus_track, memory, current, pending,
        ));
    }
    // With the harness and the connection gone, every live object is released (decode threads
    // exit asynchronously once their ring is closed).
    assert!(all_released(), "{:?}", live());
}

#[test]
fn closing_while_writers_and_the_mixer_are_busy_never_hangs() {
    let _serial = serial();
    assert!(all_released(), "{:?}", live());
    for _ in 0..5 {
        let r = Rig::new();
        r.connect();
        let (pb, _t) = r.conn.open_playback(playback_spec(PCM16_24K)).unwrap();
        let mh = MixerHarness::take(&pb).unwrap();
        let sub = r
            .conn
            .subscribe(sub_spec(
                RxSource::Mixed(MixFilter::everyone()),
                PCM16_24K,
                20,
            ))
            .unwrap();
        let writer = {
            let pb = pb.clone();
            std::thread::spawn(move || {
                let chunk = sine_mono(24_000, 20, 440.0, 0.2);
                for _ in 0..2_000 {
                    match pb.write(&chunk, WriteMeta::default()) {
                        Ok(_) | Err(CoreError::PlaybackFull { .. }) => {}
                        Err(CoreError::StreamClosed(_)) => return,
                        Err(e) => panic!("{e:?}"),
                    }
                }
            })
        };
        let mixer = {
            let mh = mh.clone();
            std::thread::spawn(move || {
                for _ in 0..200 {
                    let mut out = Vec::new();
                    if mh.pull(1, &mut out).is_err() || mh.ended() {
                        return;
                    }
                }
            })
        };
        for _ in 0..10 {
            inject_tick(&r.conn, vec![], vec![]).unwrap();
            r.step(20);
        }
        r.conn.close(CloseReason::Requested);
        writer.join().unwrap();
        mixer.join().unwrap();
        assert!(matches!(sub.queue.try_pop(), Pop::Item(_) | Pop::Closed(_)));
        assert!(pb.is_closed());
        drop((sub, pb, mh, r));
    }
    assert!(all_released(), "{:?}", live());
}

// The µ-law stream above relies on 8 kHz mono G.711 framing (160 bytes per 20 ms).
#[test]
fn mulaw_8k_is_160_bytes_per_20_ms() {
    let f = Format::new(8_000, 1, Encoding::MuLaw).unwrap();
    assert_eq!(f, MULAW_8K);
    assert_eq!(f.bytes_for_ms(20).unwrap(), 160);
}
