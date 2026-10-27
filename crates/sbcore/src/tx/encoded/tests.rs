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
fn a_stream_closed_before_its_first_byte_is_never_probed() {
    let _s = SERIAL.lock();
    for mode in [CloseMode::Abort, CloseMode::Drain] {
        let r = erig(64 << 10).unwrap();
        std::thread::sleep(Duration::from_millis(20)); // the decode thread waits for input
        if mode == CloseMode::Drain {
            // An empty finished stream still probes (and fails): it is not a close.
            LiveSource::close(&*r.shared, mode);
            wait_until("thread exited", || Arc::strong_count(&r.shared) == 1);
            assert!(PROBED.lock().contains(&r.shared.id));
            assert!(r.flags.hints() & DECODE_ERROR != 0);
            // A real decode error: symphonia's report is kept.
            assert!(MUTED_AT_EXIT.lock().contains(&(r.shared.id, false)));
            continue;
        }
        LiveSource::close(&*r.shared, mode);
        wait_until("thread exited", || Arc::strong_count(&r.shared) == 1);
        assert!(!PROBED.lock().contains(&r.shared.id));
        assert_eq!(r.flags.hints() & DECODE_ERROR, 0);
    }
    // Once a byte arrived, the probe runs.
    let r = erig(64 << 10).unwrap();
    let bytes = wav(48_000, 1, &[0i16; 4_800]);
    r.shared.write(&bytes).unwrap();
    wait_until("probed", || PROBED.lock().contains(&r.shared.id));
    LiveSource::close(&*r.shared, CloseMode::Abort);
    wait_until("thread exited", || Arc::strong_count(&r.shared) == 1);
}

