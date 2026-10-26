//! Mixed sources (W3, §8.6): one `MixBus` per distinct filter, shared by every lane with an equal
//! filter. `mix` sums the selected users (never divides by N) and limits at −1 dBFS; `floor` mixes
//! only the user holding the floor.

use std::collections::{BTreeSet, HashSet};

use smallvec::SmallVec;

use crate::dsp::Limiter;
use crate::dsp::mix::add_into;
use crate::limits::{MIX_RATE, TICK_MS};

/// Limiter threshold of every bus (§8.6).
pub const BUS_LIMIT_DBFS: f32 = -1.0;
/// Limiter release of every bus (§8.6).
pub const BUS_RELEASE_MS: f32 = 80.0;

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

impl MixFilter {
    /// Everyone, bots ignored, `mix` policy.
    pub fn everyone() -> Self {
        MixFilter {
            include: None,
            exclude: BTreeSet::new(),
            ignore_bots: true,
            policy: MixPolicy::Mix,
        }
    }

    /// Whether `user` passes include/exclude and the bot filter.
    pub fn selects(&self, user: u64, bots: &HashSet<u64>) -> bool {
        self.include.as_ref().is_none_or(|inc| inc.contains(&user))
            && !self.exclude.contains(&user)
            && !(self.ignore_bots && bots.contains(&user))
    }

    /// Whether audio of SSRCs without a user mapping is mixed in (`include is None`, `mix`).
    pub fn takes_unmapped(&self) -> bool {
        self.include.is_none() && self.policy == MixPolicy::Mix
    }
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

/// A mix bus: limiter and floor state (the frame itself is the caller's `out`).
#[derive(Debug)]
pub struct MixBus {
    filter: MixFilter,
    limiter: Limiter,
    floor: Option<Floor>,
}

/// Mean square of a frame (floor arbitration).
fn energy(frame: &[f32; 1920]) -> f64 {
    frame.iter().map(|s| f64::from(*s) * f64::from(*s)).sum()
}

impl MixBus {
    pub fn new(f: MixFilter) -> Self {
        MixBus {
            filter: f,
            limiter: Limiter::new(MIX_RATE, BUS_LIMIT_DBFS, BUS_RELEASE_MS),
            floor: None,
        }
    }

    pub fn filter(&self) -> &MixFilter {
        &self.filter
    }

    /// The current floor holder (`floor` policy).
    pub fn floor_holder(&self) -> Option<u64> {
        self.floor.map(|f| f.holder)
    }

