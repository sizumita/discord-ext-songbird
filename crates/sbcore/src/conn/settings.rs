//! Per-connection settings (W2, §9.3, §12.3): `VoiceConfig.to_native()` → `native.VoiceSettings`.

use std::num::{NonZeroU8, NonZeroU64};
use std::ops::RangeInclusive;
use std::time::Duration;

use songbird::driver::MixMode;

use crate::clock::Clock;
use crate::error::CoreError;
use crate::limits::{PLAYOUT_BUFFER_MS, PLAYOUT_SPIKE_MS, TICK_MS, check};

/// `decode_state_timeout_s` range.
pub const DECODE_STATE_TIMEOUT_S: RangeInclusive<f64> = 1.0..=600.0;
/// `speaking_hangover_ms` range.
pub const SPEAKING_HANGOVER_MS: RangeInclusive<u32> = 20..=5_000;
/// `voice_threshold_dbfs` range.
pub const VOICE_THRESHOLD_DBFS: RangeInclusive<f32> = -100.0..=0.0;
/// `max_decoded_users` range.
pub const MAX_DECODED_USERS: RangeInclusive<u32> = 1..=256;
/// `max_receive_mb` range.
pub const MAX_RECEIVE_MB: RangeInclusive<u32> = 1..=1_024;
/// `stall_timeout_ms` range.
pub const STALL_TIMEOUT_MS: RangeInclusive<u32> = 500..=30_000;
/// `bitrate` range (bps).
pub const BITRATE_BPS: RangeInclusive<i64> = 6_000..=510_000;
/// `driver_timeout_s` range.
pub const DRIVER_TIMEOUT_S: RangeInclusive<f64> = 1.0..=60.0;
/// `driver_retries` range. 0 is rejected: songbird cannot express "no retries" with
/// `reconnect=True`, and `None` would mean infinite retries (§4.8).
pub const DRIVER_RETRIES: RangeInclusive<u8> = 1..=10;
/// `rejoin_attempts` range.
pub const REJOIN_ATTEMPTS: RangeInclusive<u8> = 0..=10;

/// Unvalidated settings, exactly as passed from Python (`VoiceConfig` fields, §9.3).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RawVoiceSettings {
    pub receive: bool,
    pub playout_buffer_ms: u32,
    pub playout_spike_ms: u32,
    pub decode_state_timeout_s: f64,
    pub speaking_hangover_ms: u32,
    pub voice_threshold_dbfs: Option<f32>,
    pub max_decoded_users: u32,
    pub max_receive_mb: u32,
    pub stall_timeout_ms: u32,
    pub mix_mode_mono: bool,
    pub bitrate: Option<i32>,
    pub softclip: bool,
    pub driver_timeout_s: f64,
    pub driver_retries: u8,
    pub rejoin_attempts: u8,
    pub auto_recover: bool,
}

impl Default for RawVoiceSettings {
    /// The `VoiceConfig` defaults of §9.3.
    fn default() -> Self {
        RawVoiceSettings {
            receive: true,
            playout_buffer_ms: 60,
            playout_spike_ms: 60,
            decode_state_timeout_s: 60.0,
            speaking_hangover_ms: 200,
            voice_threshold_dbfs: None,
            max_decoded_users: 64,
            max_receive_mb: 64,
            stall_timeout_ms: 2_000,
            mix_mode_mono: false,
            bitrate: None,
            softclip: true,
            driver_timeout_s: 10.0,
            driver_retries: 5,
            rejoin_attempts: 3,
            auto_recover: true,
        }
    }
}

/// Validated settings in the units the Driver and the pipeline use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoiceSettings {
    pub receive: bool,
    pub playout_packets: NonZeroU8,
    pub spike_packets: u8,
    pub decode_state_timeout: Duration,
    pub hangover_ms: u32,
    pub voice_threshold_dbfs: Option<f32>,
    pub max_decoded_users: usize,
    pub max_receive_bytes: usize,
    pub stall_timeout: Duration,
    pub mix_mode: MixMode,
    pub bitrate: Option<i32>,
    pub softclip: bool,
    pub driver_timeout: Duration,
    pub driver_retries: NonZeroU8,
    pub rejoin_attempts: u8,
    pub auto_recover: bool,
}

/// `ms` must be a whole number of 20 ms ticks.
fn ticks_of(what: &'static str, ms: u32) -> Result<u8, CoreError> {
    if !ms.is_multiple_of(TICK_MS) {
        return Err(CoreError::InvalidAudio(
            format!("{what} must be a multiple of {TICK_MS} ms (got {ms})").into(),
        ));
    }
    u8::try_from(ms / TICK_MS).map_err(|_| CoreError::invalid("tick count out of range"))
}

