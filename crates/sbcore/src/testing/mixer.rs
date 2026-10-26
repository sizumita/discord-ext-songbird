//! `MixerHarness` (§13.2): takes ownership of a detached playback's reader/decoder pair and drives
//! it exactly as songbird's `mix_symph_indiv` does (`next_packet` → `decode`), asserting 960 frames.
//!
//! songbird semantics that are reproduced:
//!
//! * a **paused** track is not pulled at all (`mixer/mod.rs:830`): live sources start paused
//!   (§4.9), and the fake control's last `pause`/`play` tells whether the track is paused;
//! * a packet whose `track_id` differs from the input's is skipped;
//! * any `Err` from `next_packet` or `decode` ends the track: the pair is dropped, which runs the
//!   real `DoneGuard` path;
//! * a track whose flags resolved (`stop()` → `End(Stop)`) is disposed of before its next pull.
//!
//! Only `next_packet` + `decode` + the copy into a pre-reserved buffer run inside the per-thread
//! allocation-counting guard, so `alloc_count()` measures the mixer path alone.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use parking_lot::Mutex;
use songbird::input::core::audio::{AudioBufferRef, Signal};
use songbird::input::{Input, LiveInput, Parsed};

use super::registry;
use crate::alloc_count::CountGuard;
use crate::error::CoreError;
use crate::limits::{self, FRAMES_PER_TICK_48K, MIX_RATE};
use crate::runtime::{Role, role, set_role};
use crate::tracks::flags::TrackFlags;
use crate::tx::LiveSource;
use crate::tx::queue::PcmQueue;

/// Most ticks one `pull` may return (60 s; bounds the output buffer).
pub const MAX_PULL_TICKS: usize = 3_000;
/// Packets with a foreign `track_id` skipped per tick before the harness gives up.
const MAX_SKIPPED_PACKETS: usize = 8;

struct State {
    pair: Option<Parsed>,
    flags: Arc<TrackFlags>,
    channels: usize,
    pulls: u64,
    end_error: Option<&'static str>,
}

/// Drives one playback's songbird input on the calling thread (`Role::Harness`).
pub struct MixerHarness {
    source_id: u32,
    state: Mutex<State>,
}

/// Outcome of one songbird-style pull.
enum Tick {
    Audio,
    Ended(Option<&'static str>),
}

impl MixerHarness {
    /// Takes the parked input of `playback` (detached connections only).
    pub fn take(playback: &Arc<PcmQueue>) -> Result<Arc<Self>, CoreError> {
        Self::take_source(LiveSource::id(playback.as_ref()))
    }

    /// Takes the parked input of the live source with `LiveSource::id() == source_id`.
    pub fn take_source(source_id: u32) -> Result<Arc<Self>, CoreError> {
        let Some((conn, flags, input)) = registry::take_parked(source_id) else {
            return Err(CoreError::Track(
                "no parked input for this source (detached connections only; already taken?)"
                    .into(),
            ));
        };
        let parsed = match input {
            Input::Live(LiveInput::Parsed(p), _) => p,
            other => {
                registry::park_keyed(conn, Some(source_id), flags, other);
                return Err(CoreError::Internal(
                    "testing::MixerHarness (the parked input is not a parsed live input)",
                ));
            }
        };
        let channels = parsed
            .decoder
            .codec_params()
            .channels
            .map_or(2, |c| c.count())
            .clamp(1, 2);
        Ok(Arc::new(MixerHarness {
            source_id,
            state: Mutex::new(State {
                pair: Some(parsed),
                flags,
                channels,
                pulls: 0,
                end_error: None,
            }),
        }))
    }

    /// Pulls `ticks` ticks; appends 960 × ch interleaved 48 kHz `f32` samples per tick to `out`
    /// (zeros while the track is paused or after it ended, which songbird would not mix at all).
    /// Dropping the pair (EOF, `stop()`, `close()`) runs the real `DoneGuard` path.
    pub fn pull(&self, ticks: usize, out: &mut Vec<f32>) -> Result<(), CoreError> {
        limits::check("ticks", ticks, 0..=MAX_PULL_TICKS)?;
        let mut guard = self.state.lock();
        let st: &mut State = &mut guard;
        let channels = st.channels;
        let per_tick = FRAMES_PER_TICK_48K * channels;
        out.try_reserve(ticks.saturating_mul(per_tick))?;
        let prev_role = role();
        set_role(Role::Harness);
        let mut result = Ok(());
        for _ in 0..ticks {
            if st.pair.is_some() && st.flags.ended().is_some() {
                // songbird disposes of a stopped/ended track before the next mix.
                drop(st.pair.take());
            }
            let paused = st.pair.is_some() && is_paused(&st.flags);
            let before = out.len();
            let Some(pair) = st.pair.as_mut().filter(|_| !paused) else {
                out.resize(before + per_tick, 0.0);
                continue;
            };
            let tick = catch_unwind(AssertUnwindSafe(|| {
                let _count = CountGuard::new();
                mix_one(pair, channels, out)
            }));
            match tick {
                Ok(Ok(Tick::Audio)) => st.pulls += 1,
                Ok(Ok(Tick::Ended(err))) => {
                    st.pulls += 1;
                    st.end_error = err;
                    drop(st.pair.take());
                    out.truncate(before);
                    out.resize(before + per_tick, 0.0);
                }
                Ok(Err(e)) => {
                    drop(st.pair.take());
                    out.truncate(before);
                    result = Err(e);
                    break;
                }
                Err(_) => {
                    drop(st.pair.take());
                    out.truncate(before);
                    result = Err(CoreError::Internal(
                        "testing::MixerHarness (a mixer callback panicked)",
                    ));
                    break;
                }
            }
        }
        set_role(prev_role);
        result
    }

