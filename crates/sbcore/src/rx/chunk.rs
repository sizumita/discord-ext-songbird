//! Received chunks (W3, §8.7, §8.10). A chunk's payload is Arc-backed and immutable, so every
//! subscriber of a lane shares one `Arc<ChunkData>` and Arrow export can own a Rust `Arc` (§8.12).

use std::time::Instant;

use crate::dsp::Level;
use crate::format::{Format, PcmBuf};

/// Metadata of one chunk.
#[derive(Clone, Debug)]
pub struct ChunkMeta {
    pub format: Format,
    /// `None` for mixed streams.
    pub user_id: Option<u64>,
    /// Connection receive timeline of the first sample.
    pub timeline_ms: u64,
    pub frames: u32,
    /// Any voiced audio in this chunk.
    pub speaking: bool,
    pub voiced_ms: u32,
    pub concealed_ms: u32,
    /// Synthetic silence (no Discord audio flow).
    pub gap_ms: u32,
    /// Voiced users contributing.
    pub speakers: Box<[u64]>,
    pub level: Level,
    /// Arrival of the tick that produced the first sample.
    pub captured_at: Instant,
}

/// One encoded chunk: `Send + Sync + RefUnwindSafe` (an Arrow `Allocation`).
#[derive(Debug)]
pub struct ChunkData {
    pub meta: ChunkMeta,
    pub data: PcmBuf,
}

// §12.4 contract: `ChunkData` must stay usable as an Arrow `Allocation` (`Send + Sync +
// RefUnwindSafe`); this fails to compile if a field ever breaks that.
const _: () = {
    const fn assert_allocation<T: Send + Sync + std::panic::RefUnwindSafe>() {}
    assert_allocation::<ChunkData>();
};

impl ChunkData {
    /// Duration in milliseconds (`frames · 1000 / rate`).
    pub fn duration_ms(&self) -> u64 {
        self.meta.format.ms_for_frames(u64::from(self.meta.frames))
    }
}
