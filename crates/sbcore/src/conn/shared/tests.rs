//! `ConnShared` tests (§13.1 `conn::shared` / `driver`): a real songbird Driver without network,
//! and (feature `test-hooks`) detached connections driven by a manual clock.

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use songbird::input::core::io::MediaSource;
use songbird::input::{AudioStream, Input, LiveInput};

use super::*;
use crate::conn::TEST_LOCK;
use crate::conn::settings::RawVoiceSettings;
use crate::events::Pop;
use crate::rx::queue::RxSource;
use crate::wake::Doorbell;

pub(super) fn bell() -> (Doorbell, UnixStream) {
    let (a, b) = UnixStream::pair().unwrap();
    (Doorbell::from_unix_stream(a).unwrap(), b)
}

pub(super) fn params(clock: Clock, raw: RawVoiceSettings) -> ConnParams {
    ConnParams {
        guild_id: NonZeroU64::new(100).unwrap(),
        user_id: NonZeroU64::new(200).unwrap(),
        reconnect: true,
        settings: VoiceSettings::validate(raw).unwrap(),
        clock,
    }
}

pub(super) fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    f()
}

/// Reads the doorbell peer until EOF (the connection shut its write half down, T9).
pub(super) fn reads_eof(peer: &UnixStream) -> bool {
    // macOS refuses SO_RCVTIMEO (EINVAL) once the other end is fully closed; reads then return 0.
    let _ = peer.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 64];
    let mut r = peer;
    loop {
        match r.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

/// A raw (unprobed) input that owns a `DoneGuard` and reports when it is dropped.
pub(super) struct GuardSource {
    data: Cursor<Vec<u8>>,
    _done: crate::tracks::DoneGuard,
    dropped: Arc<AtomicBool>,
}

impl Read for GuardSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.data.read(buf)
    }
}

impl Seek for GuardSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.data.seek(pos)
    }
}

impl MediaSource for GuardSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.data.get_ref().len() as u64)
    }
}

