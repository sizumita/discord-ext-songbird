//! The connection state machine (W2, §4.5): pure, table-tested, no I/O.
//!
//! `ConnShared::apply` runs [`GatewayState::step`] under L2, enqueues the resulting actions in step
//! order (L2 → L2q) and executes them after unlocking (§4.6).
//!
//! Deviations from the letter of the §4.5 table (see `docs/deviations/W2.md`):
//! * every transition into `Closed(r)` yields exactly `[Teardown(r)]`; the `Emit(Disconnected
//!   {will_retry: false})` and `Control(Closed)` of the table are performed by teardown T6/T8, so the
//!   final event is emitted once and is always last;
//! * a step that changes the public `state` string appends `NotifyStatus` (unless it closes);
//! * `Connected` → `AwaitingRejoin` on a driver disconnect also emits `RxLink{up: false}` and
//!   `Disconnected{will_retry: true}` (like the 4014 row);
//! * `AwaitingMove` stores a `VoiceServer` that arrives before the move's `VoiceState`;
//! * `AwaitingMove` + `VoiceState(None)` closes with `Requested` after `request_leave()`;
//! * `rejoin_attempts = 0` disables gateway rejoins everywhere (a rejoin is scheduled only while
//!   `attempt + 1 ≤ rejoin_attempts`);
//! * a `Move` arms the 3 s server fallback only once the gateway confirms it, and an unconfirmed move
//!   returns to the previous channel (§4.5 "Move confirmation");
//! * 4022 waits like 4014 and reconnects on a fresh server; 4021 closes; `AwaitingRejoin` merges
//!   gateway updates; a `VoiceServer(None)` after a connection arms a reallocation deadline; the late
//!   leave echo of a rejoin is not a kick.

use std::num::NonZeroU64;
use std::time::{Duration, Instant};

use smallvec::SmallVec;
use songbird::id::{ChannelId, GuildId, UserId};

use crate::ctrl::Control;
use crate::error::CloseReason;
use crate::events::EventData;

/// Internal phases (§4.5). `Closed` is terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Joining,
    Connecting {
        generation: u64,
    },
    Connected,
    /// After voice close code `code` (4014 or 4022): the gateway update decides what happened.
    AwaitingMove {
        deadline: Instant,
        code: u16,
    },
    AwaitingRejoin {
        attempt: u8,
        due: Instant,
    },
    Rejoining {
        attempt: u8,
        deadline: Instant,
    },
    Closed(CloseReason),
}

impl Phase {
    /// The public `ConnectionStatus.state` of this phase.
    pub fn state_name(&self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Joining => "joining",
            Phase::Connecting { .. } => "connecting",
            Phase::Connected => "connected",
            Phase::AwaitingMove { .. } | Phase::AwaitingRejoin { .. } | Phase::Rejoining { .. } => {
                "reconnecting"
            }
            Phase::Closed(_) => "closed",
        }
    }

    /// Every phase except `Idle` and `Closed`.
    pub fn is_open(&self) -> bool {
        !matches!(self, Phase::Idle | Phase::Closed(_))
    }
}

/// songbird `DisconnectKind`, mirrored so the state machine stays testable without songbird events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscKind {
    Connect,
    Reconnect,
    Runtime,
}

/// songbird `DisconnectReason`, mirrored; `WsClosed` carries the raw voice close code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscReason {
    AttemptDiscarded,
    Internal,
    Io,
    ProtocolViolation,
    TimedOut,
    Requested,
    WsClosed(Option<u16>),
    Other,
}

impl DiscReason {
    /// The `VoiceEvent.Disconnected.reason` string.
    pub const fn as_str(self) -> &'static str {
        match self {
            DiscReason::AttemptDiscarded => "attempt_discarded",
            DiscReason::Internal => "internal",
            DiscReason::Io => "io",
            DiscReason::ProtocolViolation => "protocol_violation",
            DiscReason::TimedOut => "timed_out",
            DiscReason::Requested => "requested",
            DiscReason::WsClosed(_) => "ws_closed",
            DiscReason::Other => "other",
        }
    }

    /// The voice close code, if any.
    pub const fn code(self) -> Option<u16> {
        match self {
            DiscReason::WsClosed(c) => c,
            _ => None,
        }
    }

    /// Maps songbird's reason (the close code is the enum discriminant).
    pub fn from_songbird(r: songbird::events::context_data::DisconnectReason) -> Self {
        use songbird::events::context_data::DisconnectReason as R;
        match r {
            R::AttemptDiscarded => DiscReason::AttemptDiscarded,
            R::Internal => DiscReason::Internal,
            R::Io => DiscReason::Io,
            R::ProtocolViolation => DiscReason::ProtocolViolation,
            R::TimedOut => DiscReason::TimedOut,
            R::Requested => DiscReason::Requested,
            R::WsClosed(code) => DiscReason::WsClosed(code.map(|c| c as u16)),
            _ => DiscReason::Other,
        }
    }
}

impl DiscKind {
    /// Maps songbird's kind.
    pub fn from_songbird(k: songbird::events::context_data::DisconnectKind) -> Self {
        use songbird::events::context_data::DisconnectKind as K;
        match k {
            K::Connect => DiscKind::Connect,
            K::Reconnect => DiscKind::Reconnect,
            _ => DiscKind::Runtime,
        }
    }
}

/// Why a `Connect` future failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectFailure {
    AttemptDiscarded,
    TimedOut,
    Other(Box<str>),
}

