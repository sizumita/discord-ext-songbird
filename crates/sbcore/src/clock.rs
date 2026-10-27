//! Time source abstraction: real time in production, a manual clock in deterministic tests (§13.2).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Where "now" comes from.
#[derive(Clone, Debug, Default)]
pub enum Clock {
    #[default]
    Real,
    Manual(Arc<ManualClock>),
}

impl Clock {
    pub fn now(&self) -> Instant {
        match self {
            Clock::Real => Instant::now(),
            Clock::Manual(m) => m.now(),
        }
    }

    /// A manual clock and a handle to advance it.
    pub fn manual() -> (Clock, Arc<ManualClock>) {
        let m = ManualClock::new();
        (Clock::Manual(m.clone()), m)
    }
}

/// A clock that only moves when [`ManualClock::advance`] is called.
#[derive(Debug)]
pub struct ManualClock {
    base: Instant,
    offset_us: AtomicU64,
}

impl ManualClock {
    pub fn new() -> Arc<Self> {
        Arc::new(ManualClock {
            base: Instant::now(),
            offset_us: AtomicU64::new(0),
        })
    }

    /// Moves the clock forward by `d` (saturating).
    pub fn advance(&self, d: Duration) {
        let us = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        let mut cur = self.offset_us.load(Ordering::Acquire);
        loop {
            match self.offset_us.compare_exchange_weak(
                cur,
                cur.saturating_add(us),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }

    pub fn now(&self) -> Instant {
        let off = Duration::from_micros(self.offset_us.load(Ordering::Acquire));
        self.base.checked_add(off).unwrap_or(self.base)
    }

    /// Total time advanced since creation.
    pub fn elapsed(&self) -> Duration {
        Duration::from_micros(self.offset_us.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_when_advanced() {
        let (clock, handle) = Clock::manual();
        let t0 = clock.now();
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(clock.now(), t0);
        handle.advance(Duration::from_millis(20));
        assert_eq!(clock.now() - t0, Duration::from_millis(20));
        assert_eq!(handle.elapsed(), Duration::from_millis(20));
        let c2 = clock.clone();
        handle.advance(Duration::from_micros(5));
        assert_eq!(c2.now() - t0, Duration::from_micros(20_005));
    }

    #[test]
    fn advance_saturates() {
        let m = ManualClock::new();
        m.advance(Duration::MAX);
        m.advance(Duration::MAX);
        assert_eq!(m.elapsed(), Duration::from_micros(u64::MAX));
        let _ = m.now();
    }

    #[test]
    fn real_clock_moves() {
        let c = Clock::default();
        let a = c.now();
        std::thread::sleep(Duration::from_millis(1));
        assert!(c.now() > a);
    }
}
