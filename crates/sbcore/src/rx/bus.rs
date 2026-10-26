//! Mixed sources (W3, §8.6): one `MixBus` per distinct filter, shared by every lane with an equal
//! filter. `mix` sums the selected users (never divides by N) and limits at −1 dBFS; `floor` mixes
//! only the user holding the floor.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::{BTreeSet, HashSet};

use smallvec::SmallVec;

use crate::dsp::Limiter;
use crate::limits::MIX_RATE;

/// How a bus combines users.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MixPolicy {
    Mix,
    /// The first voiced user holds the floor until silent for `release_ms`.
    Floor {
        release_ms: u32,
    },
}

/// Which users a mixed stream contains.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct MixFilter {
    /// `None` = everyone (and unmapped audio).
    pub include: Option<BTreeSet<u64>>,
    pub exclude: BTreeSet<u64>,
    pub ignore_bots: bool,
    pub policy: MixPolicy,
}

/// One tick of per-user frames (48 kHz stereo f32, 960 frames).
pub struct UserFrames<'a> {
    /// `(user, frame, voiced)`.
    pub users: &'a [(u64, &'a [f32; 1920], bool)],
    /// Audio of SSRCs without a user mapping.
    pub unmapped: Option<&'a [f32; 1920]>,
}

/// Who contributed to a mixed tick.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusActivity {
    pub speakers: SmallVec<[u64; 8]>,
    pub voiced: bool,
}

/// Floor holder of a `floor` bus.
#[derive(Clone, Copy, Debug)]
struct Floor {
    holder: u64,
    silent_ticks: u32,
}

/// A mix bus: frame scratch, limiter and floor state.
#[derive(Debug)]
pub struct MixBus {
    filter: MixFilter,
    limiter: Limiter,
    floor: Option<Floor>,
}

impl MixBus {
    pub fn new(f: MixFilter) -> Self {
        MixBus {
            filter: f,
            limiter: Limiter::new(MIX_RATE, -1.0, 80.0),
            floor: None,
        }
    }

    /// Mixes one tick into `out` (48 kHz stereo).
    pub fn mix(
        &mut self,
        frames: &UserFrames<'_>,
        bots: &HashSet<u64>,
        out: &mut [f32; 1920],
    ) -> BusActivity {
        out.fill(0.0);
        BusActivity::default()
    }
}