/// Inputs of the state machine (§4.5).
#[derive(Clone, Debug)]
pub enum GwInput {
    Join {
        channel: NonZeroU64,
        self_mute: bool,
        self_deaf: bool,
    },
    Move {
        channel: NonZeroU64,
    },
    BeginRejoin,
    Leave,
    SetFlags {
        self_mute: bool,
        self_deaf: bool,
    },
    VoiceState {
        session_id: Box<str>,
        channel: Option<u64>,
    },
    VoiceServer {
        token: Box<str>,
        endpoint: Option<Box<str>>,
    },
    ConnectResult {
        generation: u64,
        result: Result<(), ConnectFailure>,
    },
    DriverConnect {
        ssrc: u32,
    },
    DriverReconnect {
        ssrc: u32,
    },
    DriverDisconnect {
        kind: DiscKind,
        reason: Option<DiscReason>,
    },
    RxStalled {
        since: Instant,
    },
    HandlersLost,
    Deadline,
    Close(CloseReason),
}

/// Actions produced by a step, executed in order after L2 is released (§4.6).
#[derive(Clone, Debug)]
pub enum GwAction {
    Connect {
        info: songbird::ConnectionInfo,
        generation: u64,
    },
    SetSendMute(bool),
    /// `Connected.timeline_ms` is filled in by the executor (the state machine has no timeline).
    Emit(EventData),
    Control(Control),
    RxLink {
        up: bool,
        ssrc: Option<u32>,
    },
    ReinstallDispatch,
    Teardown(CloseReason),
    NotifyStatus,
}

/// Fixed parameters of one connection's state machine.
#[derive(Clone, Copy, Debug)]
pub struct GatewayParams {
    pub guild_id: NonZeroU64,
    pub user_id: NonZeroU64,
    pub reconnect: bool,
    pub rejoin_attempts: u8,
    pub auto_recover: bool,
    /// 5 s: the grace after 4014/4022, and how long the gateway has to confirm a `Move`.
    pub move_grace: Duration,
    /// 3 s.
    pub server_fallback: Duration,
    /// 15 s.
    pub rejoin_timeout: Duration,
    /// 5 s.
    pub stall_recover: Duration,
    /// 5 s.
    pub reinstall_interval: Duration,
}

impl GatewayParams {
    /// The documented durations (§4.5) for the given ids and policy.
    pub fn new(
        guild_id: NonZeroU64,
        user_id: NonZeroU64,
        reconnect: bool,
        rejoin_attempts: u8,
        auto_recover: bool,
    ) -> Self {
        GatewayParams {
            guild_id,
            user_id,
            reconnect,
            rejoin_attempts,
            auto_recover,
            move_grace: Duration::from_secs(5),
            server_fallback: Duration::from_secs(3),
            rejoin_timeout: Duration::from_secs(15),
            stall_recover: Duration::from_secs(5),
            reinstall_interval: Duration::from_secs(5),
        }
    }
}

/// A copy of the public status (`native.ConnectionStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub state: &'static str,
    pub channel_id: Option<u64>,
    pub target_channel_id: Option<u64>,
    pub ssrc: Option<u32>,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub rejoin_attempt: u8,
    pub generation: u64,
    pub close_reason: Option<CloseReason>,
    pub close_code: Option<u16>,
}

/// What a `Joining` phase entered from an established connection is waiting for, and until when
/// (§4.5). The initial join has none: the Python `connect(timeout=)` governs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinWait {
    /// A `Move` (our op 4) that the gateway has not confirmed yet. `from`: the channel Discord
    /// still has us in. `revert`: the media connection to `channel` is intact (Connected → Move,
    /// no `Connect` and no driver disconnect since), so an unconfirmed move returns to it.
    Move {
        deadline: Instant,
        from: Option<NonZeroU64>,
        revert: bool,
    },
    /// `VoiceServer(None)`: Discord is reallocating the voice server.
    Server { deadline: Instant },
}

impl JoinWait {
    fn deadline(self) -> Instant {
        match self {
            JoinWait::Move { deadline, .. } | JoinWait::Server { deadline } => deadline,
        }
    }
}

/// The voice server half of the connection info.
#[derive(Clone, PartialEq, Eq)]
struct ServerInfo {
    token: Box<str>,
    endpoint: Box<str>,
}

/// Secrets (§6.3): `GatewayState` is `Debug`, so the token must never be printed.
impl std::fmt::Debug for ServerInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerInfo")
            .field("token", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

/// The state machine. Pure: `step` performs no I/O and never blocks.
#[derive(Clone)]
pub struct GatewayState {
    p: GatewayParams,
    phase: Phase,
    /// Channel being joined or moved to.
    target: Option<NonZeroU64>,
    /// Channel of the current media connection.
    channel: Option<u64>,
    /// Channel of the in-flight `Connect` (Connecting).
    connect_target: Option<NonZeroU64>,
    session: Option<Box<str>>,
    server: Option<ServerInfo>,
    /// The last complete server info (3 s move fallback, §4.5).
    prev_server: Option<ServerInfo>,
    self_mute: bool,
    self_deaf: bool,
    generation: u64,
    attempt: u8,
    leave_requested: bool,
    ssrc: Option<u32>,
    /// Joining after a confirmed move: when the 3 s server fallback fires.
    fallback: Option<Instant>,
    /// Joining entered from an established connection: what it waits for (§4.5).
    join_wait: Option<JoinWait>,
    /// A rejoin's op 4 `None` was sent and its `VoiceState(None)` echo has not arrived yet: a late
    /// echo (after the Rejoining timeout) is not a kick.
    leave_echo_pending: bool,
    last_reinstall: Option<Instant>,
    /// When the media link was last reported down (`Reconnected.gap_ms`).
    link_down_at: Option<Instant>,
    close_code: Option<u16>,
    /// (phase, input) pairs that matched no row (§4.5 default rule).
    unlisted: u64,
    action_seq: u64,
}

/// Secrets (§6.3): the token is never printed and the session id only as a length.
impl std::fmt::Debug for GatewayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayState")
            .field("phase", &self.phase)
            .field("target", &self.target)
            .field("channel", &self.channel)
            .field("connect_target", &self.connect_target)
            .field("session_len", &self.session.as_ref().map(|s| s.len()))
            .field("server", &self.server)
            .field("prev_server", &self.prev_server)
            .field("generation", &self.generation)
            .field("attempt", &self.attempt)
            .field("leave_requested", &self.leave_requested)
            .field("join_wait", &self.join_wait)
            .field("ssrc", &self.ssrc)
            .finish_non_exhaustive()
    }
}

