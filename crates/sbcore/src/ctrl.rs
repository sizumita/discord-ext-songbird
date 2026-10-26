//! Control slots: how Rust asks the Python pump for gateway actions without holding Python objects
//! (W2, §6.4, §12.3).
//!
//! `CtrlQueue` is a set of slots, not a queue: every control is level-triggered or coalesces to its
//! latest value, so nothing is ever dropped however long the loop stalls.
#![allow(dead_code, unused_variables, clippy::ptr_arg)] // phase-0 skeleton

use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::CloseReason;
use crate::wake::WakeHub;

/// A request for the Python pump (§6.4). There is no `TrackEnded`: completion is level-triggered
/// through `WAKE_TRACKS`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    ChannelMoved { channel_id: u64 },
    ClassifyUsers,
    Rejoin { attempt: u32, channel_id: u64 },
    Closed { reason: CloseReason },
}

/// The four slots (under L10).
#[derive(Debug, Default)]
struct Slots {
    moved: Option<u64>,
    classify: bool,
    rejoin: Option<(u32, u64)>,
    closed: Option<CloseReason>,
}

/// Coalescing control slots; setting one rings `WAKE_CTRL`.
#[derive(Debug)]
pub struct CtrlQueue {
    /// L10.
    slots: Mutex<Slots>,
    wake: Arc<WakeHub>,
}

impl CtrlQueue {
    pub fn new(wake: Arc<WakeHub>) -> Arc<Self> {
        Arc::new(CtrlQueue {
            slots: Mutex::new(Slots::default()),
            wake,
        })
    }

    /// Latest value wins; `Closed` is sticky; rings `WAKE_CTRL` after unlocking.
    pub fn set(&self, c: Control) {
        // phase 0: controls are not recorded yet.
    }

    /// Sets the sticky `Closed` slot (T8).
    pub fn set_closed(&self, r: CloseReason) {
        // phase 0: controls are not recorded yet.
    }

    /// Takes every pending slot in the table order of §6.4 and clears them; `Rejoin` is omitted
    /// once `Closed` is set.
    pub fn take(&self, out: &mut Vec<Control>) {
        // phase 0: nothing is ever pending.
    }
}
