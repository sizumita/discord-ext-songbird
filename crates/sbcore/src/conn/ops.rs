//! Asynchronous track operations (`seek`, `info`) with results polled by Python (W2, §7.10).
//!
//! Each op is a task on the RX runtime that stores its result here and rings `WAKE_OPS`; at most
//! `MAX_OPS_PER_CONN` are pending (in flight or finished but not yet taken) per connection.

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use songbird::tracks::{LoopState, PlayMode, TrackState};
use tokio::task::AbortHandle;

use crate::error::{CloseReason, CoreError};
use crate::limits::{MAX_OPS_PER_CONN, check};
use crate::runtime;
use crate::sync::lock_checked;
use crate::wake::{WAKE_OPS, WakeHub};

/// `native.TrackInfo` data.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackInfoData {
    /// `"play"`, `"pause"`, `"stop"`, `"end"` or `"errored"`.
    pub playing: &'static str,
    pub volume: f32,
    pub position_ms: u64,
    pub play_time_ms: u64,
    /// `None` = infinite.
    pub loops: Option<u32>,
}

impl TrackInfoData {
    /// Converts songbird's track state.
    pub fn from_state(s: &TrackState) -> Self {
        let playing = match s.playing {
            PlayMode::Play => "play",
            PlayMode::Pause => "pause",
            PlayMode::Stop => "stop",
            PlayMode::End => "end",
            _ => "errored",
        };
        TrackInfoData {
            playing,
            volume: s.volume,
            position_ms: u64::try_from(s.position.as_millis()).unwrap_or(u64::MAX),
            play_time_ms: u64::try_from(s.play_time.as_millis()).unwrap_or(u64::MAX),
            loops: match s.loops {
                LoopState::Infinite => None,
                LoopState::Finite(n) => Some(n.get()),
            },
        }
    }
}

/// Outcome of one op.
#[derive(Debug)]
pub enum OpResult {
    Info(TrackInfoData),
    Seeked(Duration),
    Err(CoreError),
    TimedOut,
}

/// One in-flight or finished op.
#[derive(Debug)]
struct OpSlot {
    id: u32,
    task: Option<AbortHandle>,
    result: Option<OpResult>,
}

/// Pending ops of one connection (L10).
#[derive(Debug, Default)]
pub struct OpRegistry {
    /// Shared with the op tasks (which hold only a `Weak`).
    slots: Arc<Mutex<Vec<OpSlot>>>,
    next_id: AtomicU32,
}

impl OpRegistry {
    pub fn new() -> Self {
        OpRegistry::default()
    }

    /// Spawns `fut` on `runtime::rx()`; its result is stored and `WAKE_OPS` rung. Returns the op id.
    /// `LimitExceeded` beyond `MAX_OPS_PER_CONN` pending ops.
    pub fn start<F>(&self, wake: &Arc<WakeHub>, fut: F) -> Result<u32, CoreError>
    where
        F: Future<Output = OpResult> + Send + 'static,
    {
        let id = {
            let mut slots = lock_checked(&self.slots);
            check(
                "pending_ops",
                slots.len().saturating_add(1),
                1..=MAX_OPS_PER_CONN,
            )?;
            let id = loop {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                if id != 0 && !slots.iter().any(|s| s.id == id) {
                    break id;
                }
            };
            slots.push(OpSlot {
                id,
                task: None,
                result: None,
            });
            id
        };
        let slots: Weak<Mutex<Vec<OpSlot>>> = Arc::downgrade(&self.slots);
        let wake_w = Arc::downgrade(wake);
        let handle = runtime::rx().spawn(async move {
            let r = fut.await;
            if let Some(slots) = slots.upgrade() {
                let mut g = slots.lock();
                if let Some(s) = g.iter_mut().find(|s| s.id == id && s.result.is_none()) {
                    s.result = Some(r);
                    s.task = None;
                }
            }
            if let Some(w) = wake_w.upgrade() {
                w.notify(WAKE_OPS);
            }
        });
        let abort = handle.abort_handle();
        let mut g = lock_checked(&self.slots);
        if let Some(s) = g.iter_mut().find(|s| s.id == id && s.result.is_none()) {
            s.task = Some(abort);
        }
        Ok(id)
    }