/// SplitMix64 (deterministic jitter; the state machine stays pure).
fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl GatewayState {
    pub fn new(p: GatewayParams) -> Self {
        GatewayState {
            p,
            phase: Phase::Idle,
            target: None,
            channel: None,
            connect_target: None,
            session: None,
            server: None,
            prev_server: None,
            self_mute: false,
            self_deaf: false,
            generation: 0,
            attempt: 0,
            leave_requested: false,
            ssrc: None,
            fallback: None,
            join_wait: None,
            leave_echo_pending: false,
            last_reinstall: None,
            link_down_at: None,
            close_code: None,
            unlisted: 0,
            action_seq: 0,
        }
    }

    /// The current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Number of inputs that matched no row (`stats()["gw_unlisted_inputs"]`).
    pub fn unlisted_inputs(&self) -> u64 {
        self.unlisted
    }

    /// One transition (§4.5 table). Pure: appends actions to `out`, performs no I/O.
    pub fn step(&mut self, input: GwInput, now: Instant, out: &mut SmallVec<[GwAction; 4]>) {
        if matches!(self.phase, Phase::Closed(_)) {
            return; // "Closed | anything | no-op"
        }
        let before = self.phase.state_name();
        let start = out.len();
        let listed = match input {
            GwInput::Join {
                channel,
                self_mute,
                self_deaf,
            } => self.on_join(channel, self_mute, self_deaf, out),
            GwInput::Move { channel } => self.on_move(channel, now, out),
            GwInput::BeginRejoin => self.on_begin_rejoin(now),
            GwInput::Leave => {
                self.leave_requested = true;
                true
            }
            GwInput::SetFlags {
                self_mute,
                self_deaf,
            } => {
                if self.phase.is_open() {
                    self.self_mute = self_mute;
                    self.self_deaf = self_deaf;
                    out.push(GwAction::SetSendMute(self_mute));
                    true
                } else {
                    false
                }
            }
            GwInput::VoiceState {
                session_id,
                channel,
            } => match channel.and_then(NonZeroU64::new) {
                None if channel.is_none() => self.on_voice_state_none(out),
                None => false, // channel 0: not a real channel id
                Some(c) => self.on_voice_state(session_id, c, now, out),
            },
            GwInput::VoiceServer { token, endpoint } => {
                let ep = endpoint.as_deref().and_then(normalize_endpoint);
                match ep {
                    None => self.on_voice_server_none(now, out),
                    Some(ep) => self.on_voice_server(
                        ServerInfo {
                            token,
                            endpoint: ep.into_boxed_str(),
                        },
                        out,
                    ),
                }
            }
            GwInput::ConnectResult { generation, result } => {
                self.on_connect_result(generation, result, now, out)
            }
            GwInput::DriverConnect { ssrc } => self.on_driver_connect(ssrc, false, now, out),
            GwInput::DriverReconnect { ssrc } => self.on_driver_connect(ssrc, true, now, out),
            GwInput::DriverDisconnect { kind, reason } => {
                self.on_driver_disconnect(kind, reason, now, out)
            }
            GwInput::RxStalled { since } => self.on_rx_stalled(since, now, out),
            GwInput::HandlersLost => {
                if self.phase.is_open() {
                    self.reinstall(now, out);
                    true
                } else {
                    false
                }
            }
            GwInput::Deadline => self.on_deadline(now, out),
            GwInput::Close(r) => {
                self.close_to(r, None, out);
                true
            }
        };
        if !listed {
            debug_assert_eq!(out.len(), start, "an unlisted pair produced actions");
            self.unlisted = self.unlisted.saturating_add(1);
            return;
        }
        let after = self.phase.state_name();
        if before != after
            && !matches!(self.phase, Phase::Closed(_))
            && !out
                .get(start..)
                .is_some_and(|s| s.iter().any(|a| matches!(a, GwAction::NotifyStatus)))
        {
            out.push(GwAction::NotifyStatus);
        }
    }

    // ── per-input handlers (return whether the (phase, input) pair is listed) ────────────────────

    fn on_join(
        &mut self,
        channel: NonZeroU64,
        self_mute: bool,
        self_deaf: bool,
        out: &mut SmallVec<[GwAction; 4]>,
    ) -> bool {
        if self.phase != Phase::Idle {
            return false;
        }
        self.phase = Phase::Joining;
        self.target = Some(channel);
        self.self_mute = self_mute;
        self.self_deaf = self_deaf;
        // No Rust deadline: the Python `connect(timeout=)` governs.
        out.push(GwAction::SetSendMute(self_mute));
        true
    }

    fn on_move(
        &mut self,
        channel: NonZeroU64,
        now: Instant,
        out: &mut SmallVec<[GwAction; 4]>,
    ) -> bool {
        match self.phase {
            Phase::Connected | Phase::Joining | Phase::Connecting { .. } => {
                // Where Discord has us until it confirms this move, and whether that media
                // connection is still intact. A newer move supersedes a pending one (keeping where
                // the first one started); a result of an abandoned attempt is stale (the next
                // Connect bumps the generation).
                let (from, revert) = match (self.phase, self.join_wait) {
                    (Phase::Connected, _) => (self.channel.and_then(NonZeroU64::new), true),
                    (Phase::Joining, Some(JoinWait::Move { from, revert, .. })) => (from, revert),
                    (Phase::Connecting { .. }, _) => (self.connect_target, false),
                    _ => (self.target, false),
                };
                if self.phase == Phase::Connected {
                    self.link_down(now, out);
                }
                self.stash_server();
                self.phase = Phase::Joining;
                self.target = Some(channel);
                self.server = None;
                // The 3 s server fallback is armed only once the gateway confirms the move: an op 4
                // that Discord ignores (no CONNECT permission, a full channel) must not "connect"
                // to the new channel id with the old session and token.
                self.fallback = None;
                self.join_wait = Some(JoinWait::Move {
                    deadline: now.checked_add(self.p.move_grace).unwrap_or(now),
                    from,
                    revert,
                });
                true
            }
            Phase::AwaitingMove { .. } | Phase::AwaitingRejoin { .. } | Phase::Rejoining { .. } => {
                self.target = Some(channel);
                true
            }
            Phase::Idle | Phase::Closed(_) => false,
        }
    }

    fn on_begin_rejoin(&mut self, now: Instant) -> bool {
        if !self.phase.is_open() {
            return false;
        }
        self.enter_rejoining(0, now);
        true
    }

    fn on_voice_state_none(&mut self, out: &mut SmallVec<[GwAction; 4]>) -> bool {
        match self.phase {
            // Expected from the leave half of the rejoin.
            Phase::Rejoining { .. } => {
                self.leave_echo_pending = false;
                true
            }
            // The echo of a rejoin's leave that arrived after its Rejoining timeout (the op 4 was
            // delayed, e.g. by discord.py's gateway rate limiter): not a kick.
            Phase::AwaitingRejoin { .. } if self.leave_echo_pending => {
                self.leave_echo_pending = false;
                true
            }
            Phase::AwaitingMove { .. } => {
                let r = if self.leave_requested {
                    CloseReason::Requested
                } else {
                    CloseReason::Kicked
                };
                self.close_to(r, None, out);
                true
            }
            Phase::Joining
            | Phase::Connecting { .. }
            | Phase::Connected
            | Phase::AwaitingRejoin { .. } => {
                let r = if self.leave_requested {
                    CloseReason::Requested
                } else {
                    CloseReason::LeftChannel
                };
                self.close_to(r, None, out);
                true
            }
            Phase::Idle | Phase::Closed(_) => false,
        }
    }

    fn on_voice_state(
        &mut self,
        session: Box<str>,
        c: NonZeroU64,
        now: Instant,
        out: &mut SmallVec<[GwAction; 4]>,
    ) -> bool {
        match self.phase {
            Phase::Joining => {
                self.session = Some(session);
                let pending_move = match self.join_wait {
                    Some(JoinWait::Move { from, .. }) => {
                        if from == Some(c) && self.target != Some(c) {
                            // Still in the old channel (e.g. the echo of an earlier flag op 4): not
                            // the confirmation of our move; the move deadline decides.
                            return true;
                        }
                        self.join_wait = None; // confirmed
                        true
                    }
                    _ => false,
                };
                if self.target != Some(c) {
                    self.target = Some(c);
                    // A server update from before the move is not reused (§4.5).
                    self.stash_server();
                    out.push(GwAction::Control(Control::ChannelMoved {
                        channel_id: c.get(),
                    }));
                }
                if !self.maybe_connect(out) && pending_move {
                    // The gateway confirmed the move: fall back to the previous server after 3 s.
                    self.arm_fallback(now);
                }
                true
            }
            Phase::Rejoining { .. } => {
                self.session = Some(session);
                self.leave_echo_pending = false; // the leave was processed before this join
                if self.target != Some(c) {
                    // Every server was received after the leave, so it stays valid.
                    self.target = Some(c);
                    out.push(GwAction::Control(Control::ChannelMoved {
                        channel_id: c.get(),
                    }));
                }
                self.maybe_connect(out);
                true
            }
            Phase::AwaitingRejoin { .. } => {
                // A moderator move (or a late echo of a rejoin's join) while waiting to rejoin:
                // the rejoin must target the channel Discord has us in, not drag us back. A fresh
                // server (received since the drop) connects at once.
                self.session = Some(session);
                self.leave_echo_pending = false;
                if self.target != Some(c) {
                    self.target = Some(c);
                    out.push(GwAction::Control(Control::ChannelMoved {
                        channel_id: c.get(),
                    }));
                }
                self.maybe_connect(out);
                true
            }
            Phase::Connecting { .. } | Phase::Connected => {
                let current = if self.phase == Phase::Connected {
                    self.channel.and_then(NonZeroU64::new)
                } else {
                    self.connect_target
                };
                if current != Some(c) {
                    self.session = Some(session);
                    self.moved_away(c, now, out);
                } else if self.session.as_deref() != Some(&*session) {
                    // Mirrors songbird `info.rs:59-97`: a new session on the same channel.
                    self.session = Some(session);
                    self.begin_connect(out);
                }
                // Equal session: that update was just a mute/deaf flag change.
                true
            }
            Phase::AwaitingMove { .. } => {
                self.leave_echo_pending = false;
                let session_changed = self.session.as_deref() != Some(&*session);
                self.session = Some(session);
                if self.channel != Some(c.get()) {
                    // A move: a fresh server (received while awaiting) connects at once;
                    // otherwise wait for one, with the 3 s fallback to the previous server.
                    self.phase = Phase::Joining;
                    self.target = Some(c);
                    out.push(GwAction::Control(Control::ChannelMoved {
                        channel_id: c.get(),
                    }));
                    if !self.maybe_connect(out) {
                        self.arm_fallback(now);
                    }
                } else if session_changed {
                    self.target = Some(c);
                    if self.server.is_none() {
                        self.server = self.prev_server.clone();
                    }
                    self.begin_connect(out);
                }
                true
            }
            Phase::Idle | Phase::Closed(_) => false,
        }
    }

    fn on_voice_server_none(&mut self, now: Instant, out: &mut SmallVec<[GwAction; 4]>) -> bool {
        if !self.phase.is_open() {
            return false;
        }
        // Discord sends a new update once it reallocates the server; the move fallback (reusing
        // the previous server) no longer applies, and neither does reverting a move to it.
        self.server = None;
        self.prev_server = None;
        self.fallback = None;
        // After an established connection nothing else bounds the wait (the Python connect timeout
        // only covers the initial join): if the new server never arrives, rejoin (or close).
        let established = self.channel.is_some();
        let realloc = JoinWait::Server {
            deadline: now.checked_add(self.p.rejoin_timeout).unwrap_or(now),
        };
        match self.phase {
            Phase::Connected | Phase::Connecting { .. } => {
                if self.phase == Phase::Connected
                    && let Some(ch) = self.channel.and_then(NonZeroU64::new)
                {
                    self.target = Some(ch);
                } else if let Some(ct) = self.connect_target {
                    self.target = Some(ct);
                }
                self.phase = Phase::Joining;
                self.link_down(now, out);
                if established {
                    self.join_wait = Some(realloc);
                    out.push(GwAction::Emit(EventData::Disconnected {
                        reason: "server_reallocating",
                        code: None,
                        will_retry: true,
                    }));
                }
            }
            Phase::Joining => match self.join_wait {
                Some(JoinWait::Move { deadline, from, .. }) => {
                    self.join_wait = Some(JoinWait::Move {
                        deadline,
                        from,
                        revert: false,
                    });
                }
                Some(JoinWait::Server { .. }) => {} // keep the earlier deadline
                None if established => self.join_wait = Some(realloc),
                None => {}
            },
            _ => {}
        }
        true
    }

    fn on_voice_server(&mut self, srv: ServerInfo, out: &mut SmallVec<[GwAction; 4]>) -> bool {
        match self.phase {
            Phase::Joining | Phase::Rejoining { .. } => {
                // In Rejoining every server is fresh: entering it cleared `server` (§4.5). In
                // Joining a server update ends what it waited for: Discord sends one only in
                // reply to a voice state change, so it also confirms a pending move.
                self.server = Some(srv);
                self.fallback = None;
                self.join_wait = None;
                self.maybe_connect(out);
                true
            }
            Phase::Connecting { .. } | Phase::Connected => {
                if self.server.as_ref() == Some(&srv) {
                    return false; // unchanged: not a listed row
                }
                self.server = Some(srv);
                if let Some(ch) = self.current_channel() {
                    self.target = Some(ch);
                }
                self.begin_connect(out);
                true
            }
            Phase::AwaitingMove { code, .. } => {
                // Stored for the move that the next VoiceState announces (no actions)...
                self.server = Some(srv);
                // ...except after 4022 ("call terminated", e.g. the voice server changed): a
                // server-only change sends no VoiceState, so reconnect at once with the known
                // session in the current channel (discord.py does the same).
                if code == 4022
                    && self.session.is_some()
                    && let Some(ch) = self.channel.and_then(NonZeroU64::new)
                {
                    self.target = Some(ch);
                    self.begin_connect(out);
                }
                true
            }
            Phase::AwaitingRejoin { .. } => {
                // Entering AwaitingRejoin stashed the old server, so this one is fresh (e.g. the
                // server change that broke the connection): connect at once instead of an op 4
                // leave + join after the backoff.
                self.server = Some(srv);
                self.maybe_connect(out);
                true
            }
            Phase::Idle | Phase::Closed(_) => false,
        }
    }

    fn on_connect_result(
        &mut self,
        g: u64,
        result: Result<(), ConnectFailure>,
        now: Instant,
        out: &mut SmallVec<[GwAction; 4]>,
    ) -> bool {
        let Phase::Connecting { generation } = self.phase else {
            return false;
        };
        if g != generation {
            return true; // superseded
        }
        match result {
            Err(ConnectFailure::AttemptDiscarded) => {}
            Ok(()) => {
                self.phase = Phase::Connected;
                self.channel = self.connect_target.map(NonZeroU64::get);
                self.target = self.connect_target;
                self.attempt = 0;
                self.fallback = None;
                self.leave_echo_pending = false;
                let channel_id = self.channel.unwrap_or(0);
                out.push(GwAction::Emit(EventData::Connected {
                    channel_id,
                    ssrc: self.ssrc.unwrap_or(0),
                    timeline_ms: 0,
                }));
                self.link_down_at = None;
                out.push(GwAction::RxLink {
                    up: true,
                    ssrc: self.ssrc,
                });
                out.push(GwAction::NotifyStatus);
            }
            Err(_) => {
                let next = self.attempt.saturating_add(1);
                let due = now.checked_add(self.backoff(next)).unwrap_or(now);
                if !self.schedule_rejoin(next, due) {
                    self.close_to(CloseReason::ConnectFailed, None, out);
                }
            }
        }
        true
    }

    fn on_driver_connect(
        &mut self,
        ssrc: u32,
        reconnect: bool,
        now: Instant,
        out: &mut SmallVec<[GwAction; 4]>,
    ) -> bool {
        if !self.phase.is_open() {
            return false;
        }
        self.ssrc = Some(ssrc);
        if self.phase == Phase::Connected {
            let gap_ms = self
                .link_down_at
                .take()
                .map_or(0, |t| now.saturating_duration_since(t).as_millis() as u64);
            if reconnect {
                out.push(GwAction::Emit(EventData::Reconnected {
                    channel_id: self.channel.unwrap_or(0),
                    ssrc,
                    gap_ms,
                }));
            }
            out.push(GwAction::RxLink {
                up: true,
                ssrc: Some(ssrc),
            });
        }
        // Other phases: the event comes from an attempt that is being replaced, or Connecting
        // reaches Connected only through its own ConnectResult.
        true
    }

    fn on_driver_disconnect(
        &mut self,
        kind: DiscKind,
        reason: Option<DiscReason>,
        now: Instant,
        out: &mut SmallVec<[GwAction; 4]>,
    ) -> bool {
        if !self.phase.is_open() {
            return false;
        }
        let reason = match reason {
            // Our own `leave()` (and songbird's "requested in some way").
            None | Some(DiscReason::Requested) => return true,
            Some(r) => r,
        };
        match self.phase {
            Phase::AwaitingMove { .. } => true, // already waiting for the gateway
            Phase::Connecting { .. } if kind == DiscKind::Connect => true, // ConnectResult carries it
            Phase::Connected => match kind {
                DiscKind::Connect => false,
                DiscKind::Runtime => {
                    match reason.code() {
                        Some(code @ (4014 | 4022)) => {
                            // 4014: moved, kicked or channel deleted. 4022: call terminated
                            // (channel deleted, voice server changed). The gateway update decides.
                            let deadline = now.checked_add(self.p.move_grace).unwrap_or(now);
                            self.phase = Phase::AwaitingMove { deadline, code };
                            self.stash_server();
                            self.link_down(now, out);
                            out.push(GwAction::Emit(EventData::Disconnected {
                                reason: reason.as_str(),
                                code: Some(code),
                                will_retry: true,
                            }));
                        }
                        Some(code @ (4016 | 4017)) => {
                            // A rejoin cannot fix a protocol mismatch.
                            self.close_to(CloseReason::SessionInvalid(Some(code)), Some(code), out);
                        }
                        Some(4021) => {
                            // Rate limited: Discord says not to reconnect, so no rejoin either.
                            self.close_to(CloseReason::Lost, Some(4021), out);
                        }
                        _ => self.rejoin_or_lost(reason, now, out),
                    }
                    true
                }
                DiscKind::Reconnect => {
                    self.rejoin_or_lost(reason, now, out);
                    true
                }
            },
            Phase::Joining
            | Phase::Connecting { .. }
            | Phase::AwaitingRejoin { .. }
            | Phase::Rejoining { .. } => {
                if kind == DiscKind::Connect {
                    return false;
                }
                // Typically the old voice WS closing (4014) during a move or rejoin: no phase
                // change; the pending gateway update or attempt decides what happens next. The
                // media connection a pending move could return to is gone.
                if let Some(JoinWait::Move { deadline, from, .. }) = self.join_wait {
                    self.join_wait = Some(JoinWait::Move {
                        deadline,
                        from,
                        revert: false,
                    });
                }
                self.link_down(now, out);
                out.push(GwAction::Emit(EventData::Disconnected {
                    reason: reason.as_str(),
                    code: reason.code(),
                    will_retry: true,
                }));
                true
            }
            Phase::Idle | Phase::Closed(_) => false,
        }
    }

    fn on_rx_stalled(
        &mut self,
        since: Instant,
        now: Instant,
        out: &mut SmallVec<[GwAction; 4]>,
    ) -> bool {
        if self.phase != Phase::Connected {
            return false;
        }
        self.reinstall(now, out);
        if now.saturating_duration_since(since) >= self.p.stall_recover && self.p.auto_recover {
            // The documented DAVE #310 / UDP-rx #315 recovery.
            self.schedule_rejoin(1, now);
        }
        true
    }

    fn on_deadline(&mut self, now: Instant, out: &mut SmallVec<[GwAction; 4]>) -> bool {
        match self.phase {
            Phase::Joining => {
                if let Some(at) = self.fallback
                    && now >= at
                {
                    // Reuse the previous endpoint/token with the new channel id (DAVE needs it).
                    self.fallback = None;
                    if self.session.is_some()
                        && let Some(ps) = self.prev_server.clone()
                    {
                        self.server = Some(ps);
                        self.begin_connect(out);
                    }
                    return true;
                }
                match self.join_wait {
                    Some(w) if now >= w.deadline() => {
                        self.join_wait = None;
                        self.on_join_wait_expired(w, now, out);
                        true
                    }
                    _ => false,
                }
            }
            Phase::AwaitingMove { deadline, code } if now >= deadline => {
                // No state update within the grace period: assume the gateway session dropped.
                if !self.schedule_rejoin(1, now) {
                    self.close_to(CloseReason::Lost, Some(code), out);
                }
                true
            }
            Phase::AwaitingRejoin { attempt, due } if now >= due => {
                self.enter_rejoining(attempt, now);
                let channel_id = self
                    .target
                    .map(NonZeroU64::get)
                    .or(self.channel)
                    .unwrap_or(0);
                out.push(GwAction::Control(Control::Rejoin {
                    attempt: u32::from(attempt),
                    channel_id,
                }));
                true
            }
            Phase::Rejoining { attempt, deadline } if now >= deadline => {
                let next = attempt.saturating_add(1);
                let due = now.checked_add(self.backoff(next)).unwrap_or(now);
                if !self.schedule_rejoin(next, due) {
                    self.close_to(CloseReason::RejoinFailed, None, out);
                }
                true
            }
            _ => false,
        }
    }

    /// The Joining deadline of an established connection expired (§4.5).
    fn on_join_wait_expired(
        &mut self,
        w: JoinWait,
        now: Instant,
        out: &mut SmallVec<[GwAction; 4]>,
    ) {
        if let JoinWait::Move { from, revert, .. } = w {
            if revert
                && let Some(ch) = self.channel.and_then(NonZeroU64::new)
                && let Some(srv) = self.prev_server.take()
            {
                // The gateway never confirmed the move (no CONNECT permission, a full channel, a
                // lost or failed op 4): Discord still has us in `ch`, whose media connection is
                // intact. Return to it; `move_to` sees the target change and fails.
                self.phase = Phase::Connected;
                self.target = Some(ch);
                self.server = Some(srv);
                self.link_down_at = None;
                out.push(GwAction::RxLink {
                    up: true,
                    ssrc: self.ssrc,
                });
                return;
            }
            // Nothing intact to return to: rejoin where Discord still has us.
            if let Some(f) = from {
                self.target = Some(f);
            }
        }
        if !self.schedule_rejoin(1, now) {
            self.close_to(CloseReason::Lost, None, out);
        }
    }

    // ── helpers ──────────────────────────────────────────────────────────────────────────────────

    /// The channel of the current media connection (Connected) or of the in-flight attempt.
    fn current_channel(&self) -> Option<NonZeroU64> {
        match self.phase {
            Phase::Connected => self.channel.and_then(NonZeroU64::new),
            Phase::Connecting { .. } => self.connect_target,
            _ => self.target,
        }
    }

    /// Keeps the current server as the fallback for a move, and forgets it as the active one.
    fn stash_server(&mut self) {
        if let Some(s) = self.server.take() {
            self.prev_server = Some(s);
        }
    }

    fn arm_fallback(&mut self, now: Instant) {
        self.fallback = self
            .prev_server
            .is_some()
            .then(|| now.checked_add(self.p.server_fallback).unwrap_or(now));
    }

    /// Joining after a gateway-confirmed move (the 3 s server fallback is armed).
    fn enter_joining(&mut self, target: NonZeroU64, now: Instant) {
        self.phase = Phase::Joining;
        self.target = Some(target);
        self.server = None;
        self.join_wait = None;
        self.arm_fallback(now);
    }

    /// External move away from the current channel (Connected/Connecting rows).
    fn moved_away(&mut self, c: NonZeroU64, now: Instant, out: &mut SmallVec<[GwAction; 4]>) {
        self.stash_server();
        out.push(GwAction::Control(Control::ChannelMoved {
            channel_id: c.get(),
        }));
        self.link_down(now, out);
        self.enter_joining(c, now);
    }

    fn enter_rejoining(&mut self, attempt: u8, now: Instant) {
        let deadline = now.checked_add(self.p.rejoin_timeout).unwrap_or(now);
        self.phase = Phase::Rejoining { attempt, deadline };
        self.attempt = attempt;
        // The op 4 leave revokes the old token: a fresh VoiceServer is required, and the move
        // fallback is never armed here.
        self.server = None;
        self.prev_server = None;
        self.fallback = None;
        self.join_wait = None;
        self.leave_echo_pending = true; // Python sends op 4 `None` first
    }

    /// `AwaitingRejoin{next, due}` iff `reconnect` and `next ≤ rejoin_attempts`.
    fn schedule_rejoin(&mut self, next: u8, due: Instant) -> bool {
        if !self.p.reconnect || next == 0 || next > self.p.rejoin_attempts {
            return false;
        }
        self.phase = Phase::AwaitingRejoin { attempt: next, due };
        self.attempt = next;
        self.fallback = None;
        self.join_wait = None;
        // A server received from now on is fresh and connects at once (AwaitingRejoin rows).
        self.stash_server();
        true
    }

    /// Connected + a non-4014 disconnect: rejoin with backoff, or close as `Lost`.
    fn rejoin_or_lost(
        &mut self,
        reason: DiscReason,
        now: Instant,
        out: &mut SmallVec<[GwAction; 4]>,
    ) {
        let next = self.attempt.saturating_add(1);
        let due = now.checked_add(self.backoff(next)).unwrap_or(now);
        if self.schedule_rejoin(next, due) {
            self.link_down(now, out);
            out.push(GwAction::Emit(EventData::Disconnected {
                reason: reason.as_str(),
                code: reason.code(),
                will_retry: true,
            }));
        } else {
            self.close_to(CloseReason::Lost, reason.code(), out);
        }
    }

    /// Starts a connect if session, server and target are all known.
    fn maybe_connect(&mut self, out: &mut SmallVec<[GwAction; 4]>) -> bool {
        if self.session.is_some() && self.server.is_some() && self.target.is_some() {
            self.begin_connect(out)
        } else {
            false
        }
    }

    /// Connecting{gen+1} with a freshly built `ConnectionInfo` (the attempt counter is carried).
    fn begin_connect(&mut self, out: &mut SmallVec<[GwAction; 4]>) -> bool {
        let (Some(target), Some(session), Some(server)) =
            (self.target, self.session.as_ref(), self.server.as_ref())
        else {
            return false;
        };
        let info = songbird::ConnectionInfo {
            channel_id: ChannelId::from(target),
            endpoint: server.endpoint.to_string(),
            guild_id: GuildId::from(self.p.guild_id),
            session_id: session.to_string(),
            token: server.token.to_string(),
            user_id: UserId::from(self.p.user_id),
        };
        self.generation = self.generation.wrapping_add(1);
        self.phase = Phase::Connecting {
            generation: self.generation,
        };
        self.connect_target = Some(target);
        self.fallback = None;
        self.join_wait = None;
        // The new connection gets a new SSRC (DriverConnect reports it, in either order).
        self.ssrc = None;
        out.push(GwAction::Connect {
            info,
            generation: self.generation,
        });
        true
    }

    fn link_down(&mut self, now: Instant, out: &mut SmallVec<[GwAction; 4]>) {
        if self.link_down_at.is_none() {
            self.link_down_at = Some(now);
        }
        out.push(GwAction::RxLink {
            up: false,
            ssrc: None,
        });
    }

    /// `ReinstallDispatch`, at most once per `reinstall_interval` (shared by HandlersLost and
    /// RxStalled).
    fn reinstall(&mut self, now: Instant, out: &mut SmallVec<[GwAction; 4]>) {
        if self
            .last_reinstall
            .is_none_or(|t| now.saturating_duration_since(t) >= self.p.reinstall_interval)
        {
            self.last_reinstall = Some(now);
            out.push(GwAction::ReinstallDispatch);
        }
    }

    fn close_to(&mut self, r: CloseReason, code: Option<u16>, out: &mut SmallVec<[GwAction; 4]>) {
        self.phase = Phase::Closed(r);
        self.close_code = code.or(r.code());
        self.fallback = None;
        self.join_wait = None;
        out.push(GwAction::Teardown(r));
    }

    /// Rejoin backoff: 1, 2, 4, 8, 16 s (attempt 1, 2, …) with ±20 % deterministic jitter.
    pub fn backoff(&self, attempt: u8) -> Duration {
        let exp = u32::from(attempt.saturating_sub(1).min(4));
        let base_ms: u64 = 1_000u64 << exp;
        let seed = self.p.guild_id.get()
            ^ self.generation.rotate_left(17)
            ^ u64::from(attempt).rotate_left(41);
        // jitter ∈ [-200, 200] per mille.
        let jitter = (mix64(seed) % 401) as i64 - 200;
        let ms = (base_ms as i64).saturating_add(base_ms as i64 * jitter / 1_000);
        Duration::from_millis(ms.max(0) as u64)
    }

    /// T1: sets `Closed(r)` unless already closed; returns the effective close reason.
    pub(crate) fn force_close(&mut self, r: CloseReason) -> CloseReason {
        match self.phase {
            Phase::Closed(existing) => existing,
            _ => {
                self.phase = Phase::Closed(r);
                self.close_code = r.code();
                self.fallback = None;
                r
            }
        }
    }

    /// The earliest armed deadline, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Joining => match (self.fallback, self.join_wait.map(JoinWait::deadline)) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            Phase::AwaitingMove { deadline, .. } => Some(deadline),
            Phase::AwaitingRejoin { due, .. } => Some(due),
            Phase::Rejoining { deadline, .. } => Some(deadline),
            _ => None,
        }
    }

    /// The current public status.
    pub fn status(&self) -> StatusSnapshot {
        let close_reason = match self.phase {
            Phase::Closed(r) => Some(r),
            _ => None,
        };
        StatusSnapshot {
            state: self.phase.state_name(),
            channel_id: self.channel,
            target_channel_id: self.target.map(NonZeroU64::get),
            ssrc: self.ssrc,
            self_mute: self.self_mute,
            self_deaf: self.self_deaf,
            rejoin_attempt: self.attempt,
            generation: self.generation,
            close_reason,
            close_code: if close_reason.is_some() {
                self.close_code
            } else {
                None
            },
        }
    }

    /// Monotonic batch sequence number; called under L2 (§4.6).
    pub fn next_action_seq(&mut self) -> u64 {
        self.action_seq = self.action_seq.wrapping_add(1);
        self.action_seq
    }
}

/// Actions of one step, executed in order by the single executor (§4.6).
#[derive(Debug)]
pub struct ActionBatch {
    pub seq: u64,
    pub actions: SmallVec<[GwAction; 4]>,
}

/// Strips the `wss://` scheme and any trailing `/`; `None` if nothing is left.
pub fn normalize_endpoint(ep: &str) -> Option<String> {
    let ep = ep.trim();
    let ep = ep.strip_prefix("wss://").unwrap_or(ep);
    let ep = ep.trim_end_matches('/');
    (!ep.is_empty()).then(|| ep.to_owned())
}

#[cfg(test)]
mod tests;
