//! Asynchronous track operations (`seek`, `info`) with results polled by Python (W2, §7.10).
//!
//! Each op is a task on the RX runtime that stores its result here and rings `WAKE_OPS`; at most
//! `MAX_OPS_PER_CONN` are pending per connection.
#![allow(dead_code, unused_variables, clippy::ptr_arg)] // phase-0 skeleton

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::task::AbortHandle;

use crate::error::CoreError;
use crate::wake::WakeHub;

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
    slots: Mutex<Vec<Option<OpSlot>>>,
    next_id: AtomicU32,
}

impl OpRegistry {
    pub fn new() -> Self {
        OpRegistry::default()
    }

    /// Spawns `fut` on `runtime::rx()`; its result is stored and `WAKE_OPS` rung. Returns the op id.
    pub fn start<F>(&self, wake: &Arc<WakeHub>, fut: F) -> Result<u32, CoreError>
    where
        F: Future<Output = OpResult> + Send + 'static,
    {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Moves every finished result into `out`.
    pub fn take_results(&self, out: &mut Vec<(u32, OpResult)>) {
        // phase 0: no op ever finishes.
    }

    /// Aborts every pending op and resolves it as `ConnectionClosed` (T8).
    pub fn fail_all(&self) {
        // phase 0: nothing is pending.
    }
}