impl VoiceSettings {
    /// Checks every field against its range (before any arithmetic) and converts units.
    ///
    /// Range violations are `CoreError::Limit` (`LimitExceeded`); a duration that is not a multiple
    /// of 20 ms is `CoreError::InvalidAudio` (both are `ValueError`s in Python).
    pub fn validate(raw: RawVoiceSettings) -> Result<Self, CoreError> {
        let playout_ms = check(
            "playout_buffer_ms",
            raw.playout_buffer_ms,
            PLAYOUT_BUFFER_MS,
        )?;
        let spike_ms = check("playout_spike_ms", raw.playout_spike_ms, PLAYOUT_SPIKE_MS)?;
        let playout_packets = NonZeroU8::new(ticks_of("playout_buffer_ms", playout_ms)?)
            .ok_or(CoreError::invalid("playout_buffer_ms must be at least 20"))?;
        let spike_packets = ticks_of("playout_spike_ms", spike_ms)?;
        let decode_state_timeout_s = check(
            "decode_state_timeout_s",
            raw.decode_state_timeout_s,
            DECODE_STATE_TIMEOUT_S,
        )?;
        let hangover_ms = check(
            "speaking_hangover_ms",
            raw.speaking_hangover_ms,
            SPEAKING_HANGOVER_MS,
        )?;
        let voice_threshold_dbfs = match raw.voice_threshold_dbfs {
            Some(v) => Some(check("voice_threshold_dbfs", v, VOICE_THRESHOLD_DBFS)?),
            None => None,
        };
        let max_decoded_users = check(
            "max_decoded_users",
            raw.max_decoded_users,
            MAX_DECODED_USERS,
        )?;
        let max_receive_mb = check("max_receive_mb", raw.max_receive_mb, MAX_RECEIVE_MB)?;
        let stall_timeout_ms = check("stall_timeout_ms", raw.stall_timeout_ms, STALL_TIMEOUT_MS)?;
        let bitrate = match raw.bitrate {
            Some(b) => Some(check("bitrate", i64::from(b), BITRATE_BPS)?),
            None => None,
        };
        let driver_timeout_s = check("driver_timeout_s", raw.driver_timeout_s, DRIVER_TIMEOUT_S)?;
        let driver_retries =
            NonZeroU8::new(check("driver_retries", raw.driver_retries, DRIVER_RETRIES)?)
                .ok_or(CoreError::invalid("driver_retries must be at least 1"))?;
        let rejoin_attempts = check("rejoin_attempts", raw.rejoin_attempts, REJOIN_ATTEMPTS)?;

        let max_receive_bytes = usize::try_from(max_receive_mb)
            .ok()
            .and_then(|mb| mb.checked_mul(1 << 20))
            .ok_or(CoreError::invalid("max_receive_mb overflows usize"))?;
        Ok(VoiceSettings {
            receive: raw.receive,
            playout_packets,
            spike_packets,
            decode_state_timeout: Duration::from_secs_f64(decode_state_timeout_s),
            hangover_ms,
            voice_threshold_dbfs,
            max_decoded_users: max_decoded_users as usize,
            max_receive_bytes,
            stall_timeout: Duration::from_millis(u64::from(stall_timeout_ms)),
            mix_mode: if raw.mix_mode_mono {
                MixMode::Mono
            } else {
                MixMode::Stereo
            },
            bitrate: bitrate.and_then(|b| i32::try_from(b).ok()),
            softclip: raw.softclip,
            driver_timeout: Duration::from_secs_f64(driver_timeout_s),
            driver_retries,
            rejoin_attempts,
            auto_recover: raw.auto_recover,
        })
    }
}

/// Everything `ConnShared::new` needs besides the doorbell.
#[derive(Clone, Debug)]
pub struct ConnParams {
    pub guild_id: NonZeroU64,
    pub user_id: NonZeroU64,
    pub reconnect: bool,
    pub settings: VoiceSettings,
    pub clock: Clock,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        let s = VoiceSettings::validate(RawVoiceSettings::default()).unwrap();
        assert_eq!(s.playout_packets.get(), 3);
        assert_eq!(s.spike_packets, 3);
        assert_eq!(s.max_receive_bytes, 64 << 20);
        assert_eq!(s.driver_retries.get(), 5);
        assert_eq!(s.mix_mode, MixMode::Stereo);
    }

    #[test]
    fn rejects_out_of_range_and_non_tick_values() {
        let bad = [
            RawVoiceSettings {
                playout_buffer_ms: 30,
                ..RawVoiceSettings::default()
            },
            RawVoiceSettings {
                playout_buffer_ms: 0,
                ..RawVoiceSettings::default()
            },
            RawVoiceSettings {
                driver_retries: 0,
                ..RawVoiceSettings::default()
            },
            RawVoiceSettings {
                decode_state_timeout_s: f64::NAN,
                ..RawVoiceSettings::default()
            },
            RawVoiceSettings {
                bitrate: Some(5_999),
                ..RawVoiceSettings::default()
            },
        ];
        for raw in bad {
            assert!(VoiceSettings::validate(raw).is_err(), "{raw:?}");
        }
    }
}