#[test]
fn a_close_between_the_wait_and_the_probe_mutes_symphonias_eof_report() {
    let _s = SERIAL.lock();
    let r = erig(64 << 10).unwrap();
    CLOSE_BEFORE_PROBE.lock().push(r.shared.id);
    let bytes = wav(48_000, 1, &[0i16; 4_800]);
    r.shared.write(&bytes).unwrap();
    wait_until("thread exited", || Arc::strong_count(&r.shared) == 1);
    assert!(PROBED.lock().contains(&r.shared.id));
    // The probe read EOF at 0 bytes because of the close: a normal close, not a decode error, and
    // symphonia's "probe reach EOF at 0 bytes." ERROR record was dropped on that thread.
    assert_eq!(r.flags.hints() & DECODE_ERROR, 0);
    assert!(MUTED_AT_EXIT.lock().contains(&(r.shared.id, true)));
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
    // The binding checks a buffer against this before copying it (§7.3).
    assert_eq!(r.shared.max_write_bytes(), 64 << 10);
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

#[test]
fn tiny_decoded_pieces_wait_for_segment_space_instead_of_stopping() {
    // A codec with tiny packets fills the internal queue's segment bound (48 000 / 32 + 64 = 1 564
    // segments) long before its frame cap: the pusher must wait for the mixer, not give up.
    let _s = SERIAL.lock();
    let mut r = erig(64 << 10).unwrap();
    let mut p = r.p.take().unwrap();
    let done = AtomicBool::new(false);
    let pushed = std::thread::scope(|sc| {
        let h = sc.spawn(|| {
            set_role(Role::Decode);
            let pusher = Pusher {
                shared: &r.shared,
                rs: None,
                stereo: Vec::new(),
                out48: Vec::new(),
            };
            let mut ok = true;
            for _ in 0..2_000 {
                if pusher.push48(&[0.1, 0.1]).is_err() {
                    ok = false;
                    break;
                }
            }
            done.store(true, Ordering::Release);
            ok
        });
        let t = Instant::now();
        while !done.load(Ordering::Acquire) {
            assert!(t.elapsed() < Duration::from_secs(10), "pusher stuck");
            std::thread::sleep(Duration::from_millis(5));
            if let Ok(pkt) = p.format.next_packet() {
                let _ = p.decoder.decode(&pkt);
            }
            let _ = LiveSource::housekeep(&*r.shared, Instant::now(), &EventHub::default());
        }
        h.join().unwrap()
    });
    assert!(pushed, "the pusher gave up on a full segment table");
    assert_eq!(r.shared.pcm().stats().written_ms, 2_000 * 1_000 / 48_000);
    LiveSource::close(&*r.shared, CloseMode::Abort);
}

#[test]
fn bad_packets_are_counted_in_a_row() {
    let mut b = BadPackets::default();
    for _ in 0..MAX_BAD_PACKETS_IN_A_ROW {
        assert!(b.bad());
    }
    b.good();
    for _ in 0..MAX_BAD_PACKETS_IN_A_ROW {
        assert!(b.bad(), "a decoded packet resets the run");
    }
    assert!(!b.bad());
}

#[test]
fn connection_close_wakes_writers() {
    let _s = SERIAL.lock();
    let r = erig(64 << 10).unwrap();
    let mut ready = Vec::new();
    r._hub.take_ready(&mut ready);
    r.shared.conn_closed();
    ready.clear();
    r._hub.take_ready(&mut ready);
    assert_eq!(ready, vec![r.shared.wake_id().0]);
    assert!(r.shared.arm_space(10));
    assert!(matches!(
        r.shared.write(&[0]),
        Err(CoreError::StreamClosed(CloseReason::ConnectionClosed))
    ));
}

/// A decode failure while a writer is parked on ring space wakes it at once (to
/// `StreamClosed`), even self-muted, when the decoded lookahead never drains and the track
/// never ends.
#[test]
fn a_decode_failure_wakes_a_writer_parked_on_ring_space() {
    let _s = SERIAL.lock();
    set_role(Role::Harness);
    let (hub, _bell) = test_hub();
    let (done, _flags) = guard();
    let (shared, input) = spawn_encoded_with(
        EncodedSpec {
            max_buffer_bytes: 64 << 10,
            volume: 1.0,
        },
        hub.alloc().unwrap(),
        done,
        Arc::new(AtomicBool::new(true)), // self-muted: the mixer consumes nothing
        Clock::Real,
    )
    .unwrap();
    let _input = input;
    let samples: Vec<i16> = (0..48_000 * 2 * 3)
        .map(|i| ((i as f64 * 0.01).sin() * 8000.0) as i16)
        .collect();
    let bytes = wav(48_000, 2, &samples);
    // Fill the ring until the writer parks: the decode thread blocks on the full (1 s) internal
    // queue, so the ring stops draining.
    let mut pos = 0;
    let t = Instant::now();
    loop {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "the writer never parked"
        );
        let n = 16_384.min(bytes.len() - pos);
        match shared.write(&bytes[pos..pos + n]) {
            Ok(()) => pos += n,
            Err(CoreError::PlaybackFull { .. }) => {
                if !shared.arm_space(n) {
                    std::thread::sleep(Duration::from_millis(50));
                    if !shared.arm_space(n) {
                        break;
                    }
                }
            }
            Err(e) => panic!("{e:?}"),
        }
    }
    let mut ready = Vec::new();
    hub.take_ready(&mut ready);
    ready.clear();
    // `decode_main`'s `Outcome::Failed` branch.
    shared.decode_failed();
    hub.take_ready(&mut ready);
    assert_eq!(
        ready,
        vec![shared.wake_id().0],
        "the parked writer was not woken"
    );
    assert!(shared.arm_space(1));
    assert!(matches!(
        shared.write(&[0]),
        Err(CoreError::StreamClosed(CloseReason::Closed))
    ));
    LiveSource::close(&*shared, CloseMode::Abort);
}

#[test]
fn abandon_after_finish_lets_the_stream_play_out() {
    let _s = SERIAL.lock();
    // Unfinished: dropping the handle aborts.
    let r = erig(64 << 10).unwrap();
    r.shared.abandon();
    assert!(r.shared.is_closed());
    // Finished: the decoded audio still plays out and the track ends on its own.
    let mut r = erig(64 << 10).unwrap();
    let samples = vec![1_000i16; 4_800];
    r.shared.write(&wav(48_000, 1, &samples)).unwrap();
    r.shared.finish();
    r.shared.abandon();
    wait_until("decoded", || r.shared.pcm().is_closed());
    assert!(!r.shared.closed.load(Ordering::Acquire), "not aborted");
    let (ticks, errors) = r.play_out(100);
    assert_eq!((ticks, errors), (5, 0));
    assert_eq!(r.shared.pcm().stats().played_ms, 100);
    assert!(!r.log.calls().contains(&"stop"));
}

