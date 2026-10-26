//! `native.Control`, `native.VoiceEvent` and `native.EventSubscription` (W2, §6.4, §8.11, §12.6).
//!
//! The two complex enums hold only plain Rust data (ids, counters and owned `String` reasons); they
//! never hold Python objects (I1).

use std::sync::Arc;

use pyo3::prelude::*;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pyclass_complex_enum, gen_stub_pymethods};
use sbcore::CoreError;
use sbcore::ctrl::Control as CoreControl;
use sbcore::events::{EventData, EventQueue, Pop};

use crate::errors::{core_err, ensure_not_forked};
use crate::py::chunk::monotonic;

/// A request from Rust to the Python pump (§6.4).
///
/// Variants
/// --------
/// ChannelMoved(channel_id)
///     An external move: update ``channel``.
/// ClassifyUsers()
///     Classify ``take_unclassified_users()`` from the member cache.
/// Rejoin(attempt, channel_id)
///     Send op 4 ``None`` then op 4 ``(channel, flags)``.
/// Closed(reason)
///     Sticky: finalize the client.
#[gen_stub_pyclass_complex_enum]
#[pyclass(module = "discord.ext.songbird.native", frozen, skip_from_py_object)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    ChannelMoved { channel_id: u64 },
    ClassifyUsers {},
    Rejoin { attempt: u32, channel_id: u64 },
    Closed { reason: String },
}

impl From<CoreControl> for Control {
    fn from(c: CoreControl) -> Self {
        match c {
            CoreControl::ChannelMoved { channel_id } => Control::ChannelMoved { channel_id },
            CoreControl::ClassifyUsers => Control::ClassifyUsers {},
            CoreControl::Rejoin {
                attempt,
                channel_id,
            } => Control::Rejoin {
                attempt,
                channel_id,
            },
            CoreControl::Closed { reason } => Control::Closed {
                reason: reason.as_str().to_owned(),
            },
        }
    }
}

/// A voice event (§9.6). Each variant has ``__match_args__``.
///
/// The declaration order defines the ``kind_mask`` bit of each variant for
/// ``Connection.subscribe_events`` (``Connected`` = bit 0 … ``Lagged`` = bit 12).
#[gen_stub_pyclass_complex_enum]
#[pyclass(module = "discord.ext.songbird.native", frozen, skip_from_py_object)]
#[derive(Clone, Debug, PartialEq)]
pub enum VoiceEvent {
    Connected {
        channel_id: u64,
        ssrc: u32,
        timeline_ms: u64,
    },
    Reconnected {
        channel_id: u64,
        ssrc: u32,
        gap_ms: u64,
    },
    Disconnected {
        reason: String,
        code: Option<u16>,
        will_retry: bool,
    },
    UserMapped {
        user_id: u64,
        ssrc: u32,
    },
    UserLeft {
        user_id: u64,
    },
    SpeakingStarted {
        user_id: u64,
        timeline_ms: u64,
        /// ``time.monotonic()`` of the tick that started the spurt.
        captured_at: f64,
    },
    SpeakingStopped {
        user_id: u64,
        timeline_ms: u64,
        duration_ms: u64,
    },
    PlaybackStarted {
        playback_id: u32,
    },
    PlaybackIdle {
        playback_id: u32,
        played_ms: u64,
    },
    PlaybackEnded {
        playback_id: u32,
        reason: String,
    },
    TrackEnded {
        track_id: u64,
        /// ``"finished"``, ``"stopped"``, ``"error"`` or ``"disconnected"``.
        reason: String,
    },
    ReceiveStalled {
        since_ms: u64,
    },
    Lagged {
        dropped: u64,
    },
}