impl Drop for GuardSource {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

pub(super) fn guard_input(done: crate::tracks::DoneGuard) -> (Input, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    let src = GuardSource {
        data: Cursor::new(Vec::new()),
        _done: done,
        dropped: dropped.clone(),
    };
    (
        Input::Live(
            LiveInput::Raw(AudioStream {
                input: Box::new(src),
            }),
            None,
        ),
        dropped,
    )
}

fn live(c: &'static std::sync::atomic::AtomicU64) -> u64 {
    c.load(Ordering::Acquire)
}

// ── real Driver, no network ──────────────────────────────────────────────────────────────────────

#[test]
fn real_driver_lifecycle_tears_everything_down() {
    let _serial = TEST_LOCK.lock();
    // Only these tests create connections (under TEST_LOCK); earlier real-driver tests may still be
    // winding down (songbird's events task drops its Dispatch clones after Poison). Start from zero.
    let quiet = || {
        live(&live::CONN_SHARED) == 0
            && live(&live::OWNED_DRIVER) == 0
            && live(&live::CONN_WORKER) == 0
            && live(&live::DISPATCH) == 0
    };
    assert!(wait_until(Duration::from_secs(15), quiet));
    let base = (0, 0, 0, 0);
    // Songbird's own tasks are the only oracle that sees a Driver which was never Poisoned (our
    // counters drop when `teardown` returns, `Dispatch` clones go with `RemoveGlobalEvents`, a
    // parked mixer is not a live one). Only Drivers spawn on NET, and every test that creates one
    // holds TEST_LOCK. Besides the Idle task, songbird keeps one process-wide task once its first
    // Driver has run: warm that up, then wait for earlier Drivers' tasks to wind down.
    let warm = VoiceSettings::validate(RawVoiceSettings::default()).unwrap();
    OwnedDriver::new(build_config(&warm, true)).teardown();
    assert!(wait_until(Duration::from_secs(15), || {
        runtime::net_alive_tasks() <= 2
    }));
    let net_base = runtime::net_alive_tasks();
    let (b, peer) = bell();
    let conn = ConnShared::new(params(Clock::Real, RawVoiceSettings::default()), b).unwrap();
    assert_eq!(conn.status().state, "idle");
    assert!(!conn.is_detached());
    assert_eq!(live(&live::OWNED_DRIVER), base.1 + 1);
    // The Driver's core and events tasks and its parked mixer's forwarder run on NET.
    assert!(
        wait_until(Duration::from_secs(2), || runtime::net_alive_tasks()
            >= net_base + 3),
        "{} vs {net_base}",
        runtime::net_alive_tasks()
    );
    assert!(wait_until(Duration::from_secs(2), || live(
        &live::CONN_WORKER
    ) == base.2 + 1));
    assert!(registry_snapshot().iter().any(|c| Arc::ptr_eq(c, &conn)));

    // A track on the parked mixer: dropped inline by SetTrack(None)/Poison at teardown.
    let (track, done) = conn.tracks.register(TrackKind::File);
    let (input, input_dropped) = guard_input(done);
    conn.start_track(&track, input, PlayOpts::default())
        .unwrap();
    assert!(track.is_started() && track.flags.uuid().is_some());
    assert_eq!(conn.tracks.playing(), 1);

    let events = conn.subscribe_events(16, KindMask::ALL).unwrap();
    conn.request_join(NonZeroU64::new(10).unwrap(), false, false);
    conn.set_self_flags(true, false);
    conn.set_bitrate(Some(64_000)).unwrap();
    conn.set_bitrate(None).unwrap();
    assert!(conn.send_muted.load(Ordering::Acquire));
    assert_eq!(conn.status().state, "joining");

    conn.close(CloseReason::Requested);
    conn.close(CloseReason::Dropped);
    // T2–T4 ran synchronously: the Driver is gone.
    assert_eq!(live(&live::OWNED_DRIVER), base.1);
    let s = conn.status();
    assert_eq!(
        (s.state, s.close_reason),
        ("closed", Some(CloseReason::Requested))
    );
    // T6: Disconnected(will_retry=False) is the last event, synchronously.
    let mut last = None;
    loop {
        match events.try_pop() {
            Pop::Item(e) => last = Some(e),
            Pop::Closed(_) => break,
            Pop::Empty => panic!("event queue not closed by T6"),
        }
    }
    assert!(matches!(
        last,
        Some(EventData::Disconnected {
            reason: "requested",
            will_retry: false,
            ..
        })
    ));
    // T8: the track resolved, T9: doorbell EOF, registry entry gone.
    assert_eq!(track.flags.ended(), Some(TrackEndReason::Disconnected));
    assert!(reads_eof(&peer));
    assert!(!registry_snapshot().iter().any(|c| Arc::ptr_eq(c, &conn)));
    assert!(matches!(
        conn.subscribe_events(4, KindMask::ALL),
        Err(CoreError::ConnectionClosed(CloseReason::Requested))
    ));

    // Mixer-side teardown dropped the input, the events task dropped its Dispatch clones
    // (`RemoveGlobalEvents`; this does not prove Poison, the NET task count below does), and the
    // worker exited on the shutdown flag.
    assert!(wait_until(Duration::from_secs(11), || input_dropped
        .load(Ordering::Acquire)));
    drop(conn);
    assert!(wait_until(Duration::from_secs(11), || {
        live(&live::CONN_SHARED) == base.0
            && live(&live::CONN_WORKER) == base.2
            && live(&live::DISPATCH) == base.3
    }));
    // Poison reached songbird: its core runner, events runner and the parked mixer are gone.
    assert!(
        wait_until(Duration::from_secs(11), || runtime::net_alive_tasks()
            <= net_base),
        "songbird tasks still alive after teardown: {} vs {net_base}",
        runtime::net_alive_tasks()
    );
}

#[test]
fn heartbeat_fires_from_a_parked_mixer() {
    let _serial = TEST_LOCK.lock();
    let (b, _peer) = bell();
    let raw = RawVoiceSettings {
        receive: false,
        ..RawVoiceSettings::default()
    };
    let conn = ConnShared::new(params(Clock::Real, raw), b).unwrap();
    let created = conn.dispatch.last_heartbeat();
    // `Periodic(1 s)` fires from the Idle task's ticks even without a connection (§8.8).
    let beat = wait_until(Duration::from_secs(4), || {
        conn.dispatch.last_heartbeat() > created
    });
    conn.close(CloseReason::Cleanup);
    assert!(beat, "no heartbeat from the parked mixer within 4 s");
    assert!(conn.last_heartbeat().is_none(), "no watchdog after close");
}

#[test]
fn close_is_idempotent_from_many_threads() {
    let _serial = TEST_LOCK.lock();
    let (b, peer) = bell();
    let conn = ConnShared::new(params(Clock::Real, RawVoiceSettings::default()), b).unwrap();
    let ctrl = conn.ctrl.clone();
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let c = conn.clone();
            std::thread::spawn(move || {
                c.close(if i % 2 == 0 {
                    CloseReason::Cleanup
                } else {
                    CloseReason::Dropped
                })
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert!(conn.is_closed());
    let r = conn.status().close_reason.unwrap();
    let mut out = Vec::new();
    ctrl.take(&mut out);
    assert_eq!(out, vec![Control::Closed { reason: r }]);
    assert!(reads_eof(&peer));
}

#[test]
fn terminal_transition_from_a_python_thread_completes() {
    let _serial = TEST_LOCK.lock();
    let (b, _peer) = bell();
    let conn = ConnShared::new(params(Clock::Real, RawVoiceSettings::default()), b).unwrap();
    let c = conn.clone();
    let t = std::thread::spawn(move || {
        c.request_join(NonZeroU64::new(10).unwrap(), false, false);
        // Joining + VoiceState(None) → Closed(LeftChannel) → Teardown runs inside apply().
        c.on_voice_state("s", None);
    });
    assert!(wait_until(Duration::from_secs(5), || t.is_finished()));
    t.join().unwrap();
    assert_eq!(conn.status().close_reason, Some(CloseReason::LeftChannel));
}

#[test]
fn t6_is_synchronous_while_the_rx_runtime_is_blocked() {
    let _serial = TEST_LOCK.lock();
    let (b, _peer) = bell();
    let conn = ConnShared::new(params(Clock::Real, RawVoiceSettings::default()), b).unwrap();
    let events = conn.subscribe_events(4, KindMask::ALL).unwrap();
    let token = conn.wake.alloc().unwrap();
    let sub_spec = SubSpec {
        source: RxSource::User(5),
        format: crate::format::Format::new(24_000, 1, crate::format::Encoding::Pcm16Le).unwrap(),
        chunk_ms: 20,
        max_queue_ms: 200,
        overflow: crate::rx::Overflow::DropOldest,
    };
    let sub = SubQueue::new(&sub_spec, token);
    conn.worker.register_queue(&sub);
    // Block the RX worker (the ConnWorker's runtime) until the assertions below are done.
    let g3 = Arc::new(std::sync::Barrier::new(2));
    let g4 = g3.clone();
    let started = Arc::new(AtomicBool::new(false));
    let s2 = started.clone();
    runtime::rx().spawn(async move {
        s2.store(true, Ordering::Release);
        g4.wait();
    });
    assert!(wait_until(Duration::from_secs(5), || started.load(Ordering::Acquire)));
    conn.close(CloseReason::Requested);
    assert!(sub.is_closed());
    assert_eq!(sub.close_reason(), Some(CloseReason::ConnectionClosed));
    assert!(events.is_closed());
    assert!(matches!(
        events.try_pop(),
        Pop::Item(EventData::Disconnected {
            will_retry: false,
            ..
        })
    ));
    g3.wait();
}

#[test]
fn receive_disabled_and_closed_errors() {
    let _serial = TEST_LOCK.lock();
    let (b, _peer) = bell();
    let raw = RawVoiceSettings {
        receive: false,
        ..RawVoiceSettings::default()
    };
    let conn = ConnShared::new(params(Clock::Real, raw), b).unwrap();
    let spec = SubSpec {
        source: RxSource::User(5),
        format: crate::format::Format::new(24_000, 1, crate::format::Encoding::Pcm16Le).unwrap(),
        chunk_ms: 20,
        max_queue_ms: 200,
        overflow: crate::rx::Overflow::DropOldest,
    };
    assert!(matches!(
        conn.subscribe(spec.clone()),
        Err(CoreError::ReceiveDisabled)
    ));
    conn.close(CloseReason::Kicked);
    assert!(matches!(
        conn.subscribe(spec),
        Err(CoreError::ConnectionClosed(CloseReason::Kicked))
    ));
    assert!(matches!(
        conn.set_bitrate(None),
        Err(CoreError::ConnectionClosed(CloseReason::Kicked))
    ));
    // Gateway inputs after close are ignored.
    conn.on_voice_state("s", Some(3));
    assert_eq!(conn.status().state, "closed");
    let names: Vec<&str> = conn.stats().iter().map(|(k, _)| *k).collect();
    for key in [
        "ticks_real",
        "dispatch_reinstalls",
        "connects",
        "gw_unlisted_inputs",
    ] {
        assert!(names.contains(&key), "{key}");
    }
}

/// A deadline armed off the ConnWorker (here the connect task's `ConnectResult(Err)` arming the
/// rejoin backoff) wakes the worker, so `Rejoin` fires on time rather than at the worker's next
/// unrelated timer (the heartbeat watchdog, ~3 s after its last step). `receive: false`: no
/// `DriverDisconnect` → `LinkDown` wake masks the bug.
#[test]
fn a_deadline_armed_off_the_worker_fires_on_time() {
    let _serial = TEST_LOCK.lock();
    let (b, _peer) = bell();
    let raw = RawVoiceSettings {
        receive: false,
        driver_retries: 1,
        driver_timeout_s: 1.0,
        ..RawVoiceSettings::default()
    };
    let conn = ConnShared::new(params(Clock::Real, raw), b).unwrap();
    conn.request_join(NonZeroU64::new(10).unwrap(), false, false);
    conn.on_voice_state("s", Some(10));
    // Port 1 on loopback refuses at once: the attempt fails and the rejoin backoff is armed.
    conn.on_voice_server("t", Some("127.0.0.1:1"));
    let mut due = None;
    assert!(wait_until(Duration::from_secs(5), || {
        due = (conn.status().state == "reconnecting")
            .then(|| conn.next_deadline())
            .flatten();
        due.is_some()
    }));
    let due = due.unwrap();
    let mut out = Vec::new();
    let mut rejoined = None;
    assert!(wait_until(Duration::from_secs(6), || {
        out.clear();
        conn.take_controls(8, &mut out);
        if out.iter().any(|c| matches!(c, Control::Rejoin { .. })) {
            rejoined = Some(Instant::now());
        }
        rejoined.is_some()
    }));
    conn.close(CloseReason::Cleanup);
    let late = rejoined.unwrap().saturating_duration_since(due);
    assert!(
        late < Duration::from_millis(500),
        "Rejoin fired {late:?} after its deadline"
    );
}

#[test]
fn dropping_the_last_reference_closes() {
    let _serial = TEST_LOCK.lock();
    let (b, peer) = bell();
    let conn = ConnShared::new(params(Clock::Real, RawVoiceSettings::default()), b).unwrap();
    let weak = Arc::downgrade(&conn);
    drop(conn);
    assert!(weak.upgrade().is_none());
    assert!(reads_eof(&peer));
}

#[test]
fn shutdown_all_closes_every_registered_connection() {
    let _serial = TEST_LOCK.lock();
    let (b1, _p1) = bell();
    let (b2, _p2) = bell();
    let c1 = ConnShared::new(params(Clock::Real, RawVoiceSettings::default()), b1).unwrap();
    let c2 = ConnShared::new(params(Clock::Real, RawVoiceSettings::default()), b2).unwrap();
    shutdown_all(CloseReason::ProcessExit);
    for c in [&c1, &c2] {
        assert_eq!(c.status().close_reason, Some(CloseReason::ProcessExit));
    }
    assert!(
        !registry_snapshot()
            .iter()
            .any(|c| Arc::ptr_eq(c, &c1) || Arc::ptr_eq(c, &c2))
    );
}

#[test]
fn an_op_started_while_closing_is_failed_at_once() {
    let _serial = TEST_LOCK.lock();
    let (b, _peer) = bell();
    let conn = ConnShared::new(params(Clock::Real, RawVoiceSettings::default()), b).unwrap();
    conn.close(CloseReason::Requested);
    // An op that passed `ensure_open` before T0 but registered after T8's `fail_all`.
    let id = conn
        .ops
        .start(&conn.wake, std::future::pending::<OpResult>())
        .unwrap();
    conn.fail_ops_if_closed();
    let mut out = Vec::new();
    conn.take_op_results(&mut out);
    assert!(
        matches!(out[..], [(i, OpResult::Err(CoreError::ConnectionClosed(_)))] if i == id),
        "{out:?}"
    );
    assert_eq!(conn.ops.pending(), 0);
}

#[test]
fn an_executor_panic_does_not_wedge_the_state_machine() {
    let _serial = TEST_LOCK.lock();
    let (b, _peer) = bell();
    let conn = ConnShared::new(params(Clock::Real, RawVoiceSettings::default()), b).unwrap();
    conn.panic_next_execute.store(true, Ordering::Release);
    // Join → SetSendMute(false): its execution unwinds (as a bug in an action would).
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        conn.request_join(NonZeroU64::new(10).unwrap(), false, false)
    }));
    assert!(r.is_err());
    assert_eq!(conn.status().state, "joining");
    // Later batches still execute (a stuck `executing` flag would only queue them).
    conn.set_self_flags(true, false);
    assert!(conn.send_muted.load(Ordering::Acquire));
    assert!(conn.pending.lock().is_empty());
    conn.close(CloseReason::Cleanup);
    assert_eq!(conn.status().state, "closed");
}