    /// Moves every finished result into `out`.
    pub fn take_results(&self, out: &mut Vec<(u32, OpResult)>) {
        let mut g = lock_checked(&self.slots);
        let mut i = 0;
        while i < g.len() {
            if g.get(i).is_some_and(|s| s.result.is_some()) {
                let s = g.swap_remove(i);
                if let Some(r) = s.result {
                    out.push((s.id, r));
                }
            } else {
                i += 1;
            }
        }
    }

    /// Aborts every pending op and resolves it as `ConnectionClosed` (T8).
    pub fn fail_all(&self) {
        let aborts: Vec<AbortHandle> = {
            let mut g = lock_checked(&self.slots);
            g.iter_mut()
                .filter(|s| s.result.is_none())
                .filter_map(|s| {
                    s.result = Some(OpResult::Err(CoreError::ConnectionClosed(
                        CloseReason::ConnectionClosed,
                    )));
                    s.task.take()
                })
                .collect()
        };
        for a in aborts {
            a.abort();
        }
    }

    /// Ops in flight or finished but not yet taken.
    pub fn pending(&self) -> usize {
        lock_checked(&self.slots).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake::Doorbell;
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    fn wake() -> (Arc<WakeHub>, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        (WakeHub::new(Doorbell::from_unix_stream(a).unwrap()), b)
    }

    fn wait_results(ops: &OpRegistry, n: usize) -> Vec<(u32, OpResult)> {
        let mut out = Vec::new();
        let start = Instant::now();
        while out.len() < n && start.elapsed() < Duration::from_secs(10) {
            ops.take_results(&mut out);
            std::thread::sleep(Duration::from_millis(1));
        }
        out
    }

    #[test]
    fn results_are_stored_and_rung() {
        let (w, _peer) = wake();
        let ops = OpRegistry::new();
        let a = ops
            .start(&w, async { OpResult::Seeked(Duration::from_secs(3)) })
            .unwrap();
        let b = ops.start(&w, async { OpResult::TimedOut }).unwrap();
        assert_ne!(a, b);
        let mut got = wait_results(&ops, 2);
        got.sort_by_key(|(id, _)| *id);
        assert!(
            matches!(got[..], [(x, OpResult::Seeked(d)), (y, OpResult::TimedOut)]
            if x == a && y == b && d == Duration::from_secs(3))
        );
        let mut ready = Vec::new();
        w.take_ready(&mut ready);
        assert!(ready.contains(&WAKE_OPS.0));
        assert_eq!(ops.pending(), 0);
    }

    #[test]
    fn limit_and_fail_all() {
        let (w, _peer) = wake();
        let ops = OpRegistry::new();
        for _ in 0..MAX_OPS_PER_CONN {
            ops.start(&w, std::future::pending::<OpResult>()).unwrap();
        }
        assert!(matches!(
            ops.start(&w, async { OpResult::TimedOut }),
            Err(CoreError::Limit(_))
        ));
        ops.fail_all();
        let mut out = Vec::new();
        ops.take_results(&mut out);
        assert_eq!(out.len(), MAX_OPS_PER_CONN);
        assert!(
            out.iter()
                .all(|(_, r)| matches!(r, OpResult::Err(CoreError::ConnectionClosed(_))))
        );
        assert_eq!(ops.pending(), 0);
        assert!(ops.start(&w, async { OpResult::TimedOut }).is_ok());
    }

    #[test]
    fn track_info_conversion() {
        let st = TrackState {
            position: Duration::from_millis(1500),
            loops: LoopState::Infinite,
            ..TrackState::default()
        };
        let d = TrackInfoData::from_state(&st);
        assert_eq!((d.playing, d.position_ms, d.loops), ("play", 1500, None));
    }
}
