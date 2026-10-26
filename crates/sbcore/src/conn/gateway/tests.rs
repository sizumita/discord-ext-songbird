//! Table tests for the gateway state machine (§4.5, §13.1): every phase × input pair, plus the
//! row-specific scenarios.

use super::*;

/// Phase classes used by the matrix (Joining is split by whether the move fallback is armed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum P {
    Idle,
    Joining,
    JoiningMoved,
    Connecting,
    Connected,
    AwaitingMove,
    AwaitingRejoin,
    Rejoining,
    Closed,
}

/// Action classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum A {
    Connect,
    Mute,
    Emit,
    Ctrl,
    LinkUp,
    LinkDown,
    Reinstall,
    Teardown,
    Notify,
}

fn nz(n: u64) -> NonZeroU64 {
    NonZeroU64::new(n).unwrap()
}

fn params(reconnect: bool, rejoin_attempts: u8) -> GatewayParams {
    GatewayParams::new(nz(100), nz(200), reconnect, rejoin_attempts, true)
}

struct H {
    g: GatewayState,
    now: Instant,
}

impl H {
    fn new(p: GatewayParams) -> Self {
        H {
            g: GatewayState::new(p),
            now: Instant::now(),
        }
    }

    fn step(&mut self, i: GwInput) -> Vec<GwAction> {
        let mut out = SmallVec::new();
        self.g.step(i, self.now, &mut out);
        out.into_vec()
    }

    fn advance(&mut self, d: Duration) {
        self.now += d;
    }

    fn class(&self) -> P {
        match self.g.phase {
            Phase::Idle => P::Idle,
            Phase::Joining if self.g.fallback.is_some() => P::JoiningMoved,
            Phase::Joining => P::Joining,
            Phase::Connecting { .. } => P::Connecting,
            Phase::Connected => P::Connected,
            Phase::AwaitingMove { .. } => P::AwaitingMove,
            Phase::AwaitingRejoin { .. } => P::AwaitingRejoin,
            Phase::Rejoining { .. } => P::Rejoining,
            Phase::Closed(_) => P::Closed,
        }
    }
}

fn classes(actions: &[GwAction]) -> Vec<A> {
    actions
        .iter()
        .map(|a| match a {
            GwAction::Connect { .. } => A::Connect,
            GwAction::SetSendMute(_) => A::Mute,
            GwAction::Emit(_) => A::Emit,
            GwAction::Control(_) => A::Ctrl,
            GwAction::RxLink { up: true, .. } => A::LinkUp,
            GwAction::RxLink { up: false, .. } => A::LinkDown,
            GwAction::ReinstallDispatch => A::Reinstall,
            GwAction::Teardown(_) => A::Teardown,
            GwAction::NotifyStatus => A::Notify,
        })
        .collect()
}

fn vs(session: &str, channel: Option<u64>) -> GwInput {
    GwInput::VoiceState {
        session_id: session.into(),
        channel,
    }
}

fn vsrv(token: &str, endpoint: Option<&str>) -> GwInput {
    GwInput::VoiceServer {
        token: token.into(),
        endpoint: endpoint.map(Into::into),
    }
}

fn disc(kind: DiscKind, reason: Option<DiscReason>) -> GwInput {
    GwInput::DriverDisconnect { kind, reason }
}

const E1: &str = "wss://e1.discord.media/";
const E2: &str = "e2.discord.media:443";

// ── phase builders ───────────────────────────────────────────────────────────────────────────────

fn build(p: P, prm: GatewayParams) -> H {
    let mut h = H::new(prm);
    if p == P::Idle {
        return h;
    }
    if p == P::Closed {
        h.step(GwInput::Join {
            channel: nz(10),
            self_mute: false,
            self_deaf: false,
        });
        h.step(GwInput::Close(CloseReason::Requested));
        assert_eq!(h.class(), P::Closed);
        return h;
    }
    h.step(GwInput::Join {
        channel: nz(10),
        self_mute: false,
        self_deaf: false,
    });
    if p == P::Joining {
        assert_eq!(h.class(), P::Joining);
        return h;
    }
    h.step(vs("s1", Some(10)));
    h.step(vsrv("t1", Some(E1)));
    if p == P::Connecting {
        assert_eq!(h.class(), P::Connecting);
        return h;
    }
    h.step(GwInput::ConnectResult {
        generation: 1,
        result: Ok(()),
    });
    h.step(GwInput::DriverConnect { ssrc: 7 });
    assert_eq!(h.class(), P::Connected);
    match p {
        P::Connected => {}
        P::JoiningMoved => {
            h.step(GwInput::Move { channel: nz(20) });
        }
        P::AwaitingMove => {
            h.step(disc(
                DiscKind::Runtime,
                Some(DiscReason::WsClosed(Some(4014))),
            ));
        }
        P::AwaitingRejoin => {
            h.step(disc(DiscKind::Runtime, Some(DiscReason::Io)));
        }
        P::Rejoining => {
            h.step(GwInput::BeginRejoin);
        }
        _ => unreachable!(),
    }
    assert_eq!(h.class(), p);
    h
}

const ALL_PHASES: [P; 9] = [
    P::Idle,
    P::Joining,
    P::JoiningMoved,
    P::Connecting,
    P::Connected,
    P::AwaitingMove,
    P::AwaitingRejoin,
    P::Rejoining,
    P::Closed,
];

