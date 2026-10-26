//! Per-user speaking state machine (W3, §8.5): Idle → Speaking on the first voiced tick, Speaking →
//! Idle after the hangover; up to 3 concealed ticks inside a spurt still count as voiced.
//!
//! Only speaking users are stored (an entry is removed when its user goes idle), so the map is
//! bounded by the number of concurrently speaking users.

use std::collections::HashMap;
use std::time::Instant;

use crate::events::EventData;
use crate::limits::TICK_MS;

/// Consecutive concealed (lost) ticks inside a talk spurt that still count as voiced.
pub const MAX_CONCEALED_RUN: u8 = 3;

/// Speaking state of one speaking user.
#[derive(Clone, Copy, Debug, Default)]
struct UserSpeech {
    started_tick: u64,
    last_voiced_tick: u64,
    concealed_run: u8,
    /// Last tick this user was updated (so `end_tick` advances only the others).
    last_tick: u64,
}

/// Speaking FSMs of every user.
#[derive(Debug)]
pub struct SpeakingDetector {
    hangover_ticks: u32,
    users: HashMap<u64, UserSpeech>,
}

fn ms(ticks: u64) -> u64 {
    ticks.saturating_mul(u64::from(TICK_MS))
}

impl SpeakingDetector {
    /// `hangover_ticks` non-voiced ticks end a spurt (at least 1).
    pub fn new(hangover_ticks: u32) -> Self {
        SpeakingDetector {
            hangover_ticks: hangover_ticks.max(1),
            users: HashMap::new(),
        }
    }

    /// Advances `user`'s FSM by one tick; emits `SpeakingStarted` / `SpeakingStopped`.
    ///
    /// `voiced`: a real, non-silent packet (and, if configured, at or above the RMS threshold).
    /// `concealed`: the tick was lost (packet-loss concealment); it bridges a spurt for at most
    /// [`MAX_CONCEALED_RUN`] consecutive ticks.
    pub fn on_tick(
        &mut self,
        user: u64,
        voiced: bool,
        concealed: bool,
        tick: u64,
        now: Instant,
        out: &mut Vec<EventData>,
    ) {
        if voiced {
            let e = self.users.entry(user).or_insert_with(|| {
                out.push(EventData::SpeakingStarted {
                    user_id: user,
                    timeline_ms: ms(tick),
                    captured_at: now,
                });
                UserSpeech {
                    started_tick: tick,
                    ..UserSpeech::default()
                }
            });
            e.last_voiced_tick = tick;
            e.concealed_run = 0;
            e.last_tick = tick;
            return;
        }
        let Some(e) = self.users.get_mut(&user) else {
            return; // idle and not voiced: nothing to track
        };
        e.last_tick = tick;
        if concealed && e.concealed_run < MAX_CONCEALED_RUN {
            e.concealed_run += 1;
            e.last_voiced_tick = tick;
            return;
        }
        if !concealed {
            e.concealed_run = 0;
        }
        let e = *e;
        if self.expired(&e, tick) {
            self.users.remove(&user);
            out.push(Self::stopped(user, &e));
        }
    }

    /// Ends the tick: every speaking user not updated in `tick` advances as non-voiced (their
    /// hangover runs), so silence and synthetic ticks stop spurts too.
    pub fn end_tick(&mut self, tick: u64, out: &mut Vec<EventData>) {
        let hangover = u64::from(self.hangover_ticks);
        self.users.retain(|&user, e| {
            if e.last_tick == tick {
                return true;
            }
            e.last_tick = tick;
            e.concealed_run = 0;
            if tick.saturating_sub(e.last_voiced_tick) >= hangover {
                out.push(Self::stopped(user, e));
                false
            } else {
                true
            }
        });
    }

    /// `ClientDisconnect`: `SpeakingStopped` first if the user was speaking, then `UserLeft`.
    pub fn user_left(&mut self, user: u64, tick: u64, out: &mut Vec<EventData>) {
        let _ = tick;
        if let Some(e) = self.users.remove(&user) {
            out.push(Self::stopped(user, &e));
        }
        out.push(EventData::UserLeft { user_id: user });
    }

    /// Whether `user` is inside a talk spurt (including its hangover).
    pub fn is_speaking(&self, user: u64) -> bool {
        self.users.contains_key(&user)
    }

    /// Users currently speaking.
    pub fn speaking_users(&self) -> usize {
        self.users.len()
    }

    fn expired(&self, e: &UserSpeech, tick: u64) -> bool {
        tick.saturating_sub(e.last_voiced_tick) >= u64::from(self.hangover_ticks)
    }

