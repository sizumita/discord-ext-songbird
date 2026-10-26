//! Locking helpers and the shutdown flag (§5.2, §5.3).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, MutexGuard};
use tokio::sync::Notify;

use crate::runtime::{Role, role};

/// A blocking `lock()` that asserts (debug builds) it is never taken on a mixer thread.
///
/// The mixer only ever uses `try_lock` (§5.3). This helper takes a mutex the caller already owns a
/// reference to; it does not expose any sbcore-internal lock (I7).
#[inline]
pub fn lock_checked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    debug_assert_ne!(role(), Role::Mixer, "blocking lock() on a mixer thread");
    m.lock()
}

/// A one-shot shutdown flag that async tasks can wait on (ConnWorker exit, §4.7 T6).
#[derive(Debug, Default)]
pub struct Shutdown {
    flag: AtomicBool,
    notify: Notify,
}

impl Shutdown {
    pub fn new() -> Arc<Self> {
        Arc::new(Shutdown::default())
    }

    /// Sets the flag and wakes every waiter. Idempotent; never blocks.
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Completes once [`Shutdown::trigger`] has been called (immediately if it already was).
    pub async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Register before checking the flag so a concurrent trigger cannot be missed.
            notified.as_mut().enable();
            if self.is_set() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn wait_returns_after_trigger() {
        let s = Shutdown::new();
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.wait().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!s.is_set());
        s.trigger();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap();
        // Already set: returns at once, and trigger is idempotent.
        s.trigger();
        tokio::time::timeout(Duration::from_secs(1), s.wait())
            .await
            .unwrap();
    }

    #[test]
    fn trigger_from_another_thread_wakes_waiter() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let s = Shutdown::new();
        let s2 = s.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            s2.trigger();
        });
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), s.wait())
                .await
                .unwrap()
        });
        t.join().unwrap();
    }

    #[test]
    fn lock_checked_locks() {
        let m = Mutex::new(5);
        *lock_checked(&m) += 1;
        assert_eq!(*m.lock(), 6);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn lock_checked_asserts_on_mixer_role() {
        let r = std::thread::spawn(|| {
            crate::runtime::set_role(Role::Mixer);
            let m = Mutex::new(());
            let _g = lock_checked(&m);
        })
        .join();
        assert!(r.is_err(), "lock_checked must assert on a mixer thread");
    }
}
