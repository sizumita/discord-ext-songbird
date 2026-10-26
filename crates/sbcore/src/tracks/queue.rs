//! The sequential track queue (W2, §7.10). Only the current entry is handed to songbird; pending
//! entries (≤ `MAX_QUEUE_ENTRIES`) hold their source spec, options and flags. Not gapless.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use super::flags::{DoneGuard, PendingChange, PlayOpts, TrackRef};
use super::registry::TrackRegistry;
use crate::conn::shared::ConnShared;
use crate::error::CoreError;
use crate::tx::source::SourceSpec;

/// One pending entry.
#[derive(Debug)]
pub(crate) struct QueueEntry {
    src: SourceSpec,
    opts: PlayOpts,
    track: TrackRef,
    done: DoneGuard,
}

/// Sequential queue. Index 0 is the first **pending** entry; the current track is `current()`.
#[derive(Debug, Default)]
pub struct TrackQueue {
    /// L10.
    items: Mutex<VecDeque<QueueEntry>>,
    /// L10.
    current: Mutex<Option<TrackRef>>,
    paused: AtomicBool,
}

impl TrackQueue {
    pub fn new() -> Self {
        TrackQueue::default()
    }

    /// Adds a pending entry (`LimitExceeded` beyond `MAX_QUEUE_ENTRIES`).
    pub fn enqueue(
        &self,
        src: SourceSpec,
        opts: PlayOpts,
        reg: &TrackRegistry,
    ) -> Result<TrackRef, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// The playing entry.
    pub fn current(&self) -> Option<TrackRef> {
        self.current.lock().clone()
    }

    /// Pending entries (index 0 = next up).
    pub fn snapshot(&self) -> Vec<TrackRef> {
        self.items.lock().iter().map(|e| e.track.clone()).collect()
    }

    /// Number of pending entries.
    pub fn len(&self) -> usize {
        self.items.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stops the current track, then advances.
    pub fn skip(&self) {
        // phase 0: nothing is playing.
    }

    /// `clear()` then `skip()`.
    pub fn stop(&self) {
        // phase 0: nothing is queued.
    }

    /// Pauses the current track and blocks advancing.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    /// Resumes the current track and allows advancing.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }

    /// Pending entries end with `stopped`; the current track keeps playing.
    pub fn clear(&self) {
        // phase 0: nothing is queued.
    }

    /// Removes pending entry `index` and marks it `stopped`.
    pub fn dequeue(&self, index: usize) -> Option<TrackRef> {
        None
    }

    /// Removes the pending entry of `t` and marks it `stopped` (`TrackHandle.stop()` on a pending
    /// entry); `false` if `t` is not pending.
    pub fn remove(&self, t: &TrackRef) -> bool {
        false
    }

    /// Pending entries: pause/resume/volume/loops update the stored `PlayOpts` (`Ok(false)` = not
    /// pending; use the handle).
    pub fn update_pending(&self, t: &TrackRef, c: PendingChange) -> Result<bool, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Advances when the current track resolved and the queue is not paused (pop under L10,
    /// release, then `play` under L1).
    pub(crate) fn advance(&self, conn: &ConnShared) {
        // phase 0: never advances.
    }
}