/// Representative inputs. `Deadline` is stepped after advancing the clock by 20 s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum I {
    Join,
    Move,
    BeginRejoin,
    Leave,
    SetFlags,
    StateNone,
    StateSame,
    StateNewSession,
    StateOtherChannel,
    ServerNone,
    ServerSame,
    ServerChanged,
    ResultOk,
    ResultErr,
    ResultDiscarded,
    ResultStale,
    DrvConnect,
    DrvReconnect,
    DiscConnectIo,
    DiscRequested,
    Disc4014,
    Disc4006,
    Disc4017,
    DiscReconnectIo,
    DiscNoReason,
    Stalled,
    HandlersLost,
    Deadline,
    Close,
}

const ALL_INPUTS: [I; 29] = [
    I::Join,
    I::Move,
    I::BeginRejoin,
    I::Leave,
    I::SetFlags,
    I::StateNone,
    I::StateSame,
    I::StateNewSession,
    I::StateOtherChannel,
    I::ServerNone,
    I::ServerSame,
    I::ServerChanged,
    I::ResultOk,
    I::ResultErr,
    I::ResultDiscarded,
    I::ResultStale,
    I::DrvConnect,
    I::DrvReconnect,
    I::DiscConnectIo,
    I::DiscRequested,
    I::Disc4014,
    I::Disc4006,
    I::Disc4017,
    I::DiscReconnectIo,
    I::DiscNoReason,
    I::Stalled,
    I::HandlersLost,
    I::Deadline,
    I::Close,
];

fn input(i: I, h: &mut H) -> GwInput {
    let gen_now = h.g.generation;
    match i {
        I::Join => GwInput::Join {
            channel: nz(30),
            self_mute: true,
            self_deaf: false,
        },
        I::Move => GwInput::Move { channel: nz(30) },
        I::BeginRejoin => GwInput::BeginRejoin,
        I::Leave => GwInput::Leave,
        I::SetFlags => GwInput::SetFlags {
            self_mute: true,
            self_deaf: false,
        },
        I::StateNone => vs("s1", None),
        I::StateSame => vs("s1", Some(10)),
        I::StateNewSession => vs("s2", Some(10)),
        I::StateOtherChannel => vs("s1", Some(30)),
        I::ServerNone => vsrv("t1", None),
        I::ServerSame => vsrv("t1", Some(E1)),
        I::ServerChanged => vsrv("t2", Some(E2)),
        I::ResultOk => GwInput::ConnectResult {
            generation: gen_now,
            result: Ok(()),
        },
        I::ResultErr => GwInput::ConnectResult {
            generation: gen_now,
            result: Err(ConnectFailure::Other("boom".into())),
        },
        I::ResultDiscarded => GwInput::ConnectResult {
            generation: gen_now,
            result: Err(ConnectFailure::AttemptDiscarded),
        },
        I::ResultStale => GwInput::ConnectResult {
            generation: gen_now + 5,
            result: Ok(()),
        },
        I::DrvConnect => GwInput::DriverConnect { ssrc: 8 },
        I::DrvReconnect => GwInput::DriverReconnect { ssrc: 8 },
        I::DiscConnectIo => disc(DiscKind::Connect, Some(DiscReason::Io)),
        I::DiscRequested => disc(DiscKind::Runtime, Some(DiscReason::Requested)),
        I::Disc4014 => disc(DiscKind::Runtime, Some(DiscReason::WsClosed(Some(4014)))),
        I::Disc4006 => disc(DiscKind::Runtime, Some(DiscReason::WsClosed(Some(4006)))),
        I::Disc4017 => disc(DiscKind::Runtime, Some(DiscReason::WsClosed(Some(4017)))),
        I::DiscReconnectIo => disc(DiscKind::Reconnect, Some(DiscReason::Io)),
        I::DiscNoReason => disc(DiscKind::Runtime, None),
        I::Stalled => GwInput::RxStalled {
            since: h.now - Duration::from_secs(6),
        },
        I::HandlersLost => GwInput::HandlersLost,
        I::Deadline => {
            h.advance(Duration::from_secs(20));
            GwInput::Deadline
        }
        I::Close => GwInput::Close(CloseReason::Cleanup),
    }
}

