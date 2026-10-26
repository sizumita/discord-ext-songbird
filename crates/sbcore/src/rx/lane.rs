//! Lanes (W3, §8.7): one encoder + chunk accumulator per `(SourceKey, Format, chunk_ms)`, emitting
//! `Arc<ChunkData>` at timeline-aligned boundaries; every subscriber of a lane gets the same `Arc`.
#![allow(dead_code, unused_variables, clippy::ptr_arg)] // phase-0 skeleton

use std::sync::Arc;
use std::time::Instant;

use smallvec::SmallVec;

use super::chunk::ChunkData;
use super::convert::SourceKey;
use super::queue::{SubId, SubQueue};
use crate::error::CoreError;
use crate::format::{Format, PcmBuf};

/// Per-tick metadata a lane folds into its chunk (§8.7 `ChunkMeta`).
///
/// Not named in §12.4 (which only references it from `Lane::feed`); see `docs/deviations/W3.md`.
#[derive(Clone, Debug)]
pub struct TickMeta {
    /// Any voiced audio in this tick (per-user: that user; mixed: any selected user).
    pub voiced: bool,
    /// The tick (or part of it) came from packet-loss concealment.
    pub concealed: bool,
    /// A synthetic (silence-filled) tick: counted in `gap_ms`.
    pub synthetic: bool,
    /// Voiced users contributing to this tick.
    pub speakers: SmallVec<[u64; 8]>,
    /// Arrival of the tick.
    pub captured_at: Instant,
}

/// One lane.
#[derive(Debug)]
pub struct Lane {
    key: SourceKey,
    fmt: Format,
    chunk_ms: u32,
    chunk_frames: usize,
    /// 4-byte-aligned accumulator of one chunk.
    acc: PcmBuf,
    filled_frames: usize,
    subs: Vec<(SubId, Arc<SubQueue>)>,
    start_tick: u64,
}

impl Lane {
    /// A lane whose first chunk starts at the first timeline-aligned boundary at or after
    /// `start_tick`.
    pub fn new(
        key: SourceKey,
        fmt: Format,
        chunk_ms: u32,
        start_tick: u64,
    ) -> Result<Self, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Encodes one converted tick; pushes each completed chunk to `out`.
    pub fn feed(
        &mut self,
        tick: u64,
        converted: &[f32],
        meta: &TickMeta,
        out: &mut Vec<Arc<ChunkData>>,
    ) {
        // phase 0: no chunks.
    }
}