/// Ogg's CRC-32 (polynomial 0x04C11DB7, MSB first, no reflection, init 0).
#[cfg(any(feature = "codec-full", feature = "format-ogg"))]
fn ogg_crc(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// One Ogg page holding whole `packets`.
#[cfg(any(feature = "codec-full", feature = "format-ogg"))]
fn ogg_page(serial: u32, seq: u32, flags: u8, granule: u64, packets: &[&[u8]]) -> Vec<u8> {
    let mut lacing = Vec::new();
    for p in packets {
        lacing.extend(std::iter::repeat_n(255u8, p.len() / 255));
        lacing.push((p.len() % 255) as u8);
    }
    let mut page = Vec::new();
    page.extend_from_slice(b"OggS");
    page.push(0);
    page.push(flags);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&seq.to_le_bytes());
    page.extend_from_slice(&[0; 4]);
    page.push(lacing.len() as u8);
    page.extend_from_slice(&lacing);
    for p in packets {
        page.extend_from_slice(p);
    }
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

/// A complete Ogg Opus physical stream (one "song") of `packets` 20 ms stereo packets.
#[cfg(any(feature = "codec-full", feature = "format-ogg"))]
fn ogg_opus(serial: u32, packets: &[Vec<u8>]) -> Vec<u8> {
    const PRE_SKIP: u16 = 312;
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(2);
    head.extend_from_slice(&PRE_SKIP.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&4u32.to_le_bytes());
    tags.extend_from_slice(b"test");
    tags.extend_from_slice(&0u32.to_le_bytes());
    let mut out = ogg_page(serial, 0, 0x02, 0, &[&head]);
    out.extend(ogg_page(serial, 1, 0, 0, &[&tags]));
    let pages: Vec<&[Vec<u8>]> = packets.chunks(10).collect();
    let mut granule = u64::from(PRE_SKIP);
    for (i, chunk) in pages.iter().enumerate() {
        granule += 960 * chunk.len() as u64;
        let flags = if i + 1 == pages.len() { 0x04 } else { 0 };
        let refs: Vec<&[u8]> = chunk.iter().map(Vec::as_slice).collect();
        out.extend(ogg_page(serial, 2 + i as u32, flags, granule, &refs));
    }
    out
}

/// A chained Ogg stream (one physical stream per song, as Icecast Ogg radio sends) switches songs
/// with a new BOS page, which symphonia reports as `ResetRequired`. The decode thread picks the
/// new stream's track and keeps playing: neither a mid-stream `error` end (a live feed) nor a
/// silently dropped second song (a finished input).
#[test]
#[cfg(any(feature = "codec-full", feature = "format-ogg"))]
fn a_chained_ogg_stream_plays_every_link() {
    use crate::tx::opus_stream::tests::opus_packets;
    let _s = SERIAL.lock();
    for finish_first in [false, true] {
        let mut r = erig(64 << 10).unwrap();
        let mut bytes = ogg_opus(0x1111, &opus_packets(50, 960));
        bytes.extend(ogg_opus(0x2222, &opus_packets(50, 960)));
        r.shared.write(&bytes).unwrap();
        if finish_first {
            r.shared.finish();
        }
        // Play as the mixer would; past the first song's end the second one must follow.
        let mut p = r.p.take().unwrap();
        let mut ticks = 0;
        while r.shared.pcm().stats().played_ms < 1_500 {
            assert!(
                ticks < 5_000,
                "stuck at {} ms",
                r.shared.pcm().stats().played_ms
            );
            let pkt = p.format.next_packet().expect("ended at the song change");
            assert!(p.decoder.decode(&pkt).is_ok(), "failed at the song change");
            ticks += 1;
            if r.shared.pcm().queued_frames() == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert_eq!(r.flags.hints() & DECODE_ERROR, 0);
        r.shared.finish();
        r.p = Some(p);
        let (_, errors) = r.play_out(10_000);
        assert_eq!(errors, 0);
        assert_eq!(r.shared.pcm().stats().played_ms, 2_000, "both songs played");
        assert_eq!(r.flags.hints() & DECODE_ERROR, 0);
        assert!(!r.log.calls().contains(&"stop"));
    }
}

/// An input that decodes to nothing ends naturally at its end of input (`finished`, through
/// EOF), not through a `stop()` (which songbird would report as `stopped`).
#[test]
fn an_empty_decoded_stream_ends_through_eof_not_a_stop() {
    let _s = SERIAL.lock();
    let mut r = erig(64 << 10).unwrap();
    r.shared.write(&wav(48_000, 2, &[])).unwrap();
    r.shared.finish();
    wait_until("decoded", || r.shared.pcm().is_closed());
    wait_until("thread exited", || Arc::strong_count(&r.shared) == 1);
    assert_eq!(r.flags.hints() & DECODE_ERROR, 0);
    assert_eq!(
        r.log.calls(),
        vec!["play"],
        "resumed to reach EOF, never stopped"
    );
    let (ticks, errors) = r.play_out(10);
    assert_eq!((ticks, errors), (0, 0), "EOF at the first pull");
    assert_ne!(r.flags.hints() & crate::tracks::flags::EOF_SEEN, 0);
}
