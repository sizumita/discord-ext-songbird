//! The process-wide registry of live connections (W2, §4.10): `_shutdown_all()` at `atexit` closes
//! every connection with `ProcessExit`. Holds only `Weak` references; nothing runs under the lock.

use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use super::shared::ConnShared;
use crate::error::CloseReason;

/// L10: pruned on insert.
static REGISTRY: Mutex<Vec<Weak<ConnShared>>> = Mutex::new(Vec::new());

/// Adds a connection (dead entries are pruned first).
pub(crate) fn register(conn: &Arc<ConnShared>) {
    let mut reg = REGISTRY.lock();
    reg.retain(|w| w.strong_count() > 0);
    reg.push(Arc::downgrade(conn));
}

/// Removes a connection (T9). Collects under the lock; nothing else runs under it.
pub(crate) fn unregister(conn: &ConnShared) {
    let removed: Vec<Weak<ConnShared>> = {
        let mut reg = REGISTRY.lock();
        let (gone, keep): (Vec<_>, Vec<_>) = reg
            .drain(..)
            .partition(|w| std::ptr::eq(w.as_ptr(), conn) || w.strong_count() == 0);
        *reg = keep;
        gone
    };
    drop(removed);
}

/// Every live connection (lock, upgrade, unlock).
pub fn registry_snapshot() -> Vec<Arc<ConnShared>> {
    let reg = REGISTRY.lock();
    reg.iter().filter_map(Weak::upgrade).collect()
}

/// Closes every live connection with `reason`, outside the registry lock (§4.10).
pub fn shutdown_all(reason: CloseReason) {
    for conn in registry_snapshot() {
        conn.close(reason);
    }
}

/// Test hook: a thread holds the registry lock for `d`; returns once it is held (a fork during the
/// hold copies the lock locked into the child, as a fork racing `register`/`unregister` would).
#[cfg(feature = "test-hooks")]
pub(crate) fn hold_lock_for(d: std::time::Duration) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _g = REGISTRY.lock();
        let _ = tx.send(());
        std::thread::sleep(d);
    });
    let _ = rx.recv();
}