    /// `SpeakingStopped` at the end of the last voiced tick.
    fn stopped(user: u64, e: &UserSpeech) -> EventData {
        let end = e.last_voiced_tick.saturating_add(1);
        EventData::SpeakingStopped {
            user_id: user,
            timeline_ms: ms(end),
            duration_ms: ms(end.saturating_sub(e.started_tick)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(ev: &[EventData]) -> Vec<String> {
        ev.iter()
            .map(|e| match e {
                EventData::SpeakingStarted {
                    user_id,
                    timeline_ms,
                    ..
                } => {
                    format!("start {user_id}@{timeline_ms}")
                }
                EventData::SpeakingStopped {
                    user_id,
                    timeline_ms,
                    duration_ms,
                } => format!("stop {user_id}@{timeline_ms}+{duration_ms}"),
                EventData::UserLeft { user_id } => format!("left {user_id}"),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn starts_on_first_voiced_tick_and_stops_after_hangover() {
        let now = Instant::now();
        let mut d = SpeakingDetector::new(10); // 200 ms
        let mut ev = Vec::new();
        d.on_tick(7, false, false, 0, now, &mut ev);
        d.end_tick(0, &mut ev);
        assert!(ev.is_empty());
        for t in 1..=5 {
            d.on_tick(7, true, false, t, now, &mut ev);
            d.end_tick(t, &mut ev);
        }
        assert_eq!(kinds(&ev), ["start 7@20"]);
        assert!(d.is_speaking(7));
        ev.clear();
        // Ticks 6..=14: still inside the 10-tick hangover (silence arrives as on_tick or not at all).
        for t in 6..=14 {
            if t % 2 == 0 {
                d.on_tick(7, false, false, t, now, &mut ev);
            }
            d.end_tick(t, &mut ev);
        }
        assert!(ev.is_empty(), "{:?}", kinds(&ev));
        d.end_tick(15, &mut ev);
        // Voiced ticks 1..=5: stops at the end of tick 5 (120 ms), lasting 100 ms.
        assert_eq!(kinds(&ev), ["stop 7@120+100"]);
        assert!(!d.is_speaking(7));
        assert_eq!(d.speaking_users(), 0);
    }

    #[test]
    fn silent_frames_are_not_voiced_input() {
        // The pipeline passes voiced=false for SILENT_FRAME packets; such ticks never start a spurt.
        let mut d = SpeakingDetector::new(3);
        let mut ev = Vec::new();
        for t in 0..20 {
            d.on_tick(1, false, false, t, Instant::now(), &mut ev);
            d.end_tick(t, &mut ev);
        }
        assert!(ev.is_empty());
    }

    #[test]
    fn up_to_three_concealed_ticks_keep_a_spurt_alive() {
        let now = Instant::now();
        let mut ev = Vec::new();
        // hangover 1 tick: any non-bridged gap would stop at once.
        let mut d = SpeakingDetector::new(1);
        d.on_tick(3, true, false, 0, now, &mut ev);
        for t in 1..=3 {
            d.on_tick(3, false, true, t, now, &mut ev);
            d.end_tick(t, &mut ev);
        }
        assert_eq!(kinds(&ev), ["start 3@0"]);
        d.on_tick(3, true, false, 4, now, &mut ev);
        assert_eq!(ev.len(), 1, "still the same spurt");
        // A fourth consecutive concealed tick no longer bridges.
        for t in 5..=8 {
            d.on_tick(3, false, true, t, now, &mut ev);
            d.end_tick(t, &mut ev);
        }
        assert_eq!(kinds(&ev), ["start 3@0", "stop 3@160+160"]);
        // Concealment never starts a spurt.
        ev.clear();
        d.on_tick(3, false, true, 20, now, &mut ev);
        assert!(ev.is_empty());
    }

    #[test]
    fn user_left_emits_stopped_then_left() {
        let now = Instant::now();
        let mut d = SpeakingDetector::new(10);
        let mut ev = Vec::new();
        d.on_tick(9, true, false, 50, now, &mut ev);
        d.on_tick(9, true, false, 51, now, &mut ev);
        d.user_left(9, 52, &mut ev);
        assert_eq!(kinds(&ev), ["start 9@1000", "stop 9@1040+40", "left 9"]);
        ev.clear();
        d.user_left(10, 60, &mut ev);
        assert_eq!(kinds(&ev), ["left 10"]);
    }

    #[test]
    fn synthetic_jumps_end_spurts() {
        let mut d = SpeakingDetector::new(10);
        let mut ev = Vec::new();
        d.on_tick(1, true, false, 0, Instant::now(), &mut ev);
        d.on_tick(2, true, false, 0, Instant::now(), &mut ev);
        d.end_tick(1_000, &mut ev);
        assert_eq!(ev.len(), 4);
        assert_eq!(d.speaking_users(), 0);
    }
}
