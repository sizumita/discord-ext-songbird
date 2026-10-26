//! Connection & control (W2, §4, §6, §12.3).
//!
//! * [`settings`]: validated per-connection settings.
//! * [`gateway`]: the pure connection state machine (§4.5).
//! * [`driver`]: sole ownership of the songbird `Driver` (§4.8).
//! * [`dispatch`]: the single per-connection songbird event handler (§8.2).
//! * [`worker`]: the per-connection ConnWorker task (receive DSP, housekeeping, timers).
//! * [`ops`]: asynchronous track operations (`seek`, `info`).
//! * [`shared`]: [`ConnShared`], the synchronous facade the binding calls.
//! * [`registry`]: the process-wide registry of live connections (`_shutdown_all`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[cfg(feature = "test-hooks")]
mod detached;
pub mod dispatch;
pub mod driver;
pub mod gateway;
pub mod ops;
pub mod registry;
pub mod settings;
pub mod shared;
pub mod worker;

#[cfg(feature = "test-hooks")]
pub use detached::take_parked_live_input;
pub use registry::{registry_snapshot, shutdown_all};
pub use settings::{ConnParams, RawVoiceSettings, VoiceSettings};
pub use shared::ConnShared;

/// Serialises the tests that create connections, dispatchers or drivers (they assert the
/// process-wide live counters and call `shutdown_all`).
#[cfg(test)]
pub(crate) static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Process-wide reference for [`LogLimiter`].
fn log_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Rate limit for repeated log lines (atomics only; any thread): at most one line per `period_s`.
#[derive(Debug)]
pub(crate) struct LogLimiter {
    /// Seconds since `log_epoch()` of the last allowed line, plus one (0 = never).
    last: AtomicU64,
    period_s: u64,
}

impl LogLimiter {
    pub(crate) const fn new(period_s: u64) -> Self {
        LogLimiter {
            last: AtomicU64::new(0),
            period_s,
        }
    }

    /// Once per site per minute (§5.7).
    pub(crate) const fn per_minute() -> Self {
        Self::new(60)
    }

    /// Whether a line may be logged now.
    pub(crate) fn allow(&self) -> bool {
        let now = log_epoch().elapsed().as_secs().saturating_add(1);
        let last = self.last.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < self.period_s {
            return false;
        }
        self.last
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_limiter_allows_once_per_period() {
        let l = LogLimiter::per_minute();
        assert!(l.allow());
        assert!(!l.allow());
        let every = LogLimiter::new(0);
        assert!(every.allow());
        assert!(every.allow());
    }
}
