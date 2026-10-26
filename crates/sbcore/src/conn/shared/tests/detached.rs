//! Detached-connection tests (feature `test-hooks`): no Driver, manual clock, the worker stepped
//! synchronously, fake track controls.

use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::clock::ManualClock;
use crate::conn::gateway::{DiscKind, DiscReason, Phase};
use crate::events::Pop;
use crate::tx::encoded::EncodedSpec;
use crate::tx::source::EncodedFactory;
use crate::wake::{WAKE_CONN, WAKE_OPS, WAKE_TRACKS};

struct D {
    conn: Arc<ConnShared>,
    worker: ConnWorker,
    clock: Arc<ManualClock>,
    peer: UnixStream,
}

impl D {
    fn new(raw: RawVoiceSettings) -> Self {
        let (clock, m) = Clock::manual();
        let (b, peer) = bell();
        let (conn, worker) = ConnShared::new_detached(params(clock, raw), b).unwrap();
        D {
            conn,
            worker,
            clock: m,
            peer,
        }
    }

    fn step(&mut self, advance_ms: u64) -> Option<Instant> {
        self.clock.advance(Duration::from_millis(advance_ms));
        self.worker.step(self.clock.now())
    }

    fn connect(&self) {
        self.conn
            .request_join(NonZeroU64::new(10).unwrap(), false, false);
        self.conn.on_voice_state("s1", Some(10));
        self.conn
            .on_voice_server("t1", Some("wss://e1.discord.media/"));
    }

    fn ready(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.conn.take_ready(&mut out);
        out
    }

    fn controls(&self) -> Vec<Control> {
        let mut out = Vec::new();
        self.conn.take_controls(usize::MAX, &mut out);
        out
    }

    /// A started (parked) memory track whose input reports its drop.
    fn parked_track(&self) -> (TrackRef, Arc<AtomicBool>) {
        let (track, done) = self.conn.tracks.register(TrackKind::File);
        let (input, dropped) = guard_input(done);
        self.conn
            .start_track(&track, input, PlayOpts::default())
            .unwrap();
        (track, dropped)
    }
}

fn drain(q: &EventQueue) -> Vec<EventData> {
    let mut out = Vec::new();
    while let Pop::Item(e) = q.try_pop() {
        out.push(e);
    }
    out
}

fn encoded() -> SourceSpec {
    SourceSpec::Encoded(EncodedFactory {
        spec: EncodedSpec {
            max_buffer_bytes: 1 << 20,
            volume: 1.0,
        },
    })
}

#[test]
fn detached_flow_connects_instantly_and_reports() {
    let _serial = TEST_LOCK.lock();
    let mut d = D::new(RawVoiceSettings::default());
    assert!(d.conn.is_detached());
    let events = d.conn.subscribe_events(16, KindMask::ALL).unwrap();
    d.connect();
    let s = d.conn.status();
    assert_eq!(
        (s.state, s.channel_id, s.generation),
        ("connected", Some(10), 1)
    );
    assert!(d.ready().contains(&WAKE_CONN.0));
    d.conn.inject_driver_connect(77, false);
    assert_eq!(d.conn.status().ssrc, Some(77));
    assert!(matches!(
        drain(&events)[..],
        [EventData::Connected { channel_id: 10, .. }]
    ));
    // The worker consumes the RxLink commands and the LinkUp inputs.
    d.step(20);
    assert!(d.conn.rx_sender().is_some());
    d.conn.close(CloseReason::Requested);
    assert!(reads_eof(&d.peer));
    assert!(d.step(20).is_none(), "the worker stops after T6");
}

#[test]
fn external_move_is_reported_as_a_control() {
    let _serial = TEST_LOCK.lock();
    let d = D::new(RawVoiceSettings::default());
    d.connect();
    d.ready();
    d.conn.on_voice_state("s1", Some(20));
    assert_eq!(d.conn.status().state, "joining");
    assert_eq!(d.controls(), vec![Control::ChannelMoved { channel_id: 20 }]);
    assert!(d.ready().contains(&crate::wake::WAKE_CTRL.0));
    d.conn.on_voice_server("t2", Some("e2.discord.media"));
    let s = d.conn.status();
    assert_eq!(
        (s.state, s.channel_id, s.generation),
        ("connected", Some(20), 2)
    );
    d.conn.close(CloseReason::Cleanup);
}