/// `None` = the pair is not listed in §4.5 (no actions, no change). Otherwise the resulting phase
/// class and the action classes, in order.
fn expected(p: P, i: I) -> Option<(P, &'static [A])> {
    use A::*;
    use P as Ph;
    let open = !matches!(p, Ph::Idle | Ph::Closed);
    if p == Ph::Closed {
        return Some((Ph::Closed, &[])); // "Closed | anything | no-op"
    }
    Some(match i {
        I::Join => match p {
            Ph::Idle => (Ph::Joining, &[Mute, Notify]),
            _ => return None,
        },
        I::Move => match p {
            Ph::Connected => (Ph::JoiningMoved, &[LinkDown, Notify]),
            Ph::Joining => (Ph::Joining, &[]),
            Ph::JoiningMoved => (Ph::JoiningMoved, &[]),
            Ph::Connecting => (Ph::JoiningMoved, &[Notify]),
            Ph::AwaitingMove | Ph::AwaitingRejoin | Ph::Rejoining => (p, &[]),
            _ => return None,
        },
        I::BeginRejoin => match p {
            Ph::Joining | Ph::JoiningMoved | Ph::Connecting | Ph::Connected => {
                (Ph::Rejoining, &[Notify])
            }
            Ph::AwaitingMove | Ph::AwaitingRejoin | Ph::Rejoining => (Ph::Rejoining, &[]),
            _ => return None,
        },
        I::Leave => (p, &[]),
        I::SetFlags => match open {
            true => (p, &[Mute]),
            false => return None,
        },
        I::StateNone => match p {
            Ph::Rejoining => (p, &[]),
            Ph::Idle => return None,
            _ => (Ph::Closed, &[Teardown]),
        },
        I::StateSame => match p {
            Ph::Joining => (Ph::Joining, &[]),
            Ph::JoiningMoved => (Ph::JoiningMoved, &[Ctrl]),
            Ph::Connecting | Ph::Connected | Ph::AwaitingMove | Ph::Rejoining => (p, &[]),
            _ => return None,
        },
        I::StateNewSession => match p {
            Ph::Joining => (Ph::Joining, &[]),
            Ph::JoiningMoved => (Ph::JoiningMoved, &[Ctrl]),
            Ph::Connecting => (Ph::Connecting, &[Connect]),
            Ph::Connected | Ph::AwaitingMove => (Ph::Connecting, &[Connect, Notify]),
            Ph::Rejoining => (Ph::Rejoining, &[]),
            _ => return None,
        },
        I::StateOtherChannel => match p {
            Ph::Joining => (Ph::Joining, &[Ctrl]),
            Ph::JoiningMoved => (Ph::JoiningMoved, &[Ctrl]),
            Ph::Connecting | Ph::Connected => (Ph::JoiningMoved, &[Ctrl, LinkDown, Notify]),
            Ph::AwaitingMove => (Ph::JoiningMoved, &[Ctrl, Notify]),
            Ph::Rejoining => (Ph::Rejoining, &[Ctrl]),
            _ => return None,
        },
        I::ServerNone => match p {
            Ph::Connecting | Ph::Connected => (Ph::Joining, &[LinkDown, Notify]),
            // A reallocation announcement disarms the move fallback.
            Ph::JoiningMoved => (Ph::Joining, &[]),
            _ if open => (p, &[]),
            _ => return None,
        },
        I::ServerSame => match p {
            Ph::Joining => (Ph::Joining, &[]),
            Ph::JoiningMoved | Ph::Rejoining => (Ph::Connecting, &[Connect, Notify]),
            Ph::AwaitingMove => (Ph::AwaitingMove, &[]),
            _ => return None, // Connecting/Connected: unchanged server is not a listed row
        },
        I::ServerChanged => match p {
            Ph::Joining => (Ph::Joining, &[]),
            Ph::JoiningMoved | Ph::Rejoining | Ph::Connected => {
                (Ph::Connecting, &[Connect, Notify])
            }
            Ph::Connecting => (Ph::Connecting, &[Connect]),
            Ph::AwaitingMove => (Ph::AwaitingMove, &[]),
            _ => return None,
        },
        I::ResultOk => match p {
            Ph::Connecting => (Ph::Connected, &[Emit, LinkUp, Notify]),
            _ => return None,
        },
        I::ResultErr => match p {
            Ph::Connecting => (Ph::AwaitingRejoin, &[Notify]),
            _ => return None,
        },
        I::ResultDiscarded | I::ResultStale => match p {
            Ph::Connecting => (Ph::Connecting, &[]),
            _ => return None,
        },
        I::DrvConnect => match p {
            Ph::Connected => (p, &[LinkUp]),
            _ if open => (p, &[]),
            _ => return None,
        },
        I::DrvReconnect => match p {
            Ph::Connected => (p, &[Emit, LinkUp]),
            _ if open => (p, &[]),
            _ => return None,
        },
        I::DiscConnectIo => match p {
            Ph::Connecting | Ph::AwaitingMove => (p, &[]),
            _ => return None,
        },
        I::DiscRequested | I::DiscNoReason => match open {
            true => (p, &[]),
            false => return None,
        },
        I::Disc4014 => match p {
            Ph::Connected => (Ph::AwaitingMove, &[LinkDown, Emit, Notify]),
            Ph::AwaitingMove => (p, &[]),
            _ if open => (p, &[LinkDown, Emit]),
            _ => return None,
        },
        I::Disc4006 | I::DiscReconnectIo => match p {
            Ph::Connected => (Ph::AwaitingRejoin, &[LinkDown, Emit, Notify]),
            Ph::AwaitingMove => (p, &[]),
            _ if open => (p, &[LinkDown, Emit]),
            _ => return None,
        },
        I::Disc4017 => match p {
            Ph::Connected => (Ph::Closed, &[Teardown]),
            Ph::AwaitingMove => (p, &[]),
            _ if open => (p, &[LinkDown, Emit]),
            _ => return None,
        },
        I::Stalled => match p {
            Ph::Connected => (Ph::AwaitingRejoin, &[Reinstall, Notify]),
            _ => return None,
        },
        I::HandlersLost => match open {
            true => (p, &[Reinstall]),
            false => return None,
        },
        I::Deadline => match p {
            Ph::JoiningMoved => (Ph::Connecting, &[Connect, Notify]),
            Ph::AwaitingMove => (Ph::AwaitingRejoin, &[]),
            Ph::AwaitingRejoin => (Ph::Rejoining, &[Ctrl]),
            Ph::Rejoining => (Ph::AwaitingRejoin, &[]),
            _ => return None,
        },
        I::Close => (Ph::Closed, &[Teardown]),
    })
}

