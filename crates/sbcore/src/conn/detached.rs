//! Detached connections (feature `test-hooks`, §13.2): a full `ConnShared` with **no songbird
//! Driver**. Every track gets a [`crate::tx::FakeTrackControl`] instead of a songbird handle, and the
//! songbird `Input` built for it is parked in the test hooks' tables (`testing::registry`), so
//! `MixerHarness` can take a playback's reader/decoder pair and dropping parked inputs runs the real
//! `DoneGuard` path. `Connect` actions succeed instantly unless turned off
//! (`set_detached_auto_connect(false)`, which `testing::Harness` does), and tests inject driver
//! events through the same `apply()`/`RxSender` paths the `Dispatch` uses.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::gateway::{DiscKind, DiscReason, GwInput};
use super::ops::{OpResult, TrackInfoData};
use super::shared::ConnShared;
use crate::error::CoreError;
use crate::rx::raw::RxSender;
use crate::testing::registry;
use crate::tracks::flags::TrackRef;
use crate::tx::TrackControl;

/// Per-connection detached state. The parked inputs and the fake controls live in the test hooks'
/// process-wide tables (`testing::registry`), keyed by this connection, so `MixerHarness`,
/// `track_log` and `Harness::{step, parked}` see exactly what this connection parked.
pub(crate) struct DetachedState {
    auto_connect: AtomicBool,
    ops_answered: AtomicBool,
}

impl DetachedState {
    pub(crate) fn new() -> Self {
        DetachedState {
            auto_connect: AtomicBool::new(true),
            ops_answered: AtomicBool::new(true),
        }
    }

    pub(crate) fn auto_connect(&self) -> bool {
        self.auto_connect.load(Ordering::Acquire)
    }

    pub(crate) fn ops_answered(&self) -> bool {
        self.ops_answered.load(Ordering::Acquire)
    }
}

/// songbird's side of an `info()` op on a detached track: answered from its `FakeTrackControl`
/// as a live mixer would, or, when `answered` is off, never (a parked mixer), so the op times out
/// after `timeout` exactly like a real one. `None` for a track without a fake control (not started).
pub(super) fn info_op(
    t: &TrackRef,
    answered: bool,
    timeout: Duration,
) -> Option<impl Future<Output = OpResult> + Send + 'static> {
    let fake = t.fake.get()?.clone();
    let track = t.clone();
    Some(async move {
        if !answered {
            return unanswered(timeout).await;
        }
        if track.flags.ended().is_some() {
            // songbird's handle of an ended track: `ControlError::Finished`.
            return OpResult::Err(CoreError::Track("track has ended".into()));
        }
        OpResult::Info(TrackInfoData {
            playing: if fake.is_paused() { "pause" } else { "play" },
            // A live PCM source applies its volume in its decoder, as `start_track_info` reports.
            volume: track.source_volume().unwrap_or_else(|| fake.volume()),
            position_ms: u64::try_from(fake.position().as_millis()).unwrap_or(u64::MAX),
            play_time_ms: 0,
            loops: fake.loops(),
        })
    })
}

/// songbird's side of a `seek()` op on a detached track (see [`info_op`]).
pub(super) fn seek_op(
    t: &TrackRef,
    pos: Duration,
    answered: bool,
    timeout: Duration,
) -> Option<impl Future<Output = OpResult> + Send + 'static> {
    let fake = t.fake.get()?.clone();
    let track = t.clone();
    Some(async move {
        if !answered {
            return unanswered(timeout).await;
        }
        if track.flags.ended().is_some() {
            return OpResult::Err(CoreError::Track("track has ended".into()));
        }
        fake.seek(pos);
        OpResult::Seeked(pos)
    })
}

/// A parked mixer never answers a track command: the op's own timeout ends it.
async fn unanswered(timeout: Duration) -> OpResult {
    match tokio::time::timeout(timeout, std::future::pending::<OpResult>()).await {
        Ok(r) => r,
        Err(_) => OpResult::TimedOut,
    }
}

impl ConnShared {
    /// Detached: attaches a `FakeTrackControl` (handed over `paused` or playing), marks the track
    /// started and parks its input (`source_id`: the live source's id, for `MixerHarness`).
    pub(super) fn park(
        &self,
        track: &TrackRef,
        input: songbird::input::Input,
        source_id: Option<u32>,
        paused: bool,
    ) -> Box<dyn TrackControl> {
        let fake = track
            .fake
            .get_or_init(|| registry::register_fake(&track.flags, paused))
            .clone();
        self.tracks().mark_started(track);
        let key = registry::key_of(self);
        if self.is_closed() {
            // T8 already disposed of this connection's inputs: songbird would drop it at once.
            drop(input);
        } else {
            // Inputs of ended tracks are disposed of as songbird would (keeps the table bounded).
            registry::sweep(key, false);
            registry::park_input(self, source_id, track.flags.clone(), input);
        }
        registry::fake_handle(fake)
    }

    /// Drops every parked input of this connection (`stop_all`, T8): the real `DoneGuard` path.
    pub(super) fn drop_parked(&self) {
        registry::sweep(registry::key_of(self), true);
    }

    /// Detached connections: whether `Connect` succeeds instantly (default `true`). With `false`,
    /// tests feed `ConnectResult`s through [`ConnShared::inject_gateway`].
    pub fn set_detached_auto_connect(&self, on: bool) {
        self.det.auto_connect.store(on, Ordering::Release);
    }

    /// Detached connections: whether track `info()`/`seek()` ops are answered (default `true`,
    /// like a live mixer). With `false` they time out, as on songbird's parked mixer.
    pub fn set_detached_ops_answered(&self, on: bool) {
        self.det.ops_answered.store(on, Ordering::Release);
    }

    /// Feeds one state-machine input through `apply()` (§4.6), exactly as the binding or the
    /// `Dispatch` would.
    pub fn inject_gateway(&self, input: GwInput) {
        self.apply(input);
    }

    /// `DriverConnect` / `DriverReconnect` through the `Dispatch` path (`apply` + `LinkUp`).
    pub fn inject_driver_connect(&self, ssrc: u32, reconnect: bool) {
        self.dispatch().inner().driver_connect(ssrc, reconnect);
    }

    /// `DriverDisconnect` through the `Dispatch` path (`apply` + `LinkDown`).
    pub fn inject_driver_disconnect(&self, kind: DiscKind, reason: Option<DiscReason>) {
        self.dispatch().inner().driver_disconnect(kind, reason);
    }

    /// The sender `Dispatch` uses for receive inputs (also open with `receive=False`, which only
    /// stops audio: control inputs still flow).
    pub fn rx_sender(&self) -> Option<RxSender> {
        self.rx_sender_ref().cloned()
    }

    /// Takes the parked input of a track of this detached connection.
    pub fn take_parked_input(&self, track: &TrackRef) -> Option<songbird::input::Input> {
        registry::take_parked_track(registry::key_of(self), &track.flags)
    }
}

/// Takes the parked input of the live source with `LiveSource::id() == source_id` (ids are unique
/// per process); `MixerHarness::take_source` uses the same table.
pub fn take_parked_live_input(source_id: u32) -> Option<songbird::input::Input> {
    registry::take_parked(source_id).map(|(_, _, input)| input)
}
