//! The connection state machine (W2, §4.5): pure, table-tested, no I/O.
//!
//! `ConnShared::apply` runs [`GatewayState::step`] under L2, enqueues the resulting actions in step
//! order (L2 → L2q) and executes them after unlocking (§4.6).
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::num::NonZeroU64;
use std::time::{Duration, Instant};

use smallvec::SmallVec;

use crate::ctrl::Control;
use crate::error::CloseReason;
use crate::events::EventData;

/// Internal phases (§4.5). `Closed` is terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Joining,
    Connecting { generation: u64 },
    Connected,
    AwaitingMove { deadline: Instant },
    AwaitingRejoin { attempt: u8, due: Instant },
    Rejoining { attempt: u8, deadline: Instant },
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
    /// 5 s.
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

/// The voice server half of the connection info.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ServerInfo {
    token: Box<str>,
    endpoint: Box<str>,
}

/// The state machine. Pure: `step` performs no I/O and never blocks.
#[derive(Debug)]
pub struct GatewayState {
    p: GatewayParams,
    phase: Phase,
    /// Channel being joined or moved to.
    target: Option<NonZeroU64>,
    /// Channel of the current media connection.
    channel: Option<u64>,
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
    /// Armed timer (move fallback, move grace, rejoin due/deadline).
    deadline: Option<Instant>,
    last_reinstall: Option<Instant>,
    action_seq: u64,
}

impl GatewayState {
    pub fn new(p: GatewayParams) -> Self {
        GatewayState {
            p,
            phase: Phase::Idle,
            target: None,
            channel: None,
            session: None,
            server: None,
            prev_server: None,
            self_mute: false,
            self_deaf: false,
            generation: 0,
            attempt: 0,
            leave_requested: false,
            ssrc: None,
            deadline: None,
            last_reinstall: None,
            action_seq: 0,
        }
    }

    /// One transition (§4.5 table). Pure: appends actions to `out`, performs no I/O.
    pub fn step(&mut self, input: GwInput, now: Instant, out: &mut SmallVec<[GwAction; 4]>) {
        // phase 0: every (phase, input) pair is a no-op.
    }

    /// The earliest armed deadline, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// The current public status.
    pub fn status(&self) -> StatusSnapshot {
        let (close_reason, close_code) = match self.phase {
            Phase::Closed(r) => (Some(r), r.code()),
            _ => (None, None),
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
            close_code,
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
mod tests {
    use super::*;

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
        let id = NonZeroU64::new(1).unwrap();
        let g = GatewayState::new(GatewayParams::new(id, id, true, 3, true));
        let s = g.status();
        assert_eq!(s.state, "idle");
        assert_eq!(s.generation, 0);
        assert!(g.next_deadline().is_none());
    }
}