#[test]
fn every_phase_input_pair() {
    let mut checked = 0;
    for p in ALL_PHASES {
        for i in ALL_INPUTS {
            let mut h = build(p, params(true, 3));
            let before_status = h.g.status();
            let before_unlisted = h.g.unlisted_inputs();
            let inp = input(i, &mut h);
            let acts = h.step(inp);
            match expected(p, i) {
                None => {
                    assert!(
                        acts.is_empty(),
                        "{p:?} × {i:?}: unlisted pair produced {acts:?}"
                    );
                    assert_eq!(
                        h.class(),
                        p,
                        "{p:?} × {i:?}: unlisted pair changed the phase"
                    );
                    assert_eq!(h.g.status(), before_status, "{p:?} × {i:?}");
                    assert_eq!(h.g.unlisted_inputs(), before_unlisted + 1, "{p:?} × {i:?}");
                }
                Some((to, want)) => {
                    assert_eq!(h.class(), to, "{p:?} × {i:?}: actions {acts:?}");
                    assert_eq!(classes(&acts), want, "{p:?} × {i:?}");
                    assert_eq!(h.g.unlisted_inputs(), before_unlisted, "{p:?} × {i:?}");
                }
            }
            checked += 1;
        }
    }
    assert_eq!(checked, ALL_PHASES.len() * ALL_INPUTS.len());
}

// ── row-specific scenarios ───────────────────────────────────────────────────────────────────────

fn connect_info(acts: &[GwAction]) -> (songbird::ConnectionInfo, u64) {
    acts.iter()
        .find_map(|a| match a {
            GwAction::Connect { info, generation } => Some((info.clone(), *generation)),
            _ => None,
        })
        .expect("a Connect action")
}

#[test]
fn join_then_both_update_orders_connect() {
    for server_first in [false, true] {
        let mut h = H::new(params(true, 3));
        let a = h.step(GwInput::Join {
            channel: nz(10),
            self_mute: true,
            self_deaf: true,
        });
        assert!(matches!(a[0], GwAction::SetSendMute(true)));
        assert!(
            h.g.next_deadline().is_none(),
            "no Rust deadline for the initial join"
        );
        let (first, second) = if server_first {
            (vsrv("tok", Some(E1)), vs("sess", Some(10)))
        } else {
            (vs("sess", Some(10)), vsrv("tok", Some(E1)))
        };
        assert!(h.step(first).is_empty());
        let acts = h.step(second);
        let (info, generation) = connect_info(&acts);
        assert_eq!(generation, 1);
        assert_eq!(info.channel_id, ChannelId::from(nz(10)));
        assert_eq!(info.guild_id, GuildId::from(nz(100)));
        assert_eq!(info.user_id, UserId::from(nz(200)));
        assert_eq!(info.session_id, "sess");
        assert_eq!(info.token, "tok");
        assert_eq!(info.endpoint, "e1.discord.media");
        let s = h.g.status();
        assert_eq!(s.state, "connecting");
        assert!(s.self_mute && s.self_deaf);
        let acts = h.step(GwInput::ConnectResult {
            generation: 1,
            result: Ok(()),
        });
        assert!(matches!(
            acts[0],
            GwAction::Emit(EventData::Connected { channel_id: 10, .. })
        ));
        assert!(matches!(
            acts[1],
            GwAction::RxLink {
                up: true,
                ssrc: None
            }
        ));
        // DriverConnect after ConnectResult brings the ssrc.
        let acts = h.step(GwInput::DriverConnect { ssrc: 42 });
        assert!(matches!(
            acts[..],
            [GwAction::RxLink {
                up: true,
                ssrc: Some(42)
            }]
        ));
        let s = h.g.status();
        assert_eq!(
            (s.state, s.channel_id, s.ssrc),
            ("connected", Some(10), Some(42))
        );
    }
}

#[test]
fn driver_connect_before_result_supplies_ssrc() {
    let mut h = build(P::Connecting, params(true, 3));
    assert!(h.step(GwInput::DriverConnect { ssrc: 9 }).is_empty());
    let acts = h.step(GwInput::ConnectResult {
        generation: 1,
        result: Ok(()),
    });
    assert!(matches!(
        acts[0],
        GwAction::Emit(EventData::Connected { ssrc: 9, .. })
    ));
    assert!(matches!(
        acts[1],
        GwAction::RxLink {
            up: true,
            ssrc: Some(9)
        }
    ));
}

#[test]
fn endpoint_none_waits_for_reallocation() {
    let mut h = build(P::Connected, params(true, 3));
    let acts = h.step(vsrv("t1", None));
    assert_eq!(classes(&acts), [A::LinkDown, A::Notify]);
    assert_eq!(h.g.status().state, "joining");
    // Nothing is armed: Discord sends a new update.
    assert!(h.g.next_deadline().is_none());
    let acts = h.step(vsrv("t9", Some("wss://new.discord.media")));
    let (info, generation) = connect_info(&acts);
    assert_eq!(generation, 2);
    assert_eq!(info.endpoint, "new.discord.media");
    assert_eq!(info.channel_id, ChannelId::from(nz(10)));
}

