//! `OpusQueue` / `OpusStreamReader` tests (§13.1 `tx::opus_stream`).

use std::time::Duration;

use super::*;
use crate::runtime::{Role, set_role};
use crate::tx::decoder::tests::{guard, parsed};
use crate::tx::queue::tests::{Rec, test_hub};

/// `n` real Opus packets of `frame` samples per channel (48 kHz stereo).
pub(crate) fn opus_packets(n: usize, frame: usize) -> Vec<Vec<u8>> {
    let mut enc =
        opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
    (0..n)
        .map(|i| {
            let pcm: Vec<f32> = (0..frame * 2)
                .map(|k| (((k + i * frame * 2) as f32) * 0.03).sin() * 0.3)
                .collect();
            let mut out = vec![0u8; 1_500];
            let len = enc.encode_float(&pcm, &mut out).unwrap();
            out.truncate(len);
            out
        })
        .collect()
}

struct ORig {
    q: Arc<OpusQueue>,
    p: Parsed,
    log: Rec,
    hub: Arc<crate::wake::WakeHub>,
    muted: Arc<AtomicBool>,
    _bell: std::os::unix::net::UnixStream,
}

fn orig(max_packets: usize) -> ORig {
    set_role(Role::Harness);
    let (hub, bell) = test_hub();
    let muted = Arc::new(AtomicBool::new(false));
    let q = OpusQueue::new(
        &OpusStreamSpec {
            max_packets,
            volume: 1.0,
        },
        hub.alloc().unwrap(),
        muted.clone(),
    )
    .unwrap();
    let log = Rec::default();
    q.attach_track(Box::new(log.clone()));
    let (done, _) = guard();
    let p = parsed(opus_stream_input(q.clone(), done).unwrap());
    ORig {
        q,
        p,
        log,
        hub,
        muted,
        _bell: bell,
    }
}

impl ORig {
    fn next(&mut self) -> Option<Box<[u8]>> {
        let pkt = self.p.format.next_packet().ok()?;
        assert_eq!(pkt.dur, 960);
        Some(pkt.buf().into())
    }

    fn ready(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.hub.take_ready(&mut v);
        v
    }
}

#[test]
fn packet_validation() {
    assert!(validate_opus_packet(&[]).is_err());
    assert!(validate_opus_packet(&[0xF8; 1_276]).is_err());
    assert!(validate_opus_packet(&SILENT_FRAME).is_ok());
    // TOC and frame count say "one 20 ms frame" (`get_nb_samples` == 960), but the code-3 padding
    // length runs past the end: libopus cannot decode it, and one such packet would end the whole
    // track the first time it is mixed.
    let malformed = [0xFB, 0x41, 0xFF];
    assert_eq!(
        opus::packet::get_nb_samples(&malformed, 48_000).ok(),
        Some(960)
    );
    assert!(matches!(
        validate_opus_packet(&malformed),
        Err(CoreError::InvalidAudio(_))
    ));
    // Code 1 (two equal 10 ms frames, 960 samples) with an odd payload: not two equal frames.
    let odd = [0xF1, 0x00, 0x01, 0x02];
    assert_eq!(opus::packet::get_nb_samples(&odd, 48_000).ok(), Some(960));
    assert!(matches!(
        validate_opus_packet(&odd),
        Err(CoreError::InvalidAudio(_))
    ));
    let mut dec = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
    let mut out = vec![0f32; 960 * 2];
    assert!(dec.decode_float(&malformed, &mut out, false).is_err());
    let p20 = opus_packets(1, 960);
    assert!(validate_opus_packet(&p20[0]).is_ok());
    let p10 = opus_packets(1, 480);
    assert!(matches!(
        validate_opus_packet(&p10[0]),
        Err(CoreError::InvalidAudio(_))
    ));
    let (hub, _bell) = test_hub();
    let muted = Arc::new(AtomicBool::new(false));
    for bad in [0usize, 3_001] {
        assert!(matches!(
            OpusQueue::new(
                &OpusStreamSpec {
                    max_packets: bad,
                    volume: 1.0
                },
                hub.alloc().unwrap(),
                muted.clone()
            ),
            Err(CoreError::Limit(_))
        ));
    }
}

