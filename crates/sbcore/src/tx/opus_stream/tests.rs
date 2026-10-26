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