    /// Mixes one tick into `out` (48 kHz stereo).
    pub fn mix(
        &mut self,
        frames: &UserFrames<'_>,
        bots: &HashSet<u64>,
        out: &mut [f32; 1920],
    ) -> BusActivity {
        out.fill(0.0);
        let mut act = BusActivity::default();
        match self.filter.policy {
            MixPolicy::Mix => {
                for &(user, frame, voiced) in frames.users {
                    if !self.filter.selects(user, bots) {
                        continue;
                    }
                    add_into(out, frame);
                    if voiced {
                        act.speakers.push(user);
                        act.voiced = true;
                    }
                }
                if self.filter.takes_unmapped()
                    && let Some(un) = frames.unmapped
                {
                    add_into(out, un);
                }
            }
            MixPolicy::Floor { release_ms } => {
                let release_ticks = release_ms.div_ceil(TICK_MS).max(1);
                let voiced_selected =
                    |u: u64, v: bool| -> bool { v && self.filter.selects(u, bots) };
                if let Some(fl) = self.floor.as_mut() {
                    let holder_voiced = frames
                        .users
                        .iter()
                        .any(|&(u, _, v)| u == fl.holder && voiced_selected(u, v));
                    if holder_voiced {
                        fl.silent_ticks = 0;
                    } else {
                        fl.silent_ticks = fl.silent_ticks.saturating_add(1);
                        if fl.silent_ticks >= release_ticks {
                            self.floor = None;
                        }
                    }
                }
                if self.floor.is_none() {
                    // The first voiced user takes the floor; simultaneous starts go to the higher
                    // RMS (then the lower user id, for determinism).
                    let mut best: Option<(u64, f64)> = None;
                    for &(u, frame, v) in frames.users {
                        if !voiced_selected(u, v) {
                            continue;
                        }
                        let e = energy(frame);
                        let better = match best {
                            None => true,
                            Some((bu, be)) => e > be || (e == be && u < bu),
                        };
                        if better {
                            best = Some((u, e));
                        }
                    }
                    self.floor = best.map(|(holder, _)| Floor {
                        holder,
                        silent_ticks: 0,
                    });
                }
                if let Some(fl) = self.floor
                    && let Some(&(u, frame, v)) =
                        frames.users.iter().find(|&&(u, _, _)| u == fl.holder)
                    && self.filter.selects(u, bots)
                {
                    add_into(out, frame);
                    if v {
                        act.speakers.push(u);
                        act.voiced = true;
                    }
                }
            }
        }
        self.limiter.process(out, 2);
        act
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(v: f32) -> Box<[f32; 1920]> {
        Box::new([v; 1920])
    }

    fn filter(
        include: Option<&[u64]>,
        exclude: &[u64],
        bots: bool,
        policy: MixPolicy,
    ) -> MixFilter {
        MixFilter {
            include: include.map(|i| i.iter().copied().collect()),
            exclude: exclude.iter().copied().collect(),
            ignore_bots: bots,
            policy,
        }
    }

    #[test]
    fn include_exclude_and_bots() {
        let (a, b, c) = (frame(0.01), frame(0.02), frame(0.04));
        let un = frame(0.08);
        let users = [(1, &*a, true), (2, &*b, false), (3, &*c, true)];
        let frames = UserFrames {
            users: &users,
            unmapped: Some(&un),
        };
        let bots: HashSet<u64> = [3].into_iter().collect();
        let mut out = [0.0f32; 1920];
        let cases: [(MixFilter, f32, &[u64]); 5] = [
            (
                filter(None, &[], true, MixPolicy::Mix),
                0.01 + 0.02 + 0.08,
                &[1],
            ),
            (filter(None, &[], false, MixPolicy::Mix), 0.15, &[1, 3]),
            (filter(None, &[1], false, MixPolicy::Mix), 0.14, &[3]),
            // `include` set: unmapped audio is never mixed in.
            (filter(Some(&[1, 2]), &[], true, MixPolicy::Mix), 0.03, &[1]),
            (filter(Some(&[1, 3]), &[1], true, MixPolicy::Mix), 0.0, &[]),
        ];
        for (f, want, speakers) in cases {
            let mut bus = MixBus::new(f.clone());
            let act = bus.mix(&frames, &bots, &mut out);
            assert!((out[0] - want).abs() < 1e-6, "{f:?}: {} vs {want}", out[0]);
            assert_eq!(act.speakers.as_slice(), speakers, "{f:?}");
            assert_eq!(act.voiced, !speakers.is_empty());
        }
    }

    #[test]
    fn mix_never_divides_and_limits() {
        let loud = frame(0.6);
        let users = [(1, &*loud, true), (2, &*loud, true), (3, &*loud, true)];
        let frames = UserFrames {
            users: &users,
            unmapped: None,
        };
        let mut bus = MixBus::new(MixFilter::everyone());
        let mut out = [0.0f32; 1920];
        let limit = 10f32.powf(BUS_LIMIT_DBFS / 20.0);
        for _ in 0..5 {
            bus.mix(&frames, &HashSet::new(), &mut out);
            assert!(out.iter().all(|s| s.abs() <= limit + 1e-4), "{}", out[0]);
        }
        // One quiet speaker passes at unity (no 1/N scaling).
        let quiet = frame(0.1);
        let one = [(1, &*quiet, true)];
        let mut bus = MixBus::new(MixFilter::everyone());
        bus.mix(
            &UserFrames {
                users: &one,
                unmapped: None,
            },
            &HashSet::new(),
            &mut out,
        );
        assert!((out[100] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn floor_hold_and_release() {
        let f = filter(None, &[], true, MixPolicy::Floor { release_ms: 100 }); // 5 ticks
        let mut bus = MixBus::new(f);
        let (a, b) = (frame(0.1), frame(0.3));
        let mut out = [0.0f32; 1920];
        let bots = HashSet::new();
        let run = |bus: &mut MixBus, va: bool, vb: bool, out: &mut [f32; 1920]| {
            let users = [(1, &*a, va), (2, &*b, vb)];
            bus.mix(
                &UserFrames {
                    users: &users,
                    unmapped: None,
                },
                &bots,
                out,
            )
        };
        // Simultaneous start: the louder user (2) wins.
        let act = run(&mut bus, true, true, &mut out);
        assert_eq!(bus.floor_holder(), Some(2));
        assert_eq!(act.speakers.as_slice(), &[2]);
        assert!((out[0] - 0.3).abs() < 1e-6);
        // User 2 goes quiet while 1 talks: 2 keeps the floor for 4 more silent ticks.
        for _ in 0..4 {
            let act = run(&mut bus, true, false, &mut out);
            assert_eq!(bus.floor_holder(), Some(2));
            assert!(act.speakers.is_empty());
            assert!(!act.voiced);
        }
        // Fifth silent tick: released, and user 1 takes over in the same tick.
        let act = run(&mut bus, true, false, &mut out);
        assert_eq!(bus.floor_holder(), Some(1));
        assert_eq!(act.speakers.as_slice(), &[1]);
        assert!((out[0] - 0.1).abs() < 1e-6, "only the holder is mixed");
        // Nobody voiced long enough: the floor becomes free.
        for _ in 0..5 {
            run(&mut bus, false, false, &mut out);
        }
        assert_eq!(bus.floor_holder(), None);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn floor_ignores_unselected_users() {
        let f = filter(Some(&[1]), &[], true, MixPolicy::Floor { release_ms: 600 });
        let mut bus = MixBus::new(f);
        let (a, b) = (frame(0.1), frame(0.3));
        let users = [(1, &*a, false), (2, &*b, true)];
        let mut out = [0.0f32; 1920];
        let act = bus.mix(
            &UserFrames {
                users: &users,
                unmapped: Some(&b),
            },
            &HashSet::new(),
            &mut out,
        );
        assert_eq!(bus.floor_holder(), None);
        assert!(!act.voiced);
        assert!(out.iter().all(|s| *s == 0.0));
    }
}