#[test]
fn move_fallback_reuses_previous_server_after_3s() {
    let mut h = build(P::Connected, params(true, 3));
    let acts = h.step(GwInput::Move { channel: nz(20) });
    assert_eq!(classes(&acts), [A::LinkDown, A::Notify]);
    let s = h.g.status();
    assert_eq!(
        (s.state, s.target_channel_id, s.channel_id),
        ("joining", Some(20), Some(10))
    );
    // The echo of our own move: same session, new channel; no server update.
    assert!(h.step(vs("s1", Some(20))).is_empty());
    h.advance(Duration::from_millis(2_900));
    let before = h.g.unlisted_inputs();
    assert!(h.step(GwInput::Deadline).is_empty(), "fallback not due yet");
    assert_eq!(h.g.unlisted_inputs(), before + 1);
    h.advance(Duration::from_millis(200));
    let acts = h.step(GwInput::Deadline);
    let (info, _) = connect_info(&acts);
    assert_eq!(
        info.channel_id,
        ChannelId::from(nz(20)),
        "DAVE needs the new channel id"
    );
    assert_eq!(info.token, "t1");
    assert_eq!(info.endpoint, "e1.discord.media");
    assert!(h.g.next_deadline().is_none());
}

#[test]
fn move_with_server_update_connects_without_fallback() {
    for from in [P::Connected, P::Joining, P::Connecting] {
        let mut h = build(from, params(true, 3));
        h.step(GwInput::Move { channel: nz(20) });
        assert_eq!(h.g.status().state, "joining");
        h.step(vs("s1", Some(20)));
        let acts = h.step(vsrv("t2", Some(E2)));
        let (info, generation) = connect_info(&acts);
        assert_eq!(info.channel_id, ChannelId::from(nz(20)), "{from:?}");
        assert_eq!(info.token, "t2");
        assert!(generation > h.g.generation - 1);
        // A result for an abandoned generation is ignored.
        let stale = GwInput::ConnectResult {
            generation: generation - 1,
            result: Ok(()),
        };
        assert!(h.step(stale).is_empty());
        assert_eq!(h.g.status().state, "connecting");
    }
}

#[test]
fn moves_while_reconnecting_only_retarget() {
    for p in [P::AwaitingMove, P::AwaitingRejoin, P::Rejoining] {
        let mut h = build(p, params(true, 3));
        let phase = h.g.phase();
        assert!(h.step(GwInput::Move { channel: nz(55) }).is_empty());
        assert_eq!(h.g.phase(), phase);
        assert_eq!(h.g.status().target_channel_id, Some(55));
    }
    // The Rejoin control then targets the new channel.
    let mut h = build(P::AwaitingRejoin, params(true, 3));
    h.step(GwInput::Move { channel: nz(55) });
    h.advance(Duration::from_secs(2));
    let acts = h.step(GwInput::Deadline);
    assert!(matches!(
        acts[..],
        [GwAction::Control(Control::Rejoin {
            attempt: 1,
            channel_id: 55
        })]
    ));
}

#[test]
fn old_ws_4014_during_move_or_rejoin_changes_no_phase() {
    for p in [
        P::Joining,
        P::JoiningMoved,
        P::Rejoining,
        P::Connecting,
        P::AwaitingRejoin,
    ] {
        let mut h = build(p, params(true, 3));
        let phase = h.g.phase();
        let acts = h.step(disc(
            DiscKind::Runtime,
            Some(DiscReason::WsClosed(Some(4014))),
        ));
        assert_eq!(h.g.phase(), phase, "{p:?}");
        assert!(matches!(acts[0], GwAction::RxLink { up: false, .. }));
        assert!(matches!(
            acts[1],
            GwAction::Emit(EventData::Disconnected {
                code: Some(4014),
                will_retry: true,
                ..
            })
        ));
    }
}

#[test]
fn server_change_while_connecting_bumps_generation() {
    let mut h = build(P::Connecting, params(true, 3));
    let acts = h.step(vsrv("t2", Some(E2)));
    let (info, generation) = connect_info(&acts);
    assert_eq!(generation, 2);
    assert_eq!(info.token, "t2");
    // The in-flight attempt's result is now stale.
    assert!(
        h.step(GwInput::ConnectResult {
            generation: 1,
            result: Ok(())
        })
        .is_empty()
    );
    assert_eq!(h.g.status().state, "connecting");
    let acts = h.step(GwInput::ConnectResult {
        generation: 2,
        result: Ok(()),
    });
    assert_eq!(classes(&acts), [A::Emit, A::LinkUp, A::Notify]);
}

#[test]
fn a_4014_then_move() {
    let mut h = build(P::AwaitingMove, params(true, 3));
    assert_eq!(h.g.status().state, "reconnecting");
    // The server update of the move arrives first and is kept.
    assert!(h.step(vsrv("t2", Some(E2))).is_empty());
    let acts = h.step(vs("s1", Some(30)));
    assert!(matches!(
        acts[0],
        GwAction::Control(Control::ChannelMoved { channel_id: 30 })
    ));
    let (info, _) = connect_info(&acts);
    assert_eq!(info.channel_id, ChannelId::from(nz(30)));
    assert_eq!(info.token, "t2");
}

#[test]
fn a_4014_then_kick() {
    let mut h = build(P::AwaitingMove, params(true, 3));
    let acts = h.step(vs("s1", None));
    assert!(matches!(
        acts[..],
        [GwAction::Teardown(CloseReason::Kicked)]
    ));
    assert_eq!(h.g.status().close_reason, Some(CloseReason::Kicked));
}

