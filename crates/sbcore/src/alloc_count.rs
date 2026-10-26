//! Per-thread allocation counting (§13.1): a global allocator that counts allocations on the
//! calling thread, and only while a [`CountGuard`] is alive there. Compiled for `cfg(test)` and the
//! `test-hooks` feature only; `native._testing.alloc_count()` reads it.
//!
//! A process-wide counter would flake: `cargo test` runs tests in parallel, and the NET/RX runtimes
//! allocate concurrently.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

#[inline]
fn record() {
    let active = ACTIVE.try_with(Cell::get).unwrap_or(false);
    if active {
        let _ = COUNT.try_with(|c| c.set(c.get().wrapping_add(1)));
    }
}

/// The counting allocator (delegates to [`System`]).
pub struct CountingAlloc;

// SAFETY: every method delegates to `System` with the caller's arguments; the bookkeeping only
// touches const-initialised, destructor-free thread-locals (no allocation, no panics).
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record();
        // SAFETY: forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Counts allocations on this thread while alive (nestable).
#[derive(Debug)]
pub struct CountGuard {
    prev: bool,
}

impl CountGuard {
    pub fn new() -> Self {
        let prev = ACTIVE.try_with(|a| a.replace(true)).unwrap_or(false);
        CountGuard { prev }
    }
}

impl Default for CountGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CountGuard {
    fn drop(&mut self) {
        let _ = ACTIVE.try_with(|a| a.set(self.prev));
    }
}

/// Allocations counted on the calling thread so far (inside guards).
pub fn count() -> u64 {
    COUNT.try_with(Cell::get).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_only_inside_guard_on_this_thread() {
        let before = count();
        let v: Vec<u8> = Vec::with_capacity(10);
        assert_eq!(count(), before);
        let g = CountGuard::new();
        let w: Vec<u8> = Vec::with_capacity(10);
        assert_eq!(count(), before + 1);
        std::thread::spawn(|| {
            let _x: Vec<u8> = Vec::with_capacity(10);
        })
        .join()
        .unwrap();
        drop(g);
        let _z: Vec<u8> = Vec::with_capacity(10);
        assert!(count() > before);
        drop((v, w));
    }
}