#[test]
fn packets_pass_through_and_underrun_sends_silent_frames_without_allocating() {
    let mut r = orig(4);
    assert!(r.log.calls().is_empty(), "starts paused");
    let pk = opus_packets(5, 960);
    for p in &pk[..4] {
        r.q.send(p).unwrap();
    }
    assert_eq!(r.log.calls(), vec!["play"]);
    assert!(matches!(
        r.q.send(&pk[4]),
        Err(CoreError::PlaybackFull { .. })
    ));
    assert!(matches!(
        r.q.send(&[1, 2, 3, 4]),
        Err(CoreError::InvalidAudio(_)) | Err(CoreError::PlaybackFull { .. })
    ));
    assert_eq!(r.q.buffered_ms(), 80);
    assert!(!r.q.arm_space());
    assert_eq!(&*r.next().unwrap(), &pk[0][..]);
    assert_eq!(r.q.housekeep_at(Instant::now()), HouseStatus::Live);
    assert_eq!(r.ready(), vec![r.q.wake_id().0]);
    for p in &pk[1..4] {
        assert_eq!(&*r.next().unwrap(), &p[..]);
    }
    // Underrun: SILENT_FRAME from the reserve, no allocation.
    let mut silent = Vec::with_capacity(10);
    let g = crate::alloc_count::CountGuard::new();
    let before = crate::alloc_count::count();
    for _ in 0..10 {
        let pkt = r.p.format.next_packet().unwrap();
        silent.push(pkt);
    }
    let after = crate::alloc_count::count();
    drop(g);
    assert_eq!(after, before);
    assert!(silent.iter().all(|p| p.buf() == SILENT_FRAME));
    // The decoder accepts what the reader produces.
    assert!(r.p.decoder.decode(&silent[0]).is_ok());
    // The housekeeper refills the reserve.
    r.q.housekeep_at(Instant::now());
    assert_eq!(r.q.inner.lock().silent_reserve.len(), OPUS_SILENT_RESERVE);
}

#[test]
fn contention_and_self_mute_send_silence_and_keep_packets() {
    let mut r = orig(10);
    let pk = opus_packets(2, 960);
    r.q.send(&pk[0]).unwrap();
    // Warm the reader's stash.
    r.muted.store(true, Ordering::Release);
    assert_eq!(&*r.next().unwrap(), &SILENT_FRAME[..]);
    assert_eq!(r.q.buffered_ms(), 20);
    r.muted.store(false, Ordering::Release);
    {
        let _held = r.q.inner.lock();
        let g = crate::alloc_count::CountGuard::new();
        let before = crate::alloc_count::count();
        let pkt = r.p.format.next_packet().unwrap();
        assert_eq!(
            crate::alloc_count::count(),
            before,
            "contention uses the stash"
        );
        drop(g);
        assert_eq!(pkt.buf(), SILENT_FRAME);
    }
    assert_eq!(r.q.contended.load(Ordering::Acquire), 1);
    assert_eq!(&*r.next().unwrap(), &pk[0][..]);
}

#[test]
fn idle_pauses_and_send_resumes() {
    let mut r = orig(10);
    let t0 = Instant::now();
    let pk = opus_packets(1, 960);
    r.q.send(&pk[0]).unwrap();
    r.q.housekeep_at(t0);
    r.next().unwrap();
    r.q.housekeep_at(t0 + Duration::from_millis(20));
    r.next().unwrap(); // silent
    r.q.housekeep_at(t0 + Duration::from_millis(200));
    assert_eq!(r.log.calls(), vec!["play"]);
    r.q.housekeep_at(t0 + Duration::from_millis(300));
    assert_eq!(r.log.calls(), vec!["play", "pause"]);
    r.q.send(&pk[0]).unwrap();
    assert_eq!(r.log.calls(), vec!["play", "pause", "play"]);
}

