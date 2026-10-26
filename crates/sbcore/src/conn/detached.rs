//! Detached connections (feature `test-hooks`, §13.2): a full `ConnShared` with **no songbird
//! Driver**. Every track gets a [`FakeTrackControl`] instead of a songbird handle, and the songbird
//! `Input` built for it is parked here, so `MixerHarness` can take a playback's reader/decoder pair
//! and dropping parked inputs runs the real `DoneGuard` path. `Connect` actions succeed instantly
//! (configurable), and tests inject driver events through the same `apply()`/`RxSender` paths the
//! `Dispatch` uses.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use super::gateway::{DiscKind, DiscReason, GwInput};
use super::shared::ConnShared;
use crate::rx::raw::RxSender;
use crate::sync::lock_checked;
use crate::tracks::flags::{TrackFlags, TrackRef};
use crate::tx::{FakeTrackControl, TrackControl};

/// One parked songbird input.
struct Parked {
    flags: Weak<TrackFlags>,
    source_id: Option<u32>,
    input: songbird::input::Input,
}

/// Per-connection detached state.
pub(crate) struct DetachedState {
    auto_connect: AtomicBool,
    /// L10. Bounded: entries of ended or dropped tracks are pruned on every park.
    parked: Mutex<Vec<Parked>>,
}

impl DetachedState {
    pub(crate) fn new() -> Self {
        DetachedState {
            auto_connect: AtomicBool::new(true),
            parked: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn auto_connect(&self) -> bool {
        self.auto_connect.load(Ordering::Acquire)
    }

    fn park(&self, flags: &Arc<TrackFlags>, source_id: Option<u32>, input: songbird::input::Input) {
        let pruned: Vec<Parked> = {
            let mut v = lock_checked(&self.parked);
            let (keep, gone): (Vec<Parked>, Vec<Parked>) = v
                .drain(..)
                .partition(|p| p.flags.upgrade().is_some_and(|f| f.ended().is_none()));
            *v = keep;
            v.push(Parked {
                flags: Arc::downgrade(flags),
                source_id,
                input,
            });
            gone
        };
        drop(pruned); // DoneGuards drop outside the lock
    }

    /// Drops every parked input (stop_all, T8): the real `DoneGuard` path.
    pub(crate) fn drop_all(&self) {
        let all = std::mem::take(&mut *lock_checked(&self.parked));
        drop(all);
    }

    fn take_where(&self, pred: impl Fn(&Parked) -> bool) -> Option<songbird::input::Input> {
        let mut v = lock_checked(&self.parked);
        let i = v.iter().position(pred)?;
        Some(v.swap_remove(i).input)
    }
}

/// `Box<dyn TrackControl>` over a shared fake (the `TrackRef` keeps the other `Arc` for its log).
struct SharedFake(Arc<FakeTrackControl>);

impl TrackControl for SharedFake {
    fn pause(&self) {
        self.0.pause();
    }

    fn play(&self) {
        self.0.play();
    }

    fn stop(&self) {
        self.0.stop();
    }
}

impl ConnShared {
    /// Detached: attaches a `FakeTrackControl`, marks the track started and parks its input.
    pub(super) fn park(
        &self,
        track: &TrackRef,
        input: songbird::input::Input,
        source_id: Option<u32>,
    ) -> Box<dyn TrackControl> {
        let fake = track
            .fake
            .get_or_init(|| Arc::new(FakeTrackControl::new(track.flags.clone())))
            .clone();
        self.tracks().mark_started(track);
        self.det.park(&track.flags, source_id, input);
        Box::new(SharedFake(fake))
    }

    /// Detached connections: whether `Connect` succeeds instantly (default `true`). With `false`,
    /// tests feed `ConnectResult`s through [`ConnShared::inject_gateway`].
    pub fn set_detached_auto_connect(&self, on: bool) {
        self.det.auto_connect.store(on, Ordering::Release);
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

    /// The sender `Dispatch` uses for receive inputs (`None` with `receive=False`).
    pub fn rx_sender(&self) -> Option<RxSender> {
        self.rx_sender_ref().cloned()
    }

    /// Takes the parked input of a memory/queue track of this detached connection.
    pub fn take_parked_input(&self, track: &TrackRef) -> Option<songbird::input::Input> {
        self.det
            .take_where(|p| std::ptr::eq(p.flags.as_ptr(), Arc::as_ptr(&track.flags)))
    }

    fn take_parked_source(&self, source_id: u32) -> Option<songbird::input::Input> {
        self.det.take_where(|p| p.source_id == Some(source_id))
    }
}

/// Takes the parked input of the live source with `LiveSource::id() == source_id` (ids are unique
/// per process), searching every registered connection (`MixerHarness::take`).
pub fn take_parked_live_input(source_id: u32) -> Option<songbird::input::Input> {
    super::registry::registry_snapshot()
        .iter()
        .find_map(|c| c.take_parked_source(source_id))
}