#[test]
fn a_4014_grace_expiry_with_and_without_reconnect() {
    let mut h = build(P::AwaitingMove, params(true, 3));
    h.advance(Duration::from_millis(4_900));
    assert!(h.step(GwInput::Deadline).is_empty());
    assert!(matches!(h.g.phase(), Phase::AwaitingMove { .. }));
    h.advance(Duration::from_millis(100));
    assert!(h.step(GwInput::Deadline).is_empty());
    assert!(matches!(h.g.phase(), Phase::AwaitingRejoin { attempt: 1, due } if due == h.now));
    let acts = h.step(GwInput::Deadline);
    assert!(matches!(
        acts[..],
        [GwAction::Control(Control::Rejoin {
            attempt: 1,
            channel_id: 10
        })]
    ));

    let mut h = build(P::AwaitingMove, params(false, 3));
    h.advance(Duration::from_secs(5));
    let acts = h.step(GwInput::Deadline);
    assert!(matches!(acts[..], [GwAction::Teardown(CloseReason::Lost)]));
    let s = h.g.status();
    assert_eq!((s.state, s.close_code), ("closed", Some(4014)));
}

#[test]
fn a_4006_rejoins_up_to_the_limit() {
    let mut h = build(P::Connected, params(true, 3));
    let acts = h.step(disc(
        DiscKind::Runtime,
        Some(DiscReason::WsClosed(Some(4006))),
    ));
    assert_eq!(classes(&acts), [A::LinkDown, A::Emit, A::Notify]);
    for attempt in 1..=3u8 {
        let Phase::AwaitingRejoin { attempt: a, due } = h.g.phase() else {
            panic!("expected AwaitingRejoin, got {:?}", h.g.phase());
        };
        assert_eq!(a, attempt);
        let backoff = due - h.now;
        let base = Duration::from_secs(1 << (attempt - 1));
        assert!(
            backoff >= base.mul_f64(0.8) && backoff <= base.mul_f64(1.2),
            "attempt {attempt}: {backoff:?}"
        );
        h.now = due;
        let acts = h.step(GwInput::Deadline);
        assert!(matches!(
            acts[..],
            [GwAction::Control(Control::Rejoin { attempt: a, .. })] if a == u32::from(attempt)
        ));
        assert_eq!(h.g.status().rejoin_attempt, attempt);
        // The leave half of the rejoin is ignored; a VoiceState alone issues no Connect.
        assert!(h.step(vs("s1", None)).is_empty());
        assert!(h.step(vs("s1", Some(10))).is_empty());
        h.advance(Duration::from_secs(15));
        let acts = h.step(GwInput::Deadline);
        if attempt < 3 {
            assert!(acts.is_empty());
        } else {
            assert!(matches!(
                acts[..],
                [GwAction::Teardown(CloseReason::RejoinFailed)]
            ));
        }
    }
    assert_eq!(h.g.status().close_reason, Some(CloseReason::RejoinFailed));
}

#[test]
fn a_4016_or_4017_closes_without_rejoin() {
    for code in [4016u16, 4017] {
        let mut h = build(P::Connected, params(true, 3));
        let acts = h.step(disc(
            DiscKind::Runtime,
            Some(DiscReason::WsClosed(Some(code))),
        ));
        assert!(matches!(
            acts[..],
            [GwAction::Teardown(CloseReason::SessionInvalid(Some(c)))] if c == code
        ));
        let s = h.g.status();
        assert_eq!(s.close_code, Some(code));
        assert_eq!(s.close_reason.map(|r| r.as_str()), Some("session_invalid"));
    }
}

#[test]
fn reconnect_kind_disconnect_while_connected() {
    let mut h = build(P::Connected, params(true, 3));
    h.step(disc(DiscKind::Reconnect, Some(DiscReason::TimedOut)));
    assert!(matches!(
        h.g.phase(),
        Phase::AwaitingRejoin { attempt: 1, .. }
    ));
    let mut h = build(P::Connected, params(false, 3));
    let acts = h.step(disc(DiscKind::Reconnect, Some(DiscReason::TimedOut)));
    assert!(matches!(acts[..], [GwAction::Teardown(CloseReason::Lost)]));
    // rejoin_attempts = 0 disables gateway rejoins.
    let mut h = build(P::Connected, params(true, 0));
    let acts = h.step(disc(DiscKind::Runtime, Some(DiscReason::Io)));
    assert!(matches!(acts[..], [GwAction::Teardown(CloseReason::Lost)]));
}

#[test]
fn connect_failure_rejoins_or_closes() {
    let mut h = build(P::Connecting, params(false, 3));
    let acts = h.step(GwInput::ConnectResult {
        generation: 1,
        result: Err(ConnectFailure::TimedOut),
    });
    assert!(matches!(
        acts[..],
        [GwAction::Teardown(CloseReason::ConnectFailed)]
    ));

    let mut h = build(P::Connecting, params(true, 1));
    h.step(GwInput::ConnectResult {
        generation: 1,
        result: Err(ConnectFailure::TimedOut),
    });
    let Phase::AwaitingRejoin { due, .. } = h.g.phase() else {
        panic!()
    };
    h.now = due;
    h.step(GwInput::Deadline);
    h.step(vs("s1", Some(10)));
    let acts = h.step(vsrv("t3", Some(E2)));
    let (_, generation) = connect_info(&acts);
    assert_eq!(
        h.g.status().rejoin_attempt,
        1,
        "the attempt is carried into Connecting"
    );
    // The second failure exceeds rejoin_attempts = 1.
    let acts = h.step(GwInput::ConnectResult {
        generation,
        result: Err(ConnectFailure::Other("x".into())),
    });
    assert!(matches!(
        acts[..],
        [GwAction::Teardown(CloseReason::ConnectFailed)]
    ));
}