#[test]
fn clear_and_close() {
    let mut r = orig(10);
    let pk = opus_packets(3, 960);
    for p in &pk {
        r.q.send(p).unwrap();
    }
    assert_eq!(r.q.clear(), 60);
    assert_eq!(r.q.buffered_ms(), 0);
    r.q.send(&pk[0]).unwrap();
    LiveSource::close(&*r.q, CloseMode::Drain);
    assert!(matches!(
        r.q.send(&pk[1]),
        Err(CoreError::StreamClosed(CloseReason::Closed))
    ));
    assert!(r.next().is_some());
    assert!(r.next().is_none(), "EOF after drain");
    let mut r = orig(10);
    r.q.send(&pk[0]).unwrap();
    LiveSource::close(&*r.q, CloseMode::Abort);
    assert!(r.next().is_none());
    assert_eq!(r.log.calls(), vec!["play", "stop"]);
    let r2 = orig(10);
    r2.q.conn_closed();
    assert!(matches!(
        r2.q.send(&pk[0]),
        Err(CoreError::StreamClosed(CloseReason::ConnectionClosed))
    ));
    // Reader drop → Finished.
    let ORig { q, p, .. } = r;
    drop(p);
    assert!(q.reader_gone());
    assert_eq!(q.housekeep_at(Instant::now()), HouseStatus::Finished);
}

#[test]
fn a_close_before_attach_is_applied_when_the_track_attaches() {
    set_role(Role::Harness);
    let (hub, _bell) = test_hub();
    let spec = OpusStreamSpec {
        max_packets: 10,
        volume: 1.0,
    };
    let muted = Arc::new(AtomicBool::new(false));
    // Never sent (paused), drain-closed before the attach: nothing to drain, so it is resumed and
    // ends at the next pull (EOF → `finished`, not `stopped`).
    let q = OpusQueue::new(&spec, hub.alloc().unwrap(), muted.clone()).unwrap();
    LiveSource::close(&*q, CloseMode::Drain);
    let log = Rec::default();
    q.attach_track(Box::new(log.clone()));
    assert_eq!(log.calls(), vec!["play"]);
    assert!(q.should_end());
    // Closed by T5 before the attach.
    let q = OpusQueue::new(&spec, hub.alloc().unwrap(), muted.clone()).unwrap();
    q.conn_closed();
    let log = Rec::default();
    q.attach_track(Box::new(log.clone()));
    assert_eq!(log.calls(), vec!["stop"]);
    // Sent, then drain-closed before the attach: it plays out, no stop.
    let q = OpusQueue::new(&spec, hub.alloc().unwrap(), muted).unwrap();
    q.send(&opus_packets(1, 960)[0]).unwrap();
    LiveSource::close(&*q, CloseMode::Drain);
    let log = Rec::default();
    q.attach_track(Box::new(log.clone()));
    assert_eq!(log.calls(), vec!["play"]);
}

#[test]
fn abandon_aborts_an_open_stream_but_lets_a_drain_finish() {
    let mut r = orig(10);
    let pk = opus_packets(2, 960);
    r.q.send(&pk[0]).unwrap();
    r.q.abandon();
    assert!(r.next().is_none());
    assert_eq!(r.log.calls(), vec!["play", "stop"]);
    let mut r = orig(10);
    r.q.send(&pk[0]).unwrap();
    r.q.send(&pk[1]).unwrap();
    LiveSource::close(&*r.q, CloseMode::Drain);
    r.q.abandon();
    assert_eq!(r.log.calls(), vec!["play"], "the drain continues");
    assert!(r.next().is_some());
    assert!(r.next().is_some());
    assert!(r.next().is_none());
}

/// `TrackHandle.pause()` is the stream's own user pause: the next `send` (which resumes an
/// idle-paused stream) and the idle pause do not undo it, and only `resume()` plays again.
#[test]
fn a_user_pause_survives_sends_and_the_idle_pause() {
    let mut r = orig(10);
    let t0 = Instant::now();
    let pk = opus_packets(3, 960);
    // Paused before the first packet: the send that would resume the stream keeps it paused.
    assert!(LiveSource::set_user_paused(&*r.q, true));
    r.q.send(&pk[0]).unwrap();
    assert!(r.log.calls().is_empty(), "still paused");
    // Resumed: plays.
    assert!(LiveSource::set_user_paused(&*r.q, false));
    assert_eq!(r.log.calls(), vec!["play"]);
    assert_eq!(&*r.next().unwrap(), &pk[0][..]);
    // Paused while empty, then idle for longer than OPUS_IDLE_MS: a later send keeps it paused.
    assert!(LiveSource::set_user_paused(&*r.q, true));
    r.q.housekeep_at(t0);
    r.q.housekeep_at(t0 + Duration::from_millis(400));
    r.q.send(&pk[1]).unwrap();
    assert_eq!(r.log.calls(), vec!["play", "pause"]);
    assert!(LiveSource::set_user_paused(&*r.q, false));
    assert_eq!(r.log.calls(), vec!["play", "pause", "play"]);
}

