//! The core error model (§11.1). The binding converts [`CoreError`] to Python exceptions.

use std::borrow::Cow;

use crate::limits::LimitError;

/// Why a connection, stream or subscription closed. `as_str()` is the Python-visible reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Requested,
    LeftChannel,
    Kicked,
    ConnectFailed,
    SessionInvalid(Option<u16>),
    RejoinFailed,
    Lost,
    Dropped,
    LoopClosed,
    ProcessExit,
    ConnectAborted,
    Cleanup,
    Overflow,
    UserLeft,
    InternalError,
    Closed,
    ConnectionClosed,
}

impl CloseReason {
    /// The stable snake_case name used by the Python layer (`close_reason`, `reason`).
    pub fn as_str(&self) -> &'static str {
        match self {
            CloseReason::Requested => "requested",
            CloseReason::LeftChannel => "left_channel",
            CloseReason::Kicked => "kicked",
            CloseReason::ConnectFailed => "connect_failed",
            CloseReason::SessionInvalid(_) => "session_invalid",
            CloseReason::RejoinFailed => "rejoin_failed",
            CloseReason::Lost => "lost",
            CloseReason::Dropped => "dropped",
            CloseReason::LoopClosed => "loop_closed",
            CloseReason::ProcessExit => "process_exit",
            CloseReason::ConnectAborted => "connect_aborted",
            CloseReason::Cleanup => "cleanup",
            CloseReason::Overflow => "overflow",
            CloseReason::UserLeft => "user_left",
            CloseReason::InternalError => "internal_error",
            CloseReason::Closed => "closed",
            CloseReason::ConnectionClosed => "connection_closed",
        }
    }

    /// The Discord voice close code attached to the reason, if any.
    pub fn code(&self) -> Option<u16> {
        match self {
            CloseReason::SessionInvalid(code) => *code,
            _ => None,
        }
    }

    /// Inverse of [`CloseReason::as_str`] (codes are not encoded in the name).
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "requested" => CloseReason::Requested,
            "left_channel" => CloseReason::LeftChannel,
            "kicked" => CloseReason::Kicked,
            "connect_failed" => CloseReason::ConnectFailed,
            "session_invalid" => CloseReason::SessionInvalid(None),
            "rejoin_failed" => CloseReason::RejoinFailed,
            "lost" => CloseReason::Lost,
            "dropped" => CloseReason::Dropped,
            "loop_closed" => CloseReason::LoopClosed,
            "process_exit" => CloseReason::ProcessExit,
            "connect_aborted" => CloseReason::ConnectAborted,
            "cleanup" => CloseReason::Cleanup,
            "overflow" => CloseReason::Overflow,
            "user_left" => CloseReason::UserLeft,
            "internal_error" => CloseReason::InternalError,
            "closed" => CloseReason::Closed,
            "connection_closed" => CloseReason::ConnectionClosed,
            _ => return None,
        })
    }
}

impl std::fmt::Display for CloseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every fallible sbcore operation returns this.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Limit(#[from] LimitError),
    #[error("invalid audio: {0}")]
    InvalidAudio(Cow<'static, str>),
    #[error("allocation failed")]
    Alloc,
    #[error("connection closed: {}", .0.as_str())]
    ConnectionClosed(CloseReason),
    #[error("stream closed: {}", .0.as_str())]
    StreamClosed(CloseReason),
    #[error("receive overflow")]
    StreamOverflow,
    #[error("playback buffer full")]
    PlaybackFull { needed_ms: u64, free_ms: u64 },
    #[error("receive disabled")]
    ReceiveDisabled,
    #[error("connect failed: {0}")]
    ConnectFailed(Cow<'static, str>),
    #[error("source: {0}")]
    Source(String),
    /// An I/O error of `tx::memory::read_file` (`AudioFile.from_path`), raised in Python as the
    /// matching `OSError` subclass (§11.1). Deliberately no `#[from]`: only `read_file` produces it.
    #[error(transparent)]
    Io(std::io::Error),
    #[error("track: {0}")]
    Track(Cow<'static, str>),
    #[error("runtime already configured")]
    RuntimeConfigured,
    #[error("unusable after fork()")]
    Forked,
    #[error("built without the '{0}' feature")]
    FeatureMissing(&'static str),
    #[error("internal error at {0}")]
    Internal(&'static str),
}

impl CoreError {
    /// Shorthand for a static `InvalidAudio` message.
    pub const fn invalid(msg: &'static str) -> Self {
        CoreError::InvalidAudio(Cow::Borrowed(msg))
    }
}

impl From<std::collections::TryReserveError> for CoreError {
    fn from(_: std::collections::TryReserveError) -> Self {
        CoreError::Alloc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{CHUNK_MS, check};

    const ALL: [CloseReason; 17] = [
        CloseReason::Requested,
        CloseReason::LeftChannel,
        CloseReason::Kicked,
        CloseReason::ConnectFailed,
        CloseReason::SessionInvalid(None),
        CloseReason::RejoinFailed,
        CloseReason::Lost,
        CloseReason::Dropped,
        CloseReason::LoopClosed,
        CloseReason::ProcessExit,
        CloseReason::ConnectAborted,
        CloseReason::Cleanup,
        CloseReason::Overflow,
        CloseReason::UserLeft,
        CloseReason::InternalError,
        CloseReason::Closed,
        CloseReason::ConnectionClosed,
    ];

    #[test]
    fn names_round_trip_and_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for r in ALL {
            assert!(seen.insert(r.as_str()), "duplicate name {}", r.as_str());
            assert_eq!(CloseReason::from_name(r.as_str()), Some(r));
        }
        assert_eq!(CloseReason::from_name("nope"), None);
    }

    #[test]
    fn codes() {
        assert_eq!(CloseReason::SessionInvalid(Some(4006)).code(), Some(4006));
        assert_eq!(CloseReason::Kicked.code(), None);
    }

    #[test]
    fn display_messages() {
        let e: CoreError = check("chunk_ms", 0u32, CHUNK_MS).unwrap_err().into();
        assert!(matches!(e, CoreError::Limit(_)));
        assert_eq!(
            e.to_string(),
            "chunk_ms = 0 is outside the allowed range [10, 1000]"
        );
        assert_eq!(
            CoreError::StreamClosed(CloseReason::UserLeft).to_string(),
            "stream closed: user_left"
        );
        assert_eq!(
            CoreError::FeatureMissing("arrow").to_string(),
            "built without the 'arrow' feature"
        );
        assert_eq!(CoreError::invalid("bad").to_string(), "invalid audio: bad");
    }

    #[test]
    fn try_reserve_maps_to_alloc() {
        let mut v: Vec<u8> = Vec::new();
        let e: CoreError = v.try_reserve(usize::MAX).unwrap_err().into();
        assert!(matches!(e, CoreError::Alloc));
    }
}