#[test]
fn a_forked_child_leaks_the_driver_instead_of_dropping_it() {
    let _serial = TEST_LOCK.lock();
    // A connection of an earlier test may still be closing on a runtime thread (its last `Arc`
    // was a worker's temporary upgrade): start from no Driver, or the counts below race it.
    assert!(wait_until(Duration::from_secs(15), || live(
        &live::OWNED_DRIVER
    ) == 0));
    let (b, _peer) = bell();
    let foreign = std::process::id().wrapping_add(1);
    let conn =
        ConnShared::new_with_pid(params(Clock::Real, RawVoiceSettings::default()), b, foreign)
            .unwrap();
    let events = conn.subscribe_events(4, KindMask::ALL).unwrap();
    let drivers = live(&live::OWNED_DRIVER);
    // In the child, close() and Drop skip T1–T9 (§4.10)…
    conn.close(CloseReason::LoopClosed);
    assert_eq!(conn.status().state, "idle", "T1 skipped");
    assert!(!events.is_closed(), "T6 skipped");
    assert_eq!(live(&live::OWNED_DRIVER), drivers, "T2–T4 skipped");
    // …and the last reference leaks the inherited Driver (no Poison from the child).
    drop(conn);
    let leaked: Vec<OwnedDriver> = std::mem::take(&mut *LEAKED_DRIVERS.lock());
    assert_eq!(leaked.len(), 1);
    assert_eq!(live(&live::OWNED_DRIVER), drivers);
    // This test runs in the creating process: tear the "leaked" Driver down for real.
    for d in leaked {
        d.teardown();
    }
    assert_eq!(live(&live::OWNED_DRIVER), drivers - 1);
}

