//! Sole ownership of the songbird `Driver` (W2, §4.8, I3).
//!
//! [`OwnedDriver`] has **no `Clone` and no `Deref`**: `songbird::Driver: Clone`, and dropping any
//! clone sends `Poison`. It lives in `ConnShared.driver` (L1) and is `take()`n exactly once, at
//! teardown. Every method enters the NET runtime (songbird may `tokio::spawn`) and does O(1) flume
//! sends only.
#![allow(dead_code)] // phase-0 skeleton

use std::num::NonZeroU8;
use std::time::Duration;

use songbird::driver::retry::Retry;
use songbird::driver::{Bitrate, DecodeMode};
use songbird::events::{CoreEvent, Event, TrackEvent};

use super::dispatch::Dispatch;
use super::settings::VoiceSettings;
use crate::limits::PREALLOCATED_TRACKS;
use crate::runtime::{self, LiveCounter, live};

/// The songbird configuration of §4.8. Never `retry_limit: None` (which means infinite retries).
pub(crate) fn build_config(s: &VoiceSettings, reconnect: bool) -> songbird::Config {
    let retry = Retry {
        retry_limit: Some(if reconnect {
            s.driver_retries
        } else {
            NonZeroU8::MIN
        }),
        ..Retry::default()
    };
    songbird::Config::default()
        .decode_mode(if s.receive {
            DecodeMode::Decrypt
        } else {
            DecodeMode::Pass
        })
        .playout_buffer_length(s.playout_packets)
        .playout_spike_length(s.spike_packets)
        .decode_state_timeout(s.decode_state_timeout)
        .mix_mode(s.mix_mode)
        .use_softclip(s.softclip)
        .driver_timeout(Some(s.driver_timeout))
        .driver_retry(retry)
        .preallocated_tracks(PREALLOCATED_TRACKS)
        .disposer(runtime::disposer().clone())
        .scheduler(runtime::scheduler().clone())
}

/// The one owner of a songbird `Driver` (I3). NO `Clone`, NO `Deref`.
pub(crate) struct OwnedDriver {
    d: songbird::Driver,
    last_connect_gen: u64,
    _live: LiveCounter,
}

impl OwnedDriver {
    /// Creates the Driver under `runtime::net().enter()` (`Driver::new` spawns tasks).
    pub(crate) fn new(cfg: songbird::Config) -> Self {
        let _g = runtime::net().enter();
        OwnedDriver {
            d: songbird::Driver::new(cfg),
            last_connect_gen: 0,
            _live: LiveCounter::new(&live::OWNED_DRIVER),
        }
    }

    /// `None` if `generation ≤ last_connect_gen` (stale batch, §4.6).
    pub(crate) fn connect(
        &mut self,
        info: songbird::ConnectionInfo,
        generation: u64,
    ) -> Option<songbird::driver::Connect> {
        if generation <= self.last_connect_gen {
            return None;
        }
        self.last_connect_gen = generation;
        let _g = runtime::net().enter();
        Some(self.d.connect(info))
    }

    pub(crate) fn mute(&mut self, m: bool) {
        let _g = runtime::net().enter();
        self.d.mute(m);
    }

    pub(crate) fn set_bitrate(&mut self, b: Bitrate) {
        let _g = runtime::net().enter();
        self.d.set_bitrate(b);
    }

    pub(crate) fn play(&mut self, t: songbird::tracks::Track) -> songbird::tracks::TrackHandle {
        let _g = runtime::net().enter();
        self.d.play(t)
    }

    pub(crate) fn stop(&mut self) {
        let _g = runtime::net().enter();
        self.d.stop();
    }

    /// Registers the dispatcher once per event (§4.8), plus the 1 s heartbeat handler.
    pub(crate) fn install_dispatch(&mut self, d: &Dispatch, receive: bool) {
        let _g = runtime::net().enter();
        for ev in [
            CoreEvent::SpeakingStateUpdate,
            CoreEvent::ClientDisconnect,
            CoreEvent::DriverConnect,
            CoreEvent::DriverReconnect,
            CoreEvent::DriverDisconnect,
        ] {
            self.d.add_global_event(Event::Core(ev), d.clone());
        }
        if receive {
            self.d
                .add_global_event(Event::Core(CoreEvent::VoiceTick), d.clone());
        }
        self.d
            .add_global_event(Event::Track(TrackEvent::End), d.clone());
        self.d
            .add_global_event(Event::Track(TrackEvent::Error), d.clone());
        self.d
            .add_global_event(Event::Periodic(Duration::from_secs(1), None), d.heartbeat());
    }

    /// `remove_all_global_events()` and install again (watchdog, §8.8).
    pub(crate) fn reinstall_dispatch(&mut self, d: &Dispatch, receive: bool) {
        {
            let _g = runtime::net().enter();
            self.d.remove_all_global_events();
        }
        self.install_dispatch(d, receive);
    }

    /// T2–T4: drop every `Dispatch` clone, stop all tracks (dropped inline on the mixer or Idle
    /// task), leave, then drop the Driver (`Poison`).
    pub(crate) fn teardown(self) {
        let OwnedDriver { mut d, _live, .. } = self;
        let _g = runtime::net().enter();
        d.remove_all_global_events();
        d.stop();
        d.leave();
        drop(d);
    }
}