#[test]
fn begin_rejoin_clears_server_and_requires_a_fresh_one() {
    let mut h = build(P::Connected, params(true, 3));
    h.step(GwInput::BeginRejoin);
    assert!(matches!(h.g.phase(), Phase::Rejoining { attempt: 0, .. }));
    assert!(h.g.server.is_none() && h.g.prev_server.is_none());
    assert!(h.step(vs("s1", None)).is_empty(), "leave echo ignored");
    assert!(
        h.step(vs("s5", Some(10))).is_empty(),
        "no Connect with a revoked token"
    );
    let acts = h.step(vsrv("fresh", Some(E1)));
    let (info, _) = connect_info(&acts);
    assert_eq!(
        (info.token.as_str(), info.session_id.as_str()),
        ("fresh", "s5")
    );
    h.step(GwInput::ConnectResult {
        generation: h.g.generation,
        result: Ok(()),
    });
    let s = h.g.status();
    assert_eq!(
        (s.state, s.generation, s.rejoin_attempt),
        ("connected", 2, 0)
    );
}

#[test]
fn rejoining_into_another_channel_waits_for_the_fresh_server() {
    let mut h = build(P::Rejoining, params(true, 3));
    let acts = h.step(vs("s7", Some(30)));
    assert!(matches!(
        acts[..],
        [GwAction::Control(Control::ChannelMoved { channel_id: 30 })]
    ));
    let acts = h.step(vsrv("fresh", Some(E2)));
    let (info, _) = connect_info(&acts);
    assert_eq!(info.channel_id, ChannelId::from(nz(30)));
    assert_eq!(
        (info.token.as_str(), info.session_id.as_str()),
        ("fresh", "s7")
    );
    // The other order: the session is still known, so the fresh server connects at once.
    let mut h = build(P::Rejoining, params(true, 3));
    let acts = h.step(vsrv("fresh", Some(E2)));
    let (info, _) = connect_info(&acts);
    assert_eq!(info.channel_id, ChannelId::from(nz(10)));
}

#[test]
fn reallocation_disarms_the_move_fallback() {
    let mut h = build(P::JoiningMoved, params(true, 3));
    assert!(h.step(vsrv("t1", None)).is_empty());
    assert!(h.g.next_deadline().is_none());
    h.advance(Duration::from_secs(10));
    assert!(h.step(GwInput::Deadline).is_empty());
    assert_eq!(h.g.status().state, "joining");
}

#[test]
fn rejoin_deadline_without_reconnect_fails() {
    let mut h = build(P::Rejoining, params(false, 3));
    h.advance(Duration::from_secs(15));
    let acts = h.step(GwInput::Deadline);
    assert!(matches!(
        acts[..],
        [GwAction::Teardown(CloseReason::RejoinFailed)]
    ));
}

#[test]
fn set_flags_in_every_open_phase() {
    for p in [
        P::Joining,
        P::JoiningMoved,
        P::Connecting,
        P::Connected,
        P::AwaitingMove,
        P::AwaitingRejoin,
        P::Rejoining,
    ] {
        let mut h = build(p, params(true, 3));
        let phase = h.g.phase();
        let acts = h.step(GwInput::SetFlags {
            self_mute: true,
            self_deaf: true,
        });
        assert!(matches!(acts[..], [GwAction::SetSendMute(true)]), "{p:?}");
        assert_eq!(h.g.phase(), phase);
        let s = h.g.status();
        assert!(s.self_mute && s.self_deaf);
        h.step(GwInput::SetFlags {
            self_mute: false,
            self_deaf: true,
        });
        assert!(!h.g.status().self_mute);
    }
}

#[test]
fn leave_then_voice_state_none_is_requested() {
    for p in [
        P::Joining,
        P::Connecting,
        P::Connected,
        P::AwaitingRejoin,
        P::AwaitingMove,
    ] {
        let mut h = build(p, params(true, 3));
        assert!(h.step(GwInput::Leave).is_empty());
        let acts = h.step(vs("s1", None));
        assert!(
            matches!(acts[..], [GwAction::Teardown(CloseReason::Requested)]),
            "{p:?}: {acts:?}"
        );
    }
    let mut h = build(P::Connected, params(true, 3));
    let acts = h.step(vs("s1", None));
    assert!(matches!(
        acts[..],
        [GwAction::Teardown(CloseReason::LeftChannel)]
    ));
}