/// A drain-close of a stream that went idle ends it naturally (EOF → `finished`); one a user
/// pause holds with packets left can never drain and is stopped (`aclose(drain=True)` would
/// otherwise wait forever).
#[test]
fn drain_close_resumes_an_idle_stream_and_stops_a_user_paused_one() {
    let mut r = orig(10);
    let t0 = Instant::now();
    let pk = opus_packets(2, 960);
    r.q.send(&pk[0]).unwrap();
    r.q.housekeep_at(t0);
    r.next().unwrap();
    r.q.housekeep_at(t0 + Duration::from_millis(20));
    r.q.housekeep_at(t0 + Duration::from_millis(400));
    assert_eq!(r.log.calls(), vec!["play", "pause"], "idle-paused");
    LiveSource::close(&*r.q, CloseMode::Drain);
    assert_eq!(r.log.calls(), vec!["play", "pause", "play"]);
    assert!(r.next().is_none(), "EOF at the next pull");
    // User-paused with packets left.
    let r = orig(10);
    r.q.send(&pk[0]).unwrap();
    assert!(LiveSource::set_user_paused(&*r.q, true));
    LiveSource::close(&*r.q, CloseMode::Drain);
    assert_eq!(r.log.calls(), vec!["play", "pause", "stop"]);
    // User-paused and empty: nothing would be discarded, so it ends naturally.
    let mut r = orig(10);
    assert!(LiveSource::set_user_paused(&*r.q, true));
    LiveSource::close(&*r.q, CloseMode::Drain);
    assert_eq!(r.log.calls(), vec!["play"]);
    assert!(r.next().is_none());
}

/// The allocation-free framing check agrees with libopus `opus_packet_parse` on every 20 ms
/// packet: real encoder output, and random bytes behind every TOC code (VBR/CBR, padding).
#[test]
fn framing_check_matches_libopus_parse() {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut compared = 0;
    let mut rejected = 0;
    for p in opus_packets(20, 960) {
        assert!(opus_framing_ok(&p));
        assert!(validate_opus_packet(&p).is_ok());
    }
    // 20 ms TOCs: CELT FB 20 ms (31) code 0..3, and 10 ms / 2.5 ms configs with code 1..3.
    let tocs = [
        0xF8u8, 0xF9, 0xFA, 0xFB, 0xF1, 0xF2, 0xF3, 0xE3, 0x0B, 0x4B, 0x7A,
    ];
    for _ in 0..200_000 {
        let r = next();
        let len = 1
            + (r % 64) as usize
            + if r & (1 << 40) != 0 {
                (r >> 48) as usize % 1_200
            } else {
                0
            };
        let mut p: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        p[0] = tocs[(r >> 8) as usize % tocs.len()];
        if p.len() > 1 && p[0] & 3 == 3 {
            // A frame count that makes 20 ms for this config, with random VBR/padding bits.
            let spf = opus::packet::get_samples_per_frame(&p, 48_000).unwrap();
            p[1] = (p[1] & 0xC0) | (960 / spf) as u8;
        }
        if opus::packet::get_nb_samples(&p, 48_000).ok() != Some(960) {
            continue;
        }
        compared += 1;
        let ours = opus_framing_ok(&p);
        let libopus = opus::packet::parse(&p).is_ok();
        assert_eq!(ours, libopus, "{p:02x?}");
        rejected += usize::from(!ours);
    }
    assert!(
        compared > 50_000 && rejected > 1_000,
        "{compared} {rejected}"
    );
}
