//! Race-free per-connection capacity checks (I6) for callers on many threads (3.14t).
//!
//! A plain "count, then insert" check lets N threads that all see `max - 1` items insert N items.
//! An [`Admission`] counts the visible items **plus every in-flight reservation**, and a
//! [`Reservation`] is released only after its item became visible, so concurrent admissions can
//! never exceed the bound. Lock-free: the visible count is taken with the item registry's own leaf
//! lock, never nested under another lock.
//!
//! Why this is sound: reservations are taken with `SeqCst` read-modify-writes, so of any two
//! concurrent admissions the later one sees the earlier one's reservation (still held), or — if
//! it was already released — the earlier item itself (released after it became visible; the
//! release/acquire pair on the counter orders the two). A concurrent admission may therefore be
//! counted twice (a spurious, transient `LimitExceeded` near the bound), never zero times.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::CoreError;
use crate::limits::check;

/// In-flight admissions of one limit.
#[derive(Debug, Default)]
pub(crate) struct Admission(AtomicUsize);

/// One in-flight admission; drop it once the admitted item is visible to `visible()`.
#[derive(Debug)]
pub(crate) struct Reservation<'a>(&'a AtomicUsize);

impl Admission {
    pub(crate) const fn new() -> Self {
        Admission(AtomicUsize::new(0))
    }

    /// Reserves one slot of `max`: `LimitExceeded` (`what`) unless the items `visible()` returns
    /// (computed after the reservation is published) plus every other in-flight reservation plus
    /// this one fit.
    pub(crate) fn reserve(
        &self,
        what: &'static str,
        max: usize,
        visible: impl FnOnce() -> usize,
    ) -> Result<Reservation<'_>, CoreError> {
        let others = self.0.fetch_add(1, Ordering::SeqCst);
        let r = Reservation(&self.0);
        check(
            what,
            visible().saturating_add(others).saturating_add(1),
            1..=max,
        )?;
        Ok(r)
    }

    /// Reservations in flight (tests and diagnostics).
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Barrier;

    #[test]
    fn reservations_are_released_on_error_and_drop() {
        let a = Admission::new();
        let r = a.reserve("x", 2, || 0).unwrap();
        assert_eq!(a.in_flight(), 1);
        assert!(matches!(a.reserve("x", 2, || 1), Err(CoreError::Limit(_))));
        assert_eq!(a.in_flight(), 1, "a refused reservation is released");
        drop(r);
        assert_eq!(a.in_flight(), 0);
        assert!(a.reserve("x", 2, || 1).is_ok());
    }

    /// Many threads race to admit items against a bound; the items they make visible never
    /// exceed it (a plain check-then-insert admits one item per thread).
    #[test]
    fn concurrent_admissions_never_exceed_the_bound() {
        const MAX: usize = 8;
        const THREADS: usize = 16;
        for _ in 0..50 {
            let adm = Arc::new(Admission::new());
            let items = Arc::new(parking_lot::Mutex::new(0usize));
            let start = Arc::new(Barrier::new(THREADS));
            let threads: Vec<_> = (0..THREADS)
                .map(|_| {
                    let (adm, items, start) = (adm.clone(), items.clone(), start.clone());
                    std::thread::spawn(move || {
                        start.wait();
                        if let Ok(r) = adm.reserve("items", MAX, || *items.lock()) {
                            std::thread::yield_now();
                            *items.lock() += 1; // the item becomes visible…
                            drop(r); // …before the reservation is released
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            let n = *items.lock();
            assert!(n <= MAX, "{n} items admitted (max {MAX})");
            assert!(n >= 1);
            assert_eq!(adm.in_flight(), 0);
        }
    }
}