#[test]
fn teardown_closes_everything_synchronously() {
    let _serial = TEST_LOCK.lock();
    let mut d = D::new(RawVoiceSettings::default());
    d.connect();
    let conn = d.conn.clone();
    // T6 targets: a subscription queue, a router queue and an event stream.
    let sub = SubQueue::new(
        &SubSpec {
            source: RxSource::User(5),
            format: crate::format::Format::new(16_000, 1, crate::format::Encoding::Pcm16Le)
                .unwrap(),
            chunk_ms: 20,
            max_queue_ms: 200,
            overflow: crate::rx::Overflow::DropOldest,
        },
        conn.wake.alloc().unwrap(),
    );
    conn.worker.register_queue(&sub);
    let router = RouterQueue::new(conn.wake.alloc().unwrap());
    conn.worker.register_router(&router);
    let events = conn.subscribe_events(4, KindMask::ALL).unwrap();
    // T8 targets: a started track, a pending queue entry, a pending op.
    let (track, input_dropped) = d.parked_track();
    let pending = conn
        .queue()
        .enqueue(encoded(), PlayOpts::default(), conn.tracks())
        .unwrap();
    conn.queue().pause(); // keep it pending
    let op = conn
        .ops
        .start(&conn.wake, std::future::pending::<OpResult>())
        .unwrap();
    d.ready();

    conn.close(CloseReason::Requested);

    assert_eq!(sub.close_reason(), Some(CloseReason::ConnectionClosed));
    assert!(matches!(router.try_pop_or_arm(), Pop::Closed(_)));
    let evs = drain(&events);
    assert!(matches!(
        evs.last(),
        Some(EventData::Disconnected {
            reason: "requested",
            code: None,
            will_retry: false
        })
    ));
    assert!(events.is_closed());
    assert!(
        input_dropped.load(Ordering::Acquire),
        "parked inputs dropped"
    );
    assert_eq!(track.flags.ended(), Some(TrackEndReason::Disconnected));
    assert_eq!(pending.flags.ended(), Some(TrackEndReason::Disconnected));
    assert!(conn.queue().is_empty());
    let mut results = Vec::new();
    conn.take_op_results(&mut results);
    assert!(matches!(
        results[..],
        [(id, OpResult::Err(CoreError::ConnectionClosed(_)))] if id == op
    ));
    assert_eq!(
        d.controls(),
        vec![Control::Closed {
            reason: CloseReason::Requested
        }]
    );
    let ready = d.ready();
    for id in [WAKE_TRACKS, WAKE_OPS, WAKE_CONN] {
        assert!(ready.contains(&id.0), "{id:?} in {ready:?}");
    }
    assert!(reads_eof(&d.peer));
    assert!(!registry_snapshot().iter().any(|c| Arc::ptr_eq(c, &conn)));
    assert!(d.step(20).is_none());
    // Idempotent.
    conn.close(CloseReason::Dropped);
    assert_eq!(conn.status().close_reason, Some(CloseReason::Requested));
}

#[test]
fn leaving_the_channel_tears_down_through_the_state_machine() {
    let _serial = TEST_LOCK.lock();
    let d = D::new(RawVoiceSettings::default());
    let events = d.conn.subscribe_events(8, KindMask::ALL).unwrap();
    d.connect();
    d.conn.on_voice_state("s1", None);
    let s = d.conn.status();
    assert_eq!(
        (s.state, s.close_reason),
        ("closed", Some(CloseReason::LeftChannel))
    );
    let evs = drain(&events);
    assert!(matches!(
        evs.last(),
        Some(EventData::Disconnected {
            reason: "left_channel",
            will_retry: false,
            ..
        })
    ));
    let disconnects = evs
        .iter()
        .filter(|e| matches!(e, EventData::Disconnected { .. }))
        .count();
    assert_eq!(disconnects, 1, "the final event is emitted once");
    assert_eq!(
        d.controls(),
        vec![Control::Closed {
            reason: CloseReason::LeftChannel
        }]
    );
}

#[test]
fn terminal_transition_inside_dispatch_completes() {
    let _serial = TEST_LOCK.lock();
    let d = D::new(RawVoiceSettings::default());
    d.connect();
    let c = d.conn.clone();
    let t = std::thread::spawn(move || {
        c.inject_driver_disconnect(DiscKind::Runtime, Some(DiscReason::WsClosed(Some(4017))));
    });
    assert!(wait_until(Duration::from_secs(5), || t.is_finished()));
    t.join().unwrap();
    let s = d.conn.status();
    assert_eq!(
        s.close_reason,
        Some(CloseReason::SessionInvalid(Some(4017)))
    );
    assert_eq!(s.close_code, Some(4017));
}