impl VoiceEvent {
    /// Converts a core event while attached (`captured_at` → `time.monotonic()`).
    pub(crate) fn from_core(py: Python<'_>, ev: EventData) -> Self {
        match ev {
            EventData::Connected {
                channel_id,
                ssrc,
                timeline_ms,
            } => VoiceEvent::Connected {
                channel_id,
                ssrc,
                timeline_ms,
            },
            EventData::Reconnected {
                channel_id,
                ssrc,
                gap_ms,
            } => VoiceEvent::Reconnected {
                channel_id,
                ssrc,
                gap_ms,
            },
            EventData::Disconnected {
                reason,
                code,
                will_retry,
            } => VoiceEvent::Disconnected {
                reason: reason.to_owned(),
                code,
                will_retry,
            },
            EventData::UserMapped { user_id, ssrc } => VoiceEvent::UserMapped { user_id, ssrc },
            EventData::UserLeft { user_id } => VoiceEvent::UserLeft { user_id },
            EventData::SpeakingStarted {
                user_id,
                timeline_ms,
                captured_at,
            } => VoiceEvent::SpeakingStarted {
                user_id,
                timeline_ms,
                captured_at: monotonic(py, captured_at),
            },
            EventData::SpeakingStopped {
                user_id,
                timeline_ms,
                duration_ms,
            } => VoiceEvent::SpeakingStopped {
                user_id,
                timeline_ms,
                duration_ms,
            },
            EventData::PlaybackStarted { playback_id } => {
                VoiceEvent::PlaybackStarted { playback_id }
            }
            EventData::PlaybackIdle {
                playback_id,
                played_ms,
            } => VoiceEvent::PlaybackIdle {
                playback_id,
                played_ms,
            },
            EventData::PlaybackEnded {
                playback_id,
                reason,
            } => VoiceEvent::PlaybackEnded {
                playback_id,
                reason: reason.to_owned(),
            },
            EventData::TrackEnded { track_id, reason } => VoiceEvent::TrackEnded {
                track_id,
                reason: reason.as_str().to_owned(),
            },
            EventData::ReceiveStalled { since_ms } => VoiceEvent::ReceiveStalled { since_ms },
            EventData::Lagged { dropped } => VoiceEvent::Lagged { dropped },
        }
    }
}

/// A bounded (drop-oldest) event queue; a ``Lagged`` event reports drops.
///
/// Notes
/// -----
/// Dropping the object closes the queue.
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen)]
pub struct EventSubscription {
    queue: Arc<EventQueue>,
}

impl EventSubscription {
    pub(crate) fn new(queue: Arc<EventQueue>) -> Self {
        EventSubscription { queue }
    }
}

impl Drop for EventSubscription {
    fn drop(&mut self) {
        // §4.10: in a forked child, never take a lock a vanished parent thread may hold (the
        // connection's emitters). The queue is simply freed with its last reference.
        if sbcore::runtime::check_not_forked().is_err() {
            return;
        }
        self.queue.close();
    }
}

fn pop_event(py: Python<'_>, p: Pop<EventData>) -> PyResult<Option<VoiceEvent>> {
    match p {
        Pop::Item(ev) => Ok(Some(VoiceEvent::from_core(py, ev))),
        Pop::Empty => Ok(None),
        Pop::Closed(r) => Err(core_err(py, CoreError::StreamClosed(r))),
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl EventSubscription {
    /// Pops the next event, or arms ``wake_id`` and returns ``None`` (loop thread).
    ///
    /// Raises
    /// ------
    /// StreamClosed
    ///     Once the queue is closed and drained.
    fn try_recv(&self, py: Python<'_>) -> PyResult<Option<VoiceEvent>> {
        ensure_not_forked(py)?;
        pop_event(py, self.queue.try_pop_or_arm())
    }

    /// Pops without arming (any thread); ``None`` if empty.
    ///
    /// Raises
    /// ------
    /// StreamClosed
    ///     Once the queue is closed and drained.
    fn read_nowait(&self, py: Python<'_>) -> PyResult<Option<VoiceEvent>> {
        ensure_not_forked(py)?;
        pop_event(py, self.queue.try_pop())
    }

    /// The wake id rung when an event arrives after arming.
    #[getter]
    fn wake_id(&self) -> u8 {
        self.queue.wake_id().0
    }

    /// Events dropped by the overflow policy.
    #[getter]
    fn dropped(&self) -> u64 {
        self.queue.dropped()
    }

    /// Idempotent; any thread.
    fn close(&self) {
        // A no-op after fork() (§4.10, see Drop).
        if sbcore::runtime::check_not_forked().is_ok() {
            self.queue.close();
        }
    }
}