    /// Pulls that reached the reader (paused and ended ticks are not pulls).
    pub fn pulls(&self) -> u64 {
        self.state.lock().pulls
    }

    /// Whether the track ended (the pair was dropped).
    pub fn ended(&self) -> bool {
        self.state.lock().pair.is_none()
    }

    /// Why the track ended with an error (`decode` failed), if it did.
    pub fn end_error(&self) -> Option<&'static str> {
        self.state.lock().end_error
    }

    /// Output channels (1 or 2).
    pub fn channels(&self) -> usize {
        self.state.lock().channels
    }

    /// The live-source id this harness drives.
    pub fn source_id(&self) -> u32 {
        self.source_id
    }

    /// Drops the pair now (as songbird's `SetTrack(None)` would).
    pub fn drop_track(&self) {
        let pair = self.state.lock().pair.take();
        drop(pair);
    }
}

impl Drop for MixerHarness {
    fn drop(&mut self) {
        drop(self.state.get_mut().pair.take());
    }
}

/// Live sources start paused (§4.9); afterwards the last `pause`/`play` of the fake control wins
/// (tracked on every call, not read from the bounded log). No fake control: paused.
fn is_paused(flags: &Arc<TrackFlags>) -> bool {
    registry::fake_paused(flags).unwrap_or(true)
}

/// One `mix_symph_indiv` tick: `next_packet` → (skip foreign track ids) → `decode` → copy.
fn mix_one(pair: &mut Parsed, channels: usize, out: &mut Vec<f32>) -> Result<Tick, CoreError> {
    for _ in 0..MAX_SKIPPED_PACKETS {
        let pkt = match pair.format.next_packet() {
            Ok(p) => p,
            Err(_) => return Ok(Tick::Ended(None)),
        };
        if pkt.track_id() != pair.track_id {
            continue;
        }
        let decoded = match pair.decoder.decode(&pkt) {
            Ok(d) => d,
            Err(_) => return Ok(Tick::Ended(Some("decode() returned an error"))),
        };
        let AudioBufferRef::F32(buf) = decoded else {
            return Err(CoreError::Internal(
                "testing::MixerHarness (decode() returned a non-f32 buffer)",
            ));
        };
        if buf.frames() != FRAMES_PER_TICK_48K || buf.spec().rate != MIX_RATE {
            return Err(CoreError::Internal(
                "testing::MixerHarness (decode() did not return exactly 960 frames at 48 kHz)",
            ));
        }
        let planes = buf.planes();
        let planes = planes.planes();
        for frame in 0..FRAMES_PER_TICK_48K {
            for ch in 0..channels {
                let s = planes
                    .get(ch)
                    .or_else(|| planes.first())
                    .and_then(|p| p.get(frame))
                    .copied()
                    .unwrap_or(0.0);
                out.push(s);
            }
        }
        return Ok(Tick::Audio);
    }
    Err(CoreError::Internal(
        "testing::MixerHarness (too many packets for another track id)",
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::tracks::flags::TrackKind;

    #[test]
    fn take_without_parked_input_is_a_track_error() {
        assert!(matches!(
            MixerHarness::take_source(u32::MAX - 7),
            Err(CoreError::Track(_))
        ));
    }

    #[test]
    fn take_rejects_a_non_parsed_input_and_keeps_it_parked() {
        let flags = Arc::new(TrackFlags::new(
            1,
            TrackKind::Playback,
            Arc::new(AtomicBool::new(false)),
        ));
        let key = 0x5000_usize;
        registry::park_keyed(key, Some(u32::MAX - 8), flags, Input::from(vec![0u8; 4]));
        assert!(matches!(
            MixerHarness::take_source(u32::MAX - 8),
            Err(CoreError::Internal(_))
        ));
        assert_eq!(registry::parked_count(key), 1);
        registry::sweep(key, true);
    }

    #[test]
    fn paused_state_follows_the_fake_control() {
        let flags = Arc::new(TrackFlags::new(
            2,
            TrackKind::Playback,
            Arc::new(AtomicBool::new(false)),
        ));
        assert!(is_paused(&flags)); // starts paused, no control yet
        let ctl = registry::fake_track_control(&flags);
        assert!(is_paused(&flags));
        ctl.play();
        assert!(!is_paused(&flags));
        ctl.pause();
        assert!(is_paused(&flags));
        ctl.play();
        ctl.stop();
        assert!(!is_paused(&flags));
    }
}
