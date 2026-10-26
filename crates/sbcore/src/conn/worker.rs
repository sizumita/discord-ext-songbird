//! The per-connection ConnWorker (W2, §3.1, §4.7 T6, §8.8): one tokio task on the RX runtime that
//! runs the receive pipeline, the send housekeeper and the connection timers.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::sync::mpsc::Receiver;

use super::shared::ConnShared;
use crate::clock::Clock;
use crate::error::{CloseReason, CoreError};
use crate::runtime::{LiveCounter, live};
use crate::rx::pipeline::RxPipeline;
use crate::rx::queue::{RxCmd, SubQueue};
use crate::rx::raw::RxInput;
use crate::rx::router::RouterQueue;
use crate::sync::Shutdown;
use crate::tx::LiveSource;
use crate::tx::housekeep::Housekeeper;

/// Commands from Python threads to the worker (applied at the next tick/lane boundary).
pub enum WorkerCmd {
    Rx(RxCmd),
    AddSource(Weak<dyn LiveSource>),
    /// From the `RxLink` action. A `Link` with `seq ≤` the last applied seq is ignored (§4.6).
    Link {
        up: bool,
        ssrc: Option<u32>,
        seq: u64,
    },
}

/// A reservation of the per-connection receive budget (§8.9). Drop releases it.
#[derive(Debug)]
pub struct BudgetGuard {
    worker: Weak<WorkerShared>,
    bytes: usize,
}

impl BudgetGuard {
    /// Reserved bytes.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        if let Some(w) = self.worker.upgrade() {
            w.budget_used.fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

/// State shared between Python threads and the worker task.
pub struct WorkerShared {
    /// L3.
    cmds: Mutex<Vec<WorkerCmd>>,
    notify: Notify,
    shutdown: Arc<Shutdown>,
    known_queues: Mutex<Vec<Weak<SubQueue>>>,
    known_routers: Mutex<Vec<Weak<RouterQueue>>>,
    budget_used: AtomicUsize,
    budget_max: usize,
}

impl WorkerShared {
    /// `budget_max` = `max_receive_mb` in bytes.
    pub fn new(budget_max: usize) -> Arc<Self> {
        Arc::new(WorkerShared {
            cmds: Mutex::new(Vec::new()),
            notify: Notify::new(),
            shutdown: Shutdown::new(),
            known_queues: Mutex::new(Vec::new()),
            known_routers: Mutex::new(Vec::new()),
            budget_used: AtomicUsize::new(0),
            budget_max,
        })
    }

    /// Queues a command and wakes the worker.
    pub fn push(&self, c: WorkerCmd) {
        // phase 0: commands are dropped.
    }

    /// Reserves `bytes` of the receive budget, or `LimitExceeded`.
    pub fn reserve_budget(self: &Arc<Self>, bytes: usize) -> Result<BudgetGuard, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Remembers a subscription queue so T6 can close it synchronously.
    pub fn register_queue(&self, q: &Arc<SubQueue>) {
        // phase 0: not tracked yet.
    }

    /// Remembers a router queue so T6 can close it synchronously.
    pub fn register_router(&self, q: &Arc<RouterQueue>) {
        // phase 0: not tracked yet.
    }

    /// T6, synchronous on the caller thread: closes every registered subscription and router queue.
    pub fn close_queues(&self, r: CloseReason) {
        // phase 0: nothing is registered.
    }

    /// Sets the shutdown flag and wakes the worker; it exits at its next poll.
    pub fn shutdown(&self) {
        self.shutdown.trigger();
        self.notify.notify_one();
    }

    /// Bytes of the receive budget currently reserved.
    pub fn budget_used(&self) -> usize {
        self.budget_used.load(Ordering::Acquire)
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.is_set()
    }
}

/// The worker task's state (owned by the task; nothing else holds it).
pub struct ConnWorker {
    conn: Weak<ConnShared>,
    shared: Arc<WorkerShared>,
    pipeline: RxPipeline,
    house: Housekeeper,
    rx_in: Receiver<RxInput>,
    overflow: Arc<Mutex<VecDeque<RxInput>>>,
    clock: Clock,
    last_link_seq: u64,
    _live: LiveCounter,
}

impl ConnWorker {
    pub(crate) fn new(
        conn: Weak<ConnShared>,
        shared: Arc<WorkerShared>,
        pipeline: RxPipeline,
        rx_in: Receiver<RxInput>,
        overflow: Arc<Mutex<VecDeque<RxInput>>>,
        clock: Clock,
    ) -> Self {
        ConnWorker {
            conn,
            shared,
            pipeline,
            house: Housekeeper::new(),
            rx_in,
            overflow,
            clock,
            last_link_seq: 0,
            _live: LiveCounter::new(&live::CONN_WORKER),
        }
    }

    /// One iteration: commands, queued inputs, timers. Returns the next wake-up time.
    pub fn step(&mut self, now: Instant) -> Option<Instant> {
        None
    }

    /// The task body: `select!{shutdown, rx_in.recv, notify, sleep_until(next)}` around `step`.
    // Contract signature (§12.3): the explicit `+ Send` bound is part of the API.
    #[allow(clippy::manual_async_fn)]
    pub fn run(self) -> impl Future<Output = ()> + Send {
        async move {
            // phase 0: the worker exits immediately.
            drop(self);
        }
    }
}
