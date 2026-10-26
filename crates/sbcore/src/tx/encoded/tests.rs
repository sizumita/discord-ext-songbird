//! Encoded-stream tests (§13.1 `tx::encoded`). `ENCODED_WORKERS` is process-wide, so these tests
//! run one at a time.

use std::time::{Duration, Instant};

use songbird::input::Parsed;

use super::*;
use crate::runtime::{Role, set_role};
use crate::tracks::flags::TrackFlags;
use crate::tx::decoder::tests::{guard, parsed};
use crate::tx::memory::tests::wav;
use crate::tx::queue::tests::{Rec, test_hub};

static SERIAL: Mutex<()> = Mutex::new(());

struct ERig {
    shared: Arc<EncodedShared>,
    p: Option<Parsed>,
    log: Rec,
    flags: Arc<TrackFlags>,
    _hub: Arc<crate::wake::WakeHub>,
    _bell: std::os::unix::net::UnixStream,
}

fn erig(max_buffer_bytes: usize) -> Result<ERig, CoreError> {
    set_role(Role::Harness);
    let (hub, bell) = test_hub();
    let (done, flags) = guard();
    let (shared, input) = spawn_encoded(
        EncodedSpec {
            max_buffer_bytes,
            volume: 1.0,
        },
        hub.alloc()?,
        done,
    )?;
    let log = Rec::default();
    shared.attach_track(Box::new(log.clone()));
    Ok(ERig {
        shared,
        p: Some(parsed(input)),
        log,
        flags,
        _hub: hub,
        _bell: bell,
    })
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(10), "timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

impl ERig {
    /// Drives the mixer side until EOF (or `max` ticks); returns (ticks, decode errors).
    fn play_out(&mut self, max: usize) -> (usize, usize) {
        let p = self.p.as_mut().unwrap();
        let mut ticks = 0;
        let mut errors = 0;
        while ticks < max {
            let Ok(pkt) = p.format.next_packet() else {
                break;
            };
            if p.decoder.decode(&pkt).is_err() {
                errors += 1;
                break;
            }
            ticks += 1;
        }
        (ticks, errors)
    }
}

#[test]
fn wav_in_random_pieces_decodes_and_resumes_only_on_the_first_frame() {
    let _s = SERIAL.lock();
    let mut r = erig(64 << 10).unwrap();
    // 0.5 s of 44.1 kHz stereo.
    let samples: Vec<i16> = (0..44_100)
        .map(|i| ((i as f64 * 0.05).sin() * 12_000.0) as i16)
        .collect();
    let bytes = wav(44_100, 2, &samples);
    std::thread::sleep(Duration::from_millis(20));
    assert!(
        r.log.calls().is_empty(),
        "paused until the first decoded frame"
    );
    let mut pos = 0;
    let mut k = 1usize;
    while pos < bytes.len() {
        let n = ((k * 7_919) % 5_000 + 1).min(bytes.len() - pos);
        match r.shared.write(&bytes[pos..pos + n]) {
            Ok(()) => {
                pos += n;
                k += 1;
            }
            Err(CoreError::PlaybackFull { .. }) => std::thread::sleep(Duration::from_millis(1)),
            Err(e) => panic!("{e:?}"),
        }
    }
    r.shared.finish();
    assert!(matches!(
        r.shared.write(&[0]),
        Err(CoreError::StreamClosed(_))
    ));
    // The decode thread drains the ring and closes the internal queue (drain).
    wait_until("decoded", || r.shared.pcm().is_closed());
    assert_eq!(r.log.calls(), vec!["play"]);
    let queued = r.shared.pcm().queued_frames();
    assert!((23_900..=24_100).contains(&queued), "{queued}");
    let (ticks, errors) = r.play_out(1_000);
    assert_eq!(errors, 0);
    assert_eq!(ticks, 25);
    assert_eq!(r.shared.pcm().stats().played_ms, 500);
    wait_until("worker slot released", || Arc::strong_count(&r.shared) == 1);
}

#[test]
fn decode_thread_exits_on_close_mid_stream() {
    let _s = SERIAL.lock();
    let r = erig(64 << 10).unwrap();
    let samples = vec![1_000i16; 48_000];
    let bytes = wav(48_000, 1, &samples);
    r.shared.write(&bytes[..10_000]).unwrap();
    wait_until("some audio decoded", || r.shared.pcm().queued_frames() > 0);
    LiveSource::close(&*r.shared, CloseMode::Abort);
    assert!(r.shared.is_closed());
    wait_until("thread exited", || Arc::strong_count(&r.shared) == 1);
    assert!(matches!(
        r.shared.write(&bytes[..10]),
        Err(CoreError::StreamClosed(CloseReason::Closed))
    ));
    assert!(r.log.calls().contains(&"stop"));
}

#[test]
fn the_17th_encoded_stream_is_refused() {
    let _s = SERIAL.lock();
    wait_until("no workers", || {
        ENCODED_WORKERS.load(Ordering::Acquire) == 0
    });
    let rigs: Vec<ERig> = (0..MAX_ENCODED_WORKERS)
        .map(|_| erig(64 << 10).unwrap())
        .collect();
    assert!(matches!(erig(64 << 10), Err(CoreError::Limit(_))));
    for r in &rigs {
        r.shared.conn_closed();
    }
    wait_until("all workers exited", || {
        ENCODED_WORKERS.load(Ordering::Acquire) == 0
    });
    assert!(matches!(
        rigs[0].shared.write(&[0]),
        Err(CoreError::StreamClosed(CloseReason::ConnectionClosed))
    ));
    drop(rigs);
    let r = erig(64 << 10).unwrap();
    LiveSource::close(&*r.shared, CloseMode::Abort);
}

#[test]
fn garbage_fails_with_a_decode_error() {
    let _s = SERIAL.lock();
    let mut r = erig(64 << 10).unwrap();
    r.shared.write(&[0x42; 30_000]).unwrap();
    r.shared.finish();
    wait_until("failed", || r.shared.pcm().failed());
    assert_ne!(r.flags.hints() & crate::tracks::flags::DECODE_ERROR, 0);
    // The paused track is resumed so the decoder can report the error to songbird.
    assert_eq!(r.log.calls(), vec!["play"]);
    let (_, errors) = r.play_out(10);
    assert_eq!(errors, 1);
}

#[test]
fn ring_limits_and_space_wakeups() {
    let _s = SERIAL.lock();
    assert!(matches!(erig(1_000), Err(CoreError::Limit(_))));
    let r = erig(64 << 10).unwrap();
    assert!(matches!(
        r.shared.write(&vec![0u8; (64 << 10) + 1]),
        Err(CoreError::Limit(_))
    ));
    // The decode thread is blocked in the probe (not enough bytes for a format): fill the ring.
    r.shared.write(&[0u8; 10]).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let _ = r.shared.buffered_bytes();
    LiveSource::close(&*r.shared, CloseMode::Abort);
    assert!(r.shared.arm_space(10));
    let p = r.p;
    drop(p);
    assert_eq!(
        LiveSource::housekeep(&*r.shared, Instant::now(), &EventHub::default()),
        HouseStatus::Finished
    );
}