#[test]
fn rx_stalled_reinstall_is_rate_limited_and_rejoins_at_5s() {
    let mut h = build(P::Connected, params(true, 3));
    let since = h.now;
    h.advance(Duration::from_secs(2));
    assert_eq!(
        classes(&h.step(GwInput::RxStalled { since })),
        [A::Reinstall]
    );
    h.advance(Duration::from_secs(1));
    assert!(
        h.step(GwInput::RxStalled { since }).is_empty(),
        "rate-limited"
    );
    assert!(
        h.step(GwInput::HandlersLost).is_empty(),
        "shared rate limit"
    );
    h.advance(Duration::from_millis(1_999));
    assert!(h.step(GwInput::RxStalled { since }).is_empty());
    assert_eq!(h.g.status().state, "connected");
    h.advance(Duration::from_millis(1));
    // Stalled for 5 s: rejoin (the reinstall is still rate-limited: the last was 3 s ago).
    let acts = h.step(GwInput::RxStalled { since });
    assert_eq!(classes(&acts), [A::Notify]);
    assert!(matches!(h.g.phase(), Phase::AwaitingRejoin { attempt: 1, due } if due == h.now));

    // auto_recover off: reinstall only.
    let mut prm = params(true, 3);
    prm.auto_recover = false;
    let mut h = build(P::Connected, prm);
    let acts = h.step(GwInput::RxStalled {
        since: h.now - Duration::from_secs(60),
    });
    assert_eq!(classes(&acts), [A::Reinstall]);
    assert_eq!(h.g.status().state, "connected");
}

#[test]
fn reconnected_reports_the_gap() {
    let mut h = build(P::Connected, params(true, 3));
    // A 4014 during a move to the same channel, then a fresh session reconnects.
    h.step(disc(
        DiscKind::Runtime,
        Some(DiscReason::WsClosed(Some(4014))),
    ));
    h.advance(Duration::from_millis(700));
    h.step(vs("s9", Some(10)));
    h.step(GwInput::ConnectResult {
        generation: h.g.generation,
        result: Ok(()),
    });
    // Internal resume while connected: gap measured since the last link-down (none).
    let acts = h.step(GwInput::DriverReconnect { ssrc: 3 });
    assert!(matches!(
        acts[0],
        GwAction::Emit(EventData::Reconnected {
            channel_id: 10,
            ssrc: 3,
            gap_ms: 0
        })
    ));
}

#[test]
fn close_from_every_phase_and_closed_is_terminal() {
    for p in ALL_PHASES {
        let mut h = build(p, params(true, 3));
        let acts = h.step(GwInput::Close(CloseReason::Dropped));
        if p == P::Closed {
            assert!(acts.is_empty());
            assert_eq!(h.g.status().close_reason, Some(CloseReason::Requested));
        } else {
            assert!(matches!(
                acts[..],
                [GwAction::Teardown(CloseReason::Dropped)]
            ));
        }
        for i in ALL_INPUTS {
            let inp = input(i, &mut h);
            assert!(h.step(inp).is_empty(), "{p:?} closed × {i:?}");
            assert_eq!(h.class(), P::Closed);
        }
    }
}

#[test]
fn force_close_keeps_the_first_reason() {
    let mut h = build(P::Connected, params(true, 3));
    assert_eq!(h.g.force_close(CloseReason::Kicked), CloseReason::Kicked);
    assert_eq!(h.g.force_close(CloseReason::Dropped), CloseReason::Kicked);
    assert_eq!(h.g.status().state, "closed");
}

#[test]
fn action_seq_is_monotonic() {
    let mut g = GatewayState::new(params(true, 3));
    let mut last = 0;
    for _ in 0..1000 {
        let s = g.next_action_seq();
        assert!(s > last);
        last = s;
    }
}

#[test]
fn deadlines_are_reported() {
    let h = build(P::JoiningMoved, params(true, 3));
    assert_eq!(h.g.next_deadline(), Some(h.now + Duration::from_secs(3)));
    let h = build(P::AwaitingMove, params(true, 3));
    assert_eq!(h.g.next_deadline(), Some(h.now + Duration::from_secs(5)));
    let h = build(P::Rejoining, params(true, 3));
    assert_eq!(h.g.next_deadline(), Some(h.now + Duration::from_secs(15)));
    for p in [P::Idle, P::Joining, P::Connecting, P::Connected, P::Closed] {
        assert!(
            build(p, params(true, 3)).g.next_deadline().is_none(),
            "{p:?}"
        );
    }
}

#[test]
fn disc_reason_mapping() {
    use songbird::events::context_data::{DisconnectKind as K, DisconnectReason as R};
    use songbird::model::CloseCode;
    assert_eq!(
        DiscReason::from_songbird(R::WsClosed(Some(CloseCode::Disconnected))),
        DiscReason::WsClosed(Some(4014))
    );
    assert_eq!(
        DiscReason::from_songbird(R::WsClosed(None)),
        DiscReason::WsClosed(None)
    );
    assert_eq!(
        DiscReason::from_songbird(R::Requested),
        DiscReason::Requested
    );
    assert_eq!(DiscKind::from_songbird(K::Reconnect), DiscKind::Reconnect);
    assert_eq!(DiscReason::WsClosed(Some(4006)).as_str(), "ws_closed");
}

#[test]
fn endpoint_normalisation() {
    assert_eq!(
        normalize_endpoint("wss://c-ams.discord.media/").as_deref(),
        Some("c-ams.discord.media")
    );
    assert_eq!(
        normalize_endpoint("c-ams.discord.media:443").as_deref(),
        Some("c-ams.discord.media:443")
    );
    assert_eq!(normalize_endpoint("wss:///"), None);
    assert_eq!(normalize_endpoint(""), None);
}

#[test]
fn new_state_is_idle() {
    let g = GatewayState::new(params(true, 3));
    let s = g.status();
    assert_eq!(s.state, "idle");
    assert_eq!(s.generation, 0);
    assert!(g.next_deadline().is_none());
}