#[test]
fn apply_ordering_under_contention() {
    let _serial = TEST_LOCK.lock();
    let d = D::new(RawVoiceSettings::default());
    d.conn.set_detached_auto_connect(false);
    d.connect();
    assert_eq!(d.conn.status().state, "connecting");
    const N: usize = 3_000;
    let threads: Vec<_> = (0..3)
        .map(|t| {
            let c = d.conn.clone();
            std::thread::spawn(move || {
                for i in 0..N {
                    match (i + t) % 4 {
                        0 => c.on_voice_server(&format!("tok-{t}-{i}"), Some("e.discord.media")),
                        1 => c.inject_gateway(GwInput::ConnectResult {
                            generation: c.status().generation,
                            result: Ok(()),
                        }),
                        2 => c.request_move(
                            NonZeroU64::new(if i % 2 == 0 { 20 } else { 10 }).unwrap(),
                        ),
                        _ => c.on_voice_state("s1", Some(if i % 3 == 0 { 20 } else { 10 })),
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let log = d.conn.exec_log();
    let gens: Vec<u64> = log
        .iter()
        .filter_map(|e| match e {
            Exec::Connect(g) => Some(*g),
            _ => None,
        })
        .collect();
    assert!(
        gens.windows(2).all(|w| w[0] < w[1]),
        "Connect generations reordered"
    );
    assert_eq!(gens.last().copied(), Some(d.conn.status().generation));
    let seqs: Vec<u64> = log
        .iter()
        .filter_map(|e| match e {
            Exec::Link { seq, .. } => Some(*seq),
            _ => None,
        })
        .collect();
    assert!(!seqs.is_empty());
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "RxLink reordered");
    d.conn.close(CloseReason::Cleanup);
}

#[test]
fn worker_fires_gateway_deadlines() {
    let _serial = TEST_LOCK.lock();
    let mut d = D::new(RawVoiceSettings::default());
    d.connect();
    d.conn
        .inject_driver_disconnect(DiscKind::Runtime, Some(DiscReason::WsClosed(Some(4014))));
    assert!(matches!(
        d.conn.gw.lock().phase(),
        Phase::AwaitingMove { .. }
    ));
    let next = d.step(20).unwrap();
    assert_eq!(next, d.clock.now() + Duration::from_millis(4_980));
    d.step(4_980);
    assert!(matches!(
        d.conn.gw.lock().phase(),
        Phase::AwaitingRejoin { attempt: 1, .. }
    ));
    let next = d.step(0).unwrap();
    assert!(matches!(
        d.conn.gw.lock().phase(),
        Phase::Rejoining { attempt: 1, .. }
    ));
    assert_eq!(next, d.clock.now() + Duration::from_secs(15));
    assert_eq!(
        d.controls(),
        vec![Control::Rejoin {
            attempt: 1,
            channel_id: 10
        }]
    );
    d.conn.close(CloseReason::Cleanup);
}

#[test]
fn worker_finalises_dropped_tracks_and_reports_track_ended() {
    let _serial = TEST_LOCK.lock();
    let mut d = D::new(RawVoiceSettings::default());
    let events = d.conn.subscribe_events(8, KindMask::ALL).unwrap();
    let (track, dropped) = d.parked_track();
    assert!(d.conn.tracks_need_poll());
    assert_eq!(d.conn.tracks().playing(), 1);
    // The reader is dropped without an End event (e.g. Driver::stop()).
    drop(d.conn.take_parked_input(&track));
    assert!(dropped.load(Ordering::Acquire));
    d.ready();
    d.step(20);
    d.step(20);
    assert_eq!(track.flags.ended(), None, "2-tick grace");
    d.step(20);
    assert_eq!(track.flags.ended(), Some(TrackEndReason::Stopped));
    assert!(matches!(
        drain(&events)[..],
        [EventData::TrackEnded {
            reason: TrackEndReason::Stopped,
            track_id
        }] if track_id == track.flags.id
    ));
    assert!(d.ready().contains(&WAKE_TRACKS.0));
    assert!(!d.conn.tracks_need_poll());
    assert_eq!(d.conn.tracks().playing(), 0);
    d.conn.close(CloseReason::Cleanup);
}

#[test]
fn fake_control_records_calls_and_stop_resolves() {
    let _serial = TEST_LOCK.lock();
    let mut d = D::new(RawVoiceSettings::default());
    let (track, _dropped) = d.parked_track();
    track.pause().unwrap();
    track.resume().unwrap();
    track.set_volume(0.5).unwrap();
    track.stop().unwrap();
    assert_eq!(track.fake_log(), vec!["pause", "play", "stop"]);
    assert_eq!(track.flags.ended(), Some(TrackEndReason::Stopped));
    d.step(20);
    assert!(!d.conn.tracks_need_poll());
    // Seek/info need a songbird handle.
    assert!(matches!(
        d.conn.start_track_info(&track, Duration::from_millis(10)),
        Err(CoreError::Track(_))
    ));
    d.conn.close(CloseReason::Cleanup);
}

#[test]
fn stop_all_drops_every_parked_input() {
    let _serial = TEST_LOCK.lock();
    let mut d = D::new(RawVoiceSettings::default());
    let (a, da) = d.parked_track();
    let (b, db) = d.parked_track();
    let pending = d
        .conn
        .queue()
        .enqueue(encoded(), PlayOpts::default(), d.conn.tracks())
        .unwrap();
    d.conn.queue().pause();
    d.conn.stop_all();
    assert!(da.load(Ordering::Acquire) && db.load(Ordering::Acquire));
    assert_eq!(pending.flags.ended(), Some(TrackEndReason::Stopped));
    for _ in 0..3 {
        d.step(20);
    }
    assert_eq!(a.flags.ended(), Some(TrackEndReason::Stopped));
    assert_eq!(b.flags.ended(), Some(TrackEndReason::Stopped));
    assert!(!d.conn.is_closed(), "the connection stays up");
    d.conn.close(CloseReason::Cleanup);
}

#[test]
fn queue_entry_becomes_current_through_the_worker() {
    let _serial = TEST_LOCK.lock();
    let mut d = D::new(RawVoiceSettings::default());
    let entry = d
        .conn
        .queue()
        .enqueue(encoded(), PlayOpts::default(), d.conn.tracks())
        .unwrap();
    assert!(d.conn.tracks_need_poll(), "an entry waits to be advanced");
    d.step(20);
    assert_eq!(d.conn.queue().len(), 0);
    // It started (a paused live source with a fake control) and is current; its stream is handed
    // out once and counts towards the playing tracks, not the `open_*` live sources.
    assert!(entry.is_started());
    assert!(d.conn.queue().current().is_some_and(|c| c.same(&entry)));
    assert_eq!(entry.fake_log(), Vec::<&str>::new(), "started paused");
    let stream = d.conn.queue_take_stream(&entry).expect("the lazy stream");
    assert!(d.conn.queue_take_stream(&entry).is_none());
    assert_eq!(d.conn.queue().live_current(), 1);
    d.conn.close(CloseReason::Cleanup);
    assert_eq!(entry.flags.ended(), Some(TrackEndReason::Disconnected));
    assert!(matches!(
        stream.write(b"x"),
        Err(CoreError::StreamClosed(CloseReason::ConnectionClosed))
    ));
}

#[test]
fn flags_mirror_send_mute_and_unclassified_users_are_taken() {
    let _serial = TEST_LOCK.lock();
    let d = D::new(RawVoiceSettings::default());
    d.connect();
    d.conn.set_self_flags(true, true);
    assert!(d.conn.send_muted.load(Ordering::Acquire));
    let s = d.conn.status();
    assert!(s.self_mute && s.self_deaf);
    d.conn.set_self_flags(false, true);
    assert!(!d.conn.send_muted.load(Ordering::Acquire));
    d.conn.unclassified.lock().extend([5, 6]);
    assert_eq!(d.conn.take_unclassified_users(), vec![5, 6]);
    assert!(d.conn.take_unclassified_users().is_empty());
    d.conn.set_users_bot(&[(5, true)]);
    d.conn.close(CloseReason::Cleanup);
}

#[test]
fn injected_connect_failure_schedules_a_rejoin() {
    let _serial = TEST_LOCK.lock();
    let d = D::new(RawVoiceSettings::default());
    d.conn.set_detached_auto_connect(false);
    d.connect();
    d.conn.inject_gateway(GwInput::ConnectResult {
        generation: 1,
        result: Err(ConnectFailure::TimedOut),
    });
    assert!(matches!(
        d.conn.gw.lock().phase(),
        Phase::AwaitingRejoin { attempt: 1, .. }
    ));
    assert_eq!(d.conn.status().state, "reconnecting");
    d.conn.close(CloseReason::Cleanup);
}
