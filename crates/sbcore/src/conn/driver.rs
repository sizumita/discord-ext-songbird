//! Sole ownership of the songbird `Driver` (W2, §4.8, I3).
//!
//! [`OwnedDriver`] has **no `Clone` and no `Deref`**: `songbird::Driver: Clone`, and dropping any
//! clone sends `Poison`. It lives in `ConnShared.driver` (L1) and is `take()`n exactly once, at
//! teardown. Every method enters the NET runtime (songbird may `tokio::spawn`) and does O(1) flume
//! sends only.

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

/// How long a `Connect` future may run (§4.8): `min(120 s, driver_timeout × (retries + 1) + 30 s)`,
/// with `retries` = `driver_retries` (`reconnect=True`) or 1.
pub(crate) fn connect_deadline(s: &VoiceSettings, reconnect: bool) -> Duration {
    let retries = if reconnect {
        u32::from(s.driver_retries.get())
    } else {
        1
    };
    s.driver_timeout
        .saturating_mul(retries.saturating_add(1))
        .saturating_add(Duration::from_secs(30))
        .min(Duration::from_secs(120))
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

    /// `SetTrack(None)`: songbird drops every input inline on the mixer (or the Idle task). Memory
    /// buffers survive that drop: `tx::keep` holds them and frees them on RX.
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
    /// task; their memory buffers are freed later on RX by `tx::keep`), leave, then drop the Driver
    /// (`Poison`).
    pub(crate) fn teardown(self) {
        let OwnedDriver { mut d, _live, .. } = self;
        let _g = runtime::net().enter();
        d.remove_all_global_events();
        d.stop();
        d.leave();
        drop(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::settings::RawVoiceSettings;

    #[test]
    fn config_never_retries_infinitely_and_preallocates() {
        for (reconnect, retries) in [(true, 5u8), (true, 1), (false, 7)] {
            let s = VoiceSettings::validate(RawVoiceSettings {
                driver_retries: retries,
                ..RawVoiceSettings::default()
            })
            .unwrap();
            let cfg = build_config(&s, reconnect);
            let limit = cfg.driver_retry.retry_limit.expect("never None (infinite)");
            assert_eq!(limit.get(), if reconnect { retries } else { 1 });
            assert_eq!(cfg.preallocated_tracks, PREALLOCATED_TRACKS);
            assert!(cfg.driver_timeout.is_some());
        }
        let off = VoiceSettings::validate(RawVoiceSettings {
            receive: false,
            ..RawVoiceSettings::default()
        })
        .unwrap();
        assert!(matches!(
            build_config(&off, true).decode_mode,
            DecodeMode::Pass
        ));
        let on = VoiceSettings::validate(RawVoiceSettings::default()).unwrap();
        assert!(matches!(
            build_config(&on, true).decode_mode,
            DecodeMode::Decrypt
        ));
    }

    #[test]
    fn connect_deadline_is_capped() {
        let s = VoiceSettings::validate(RawVoiceSettings::default()).unwrap();
        // 10 s × (5 + 1) + 30 s = 90 s.
        assert_eq!(connect_deadline(&s, true), Duration::from_secs(90));
        // 10 s × 2 + 30 s.
        assert_eq!(connect_deadline(&s, false), Duration::from_secs(50));
        let slow = VoiceSettings::validate(RawVoiceSettings {
            driver_timeout_s: 60.0,
            driver_retries: 10,
            ..RawVoiceSettings::default()
        })
        .unwrap();
        assert_eq!(connect_deadline(&slow, true), Duration::from_secs(120));
    }
}
