//! Receive pipeline (W3, §8, §12.4).
//!
//! `Dispatch` turns songbird `VoiceTick`s into [`raw::RawTick`]s; the ConnWorker runs
//! [`pipeline::RxPipeline`]: extract → Opus decode → 960-frame normalise → speaking FSM → `MixBus` →
//! converters → lanes (encode + chunk, `Arc<ChunkData>`) → `SubQueue` fan-out, on a continuous,
//! silence-filled timeline.

pub mod bus;
pub mod chunk;
pub mod convert;
pub mod decoder;
pub mod lane;
pub mod opus;
pub mod pipeline;
pub mod queue;
pub mod raw;
pub mod router;
pub mod speaking;

#[cfg(test)]
mod pipeline_tests;
#[cfg(test)]
mod test_util;

pub use bus::{MixFilter, MixPolicy};
pub use chunk::{ChunkData, ChunkMeta};
pub use pipeline::{RxConfig, RxCounts, RxPipeline, RxTimer};
pub use queue::{Delivered, Overflow, RxSource, SubQueue, SubSpec, Subscription};
pub use raw::{RawTick, RawVoice, RxInput, RxSender};
pub use router::{RouterHandle, RouterQueue, RouterSpec};