#[test]
fn entries_enqueued_while_closing_always_end() {
    let _serial = TEST_LOCK.lock();
    for _ in 0..20 {
        let (b, _peer) = bell();
        let conn = ConnShared::new(params(Clock::Real, RawVoiceSettings::default()), b).unwrap();
        let start = Arc::new(std::sync::Barrier::new(5));
        let enqueuers: Vec<_> = (0..4)
            .map(|_| {
                let (c, start) = (conn.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    let mut got = Vec::new();
                    while let Ok(t) = c.queue().enqueue(
                        SourceSpec::Encoded(crate::tx::source::EncodedFactory {
                            spec: EncodedSpec {
                                max_buffer_bytes: 1 << 20,
                                volume: 1.0,
                            },
                        }),
                        PlayOpts::default(),
                        c.tracks(),
                    ) {
                        got.push(t);
                        if got.len() >= 2_000 {
                            break;
                        }
                    }
                    got
                })
            })
            .collect();
        start.wait();
        conn.close(CloseReason::Requested);
        for t in enqueuers {
            for track in t.join().unwrap() {
                assert!(
                    track.flags.ended().is_some(),
                    "a pending entry outlived the connection unresolved"
                );
            }
        }
    }
}

/// songbird runs track commands only on a live mixer, and a connection that is joining (or
/// reconnecting) has none: every closed stream and stopped track stays in the parked mixer. The
/// user-facing caps count only what is still open or playing (a closed stream frees its buffer;
/// a stopped memory track resolves `stopped` at once, like `TrackQueue::skip`, so `wait_ended()`
/// and `TrackEnded` do not wait for the link), and `resident_tracks` keeps songbird within its 48
/// preallocated tracks.
#[test]
fn a_parked_mixer_keeps_closed_streams_and_stopped_tracks_off_the_user_caps() {
    use crate::format::{PCM16_24K, PCM16_48K};
    use crate::limits::{MAX_LIVE_SOURCES_PER_CONN, PREALLOCATED_TRACKS};
    use crate::tx::memory::MemPcm;
    use crate::tx::queue::tests::spec;
    let _serial = TEST_LOCK.lock();
    let (b, _peer) = bell();
    let conn = ConnShared::new(params(Clock::Real, RawVoiceSettings::default()), b).unwrap();
    let events = conn.subscribe_events(256, KindMask::ALL).unwrap();
    conn.request_join(NonZeroU64::new(10).unwrap(), false, false);
    assert_eq!(conn.status().state, "joining");
    // One playback per utterance, each closed: none is open, so the 17th opens.
    for _ in 0..=MAX_LIVE_SOURCES_PER_CONN {
        let (q, _track) = conn.open_playback(spec(PCM16_24K)).unwrap();
        LiveSource::close(&*q, CloseMode::Abort);
    }
    assert_eq!(conn.open_live_sources(), 0);
    assert_eq!(conn.tracks.resident(), MAX_LIVE_SOURCES_PER_CONN + 1);
    // Short clips played and stopped: resolved `stopped` now, off `MAX_PLAYING_TRACKS_PER_CONN`.
    let clip = SourceSpec::Pcm(Arc::new(
        MemPcm::from_i16(PCM16_48K, vec![0; 9_600]).unwrap(),
    ));
    let free = usize::from(PREALLOCATED_TRACKS) - conn.tracks.resident();
    let mut stopped = Vec::new();
    for _ in 0..free {
        let t = conn.play(&clip, PlayOpts::default()).unwrap();
        t.stop().unwrap();
        assert_eq!(t.flags.ended(), Some(TrackEndReason::Stopped));
        stopped.push(t);
    }
    assert_eq!(conn.tracks.playing(), 0);
    // songbird still holds all of them: the 49th track is refused, as `resident_tracks`.
    assert_eq!(conn.tracks.resident(), usize::from(PREALLOCATED_TRACKS));
    for r in [
        conn.play(&clip, PlayOpts::default()).map(drop),
        conn.open_playback(spec(PCM16_24K)).map(drop),
    ] {
        match r {
            Err(CoreError::Limit(e)) => assert_eq!(e.what, "resident_tracks"),
            other => panic!("{other:?}"),
        }
    }
    // Every stop is reported while the link is still down.
    let mut ended = 0;
    assert!(wait_until(Duration::from_secs(5), || {
        while let Pop::Item(e) = events.try_pop() {
            if let EventData::TrackEnded {
                reason: TrackEndReason::Stopped,
                ..
            } = e
            {
                ended += 1;
            }
        }
        ended == stopped.len()
    }));
    conn.close(CloseReason::Requested);
    assert!(
        stopped
            .iter()
            .all(|t| t.flags.ended() == Some(TrackEndReason::Stopped))
    );
}

#[cfg(feature = "test-hooks")]
mod detached;
