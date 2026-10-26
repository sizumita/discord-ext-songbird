//! Per-user speaking state machine (W3, §8.5): Idle → Speaking on the first voiced tick, Speaking →
//! Idle after the hangover; up to 3 concealed ticks inside a spurt still count as voiced.
#![allow(dead_code, unused_variables, clippy::ptr_arg)] // phase-0 skeleton

use std::collections::HashMap;
use std::time::Instant;

use crate::events::EventData;

/// Speaking state of one user.
#[derive(Clone, Copy, Debug, Default)]
struct UserSpeech {
    speaking: bool,
    started_tick: u64,
    last_voiced_tick: u64,
    concealed_run: u8,
}

/// Speaking FSMs of every user.
#[derive(Debug)]
pub struct SpeakingDetector {
    hangover_ticks: u32,
    users: HashMap<u64, UserSpeech>,
}

impl SpeakingDetector {
    pub fn new(hangover_ticks: u32) -> Self {
        SpeakingDetector {
            hangover_ticks,
            users: HashMap::new(),
        }
    }

    /// Advances `user`'s FSM by one tick; emits `SpeakingStarted` / `SpeakingStopped`.
    pub fn on_tick(
        &mut self,
        user: u64,
        voiced: bool,
        concealed: bool,
        tick: u64,
        now: Instant,
        out: &mut Vec<EventData>,
    ) {
        // phase 0: no events.
    }

    /// `ClientDisconnect`: `SpeakingStopped` first if the user was speaking, then forget them.
    pub fn user_left(&mut self, user: u64, tick: u64, out: &mut Vec<EventData>) {
        self.users.remove(&user);
    }
}
