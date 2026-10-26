//! `sbcore`: the pyo3-free core of discord-ext-songbird (§2.1).
//!
//! All audio, queue, connection and songbird-facing logic lives here. This crate has **no pyo3
//! dependency**, so no state reachable from songbird, tokio or the mixer can name a Python type
//! (G1, I1). The binding crate (`discord_ext_songbird_native`) wraps it in pyclasses.
//!
//! Module ownership follows §12.1: W1 owns `limits`, `error`, `runtime`, `sync`, `clock`, `wake`,
//! `log`, `format` and `dsp`; W2 `events`, `ctrl`, `conn`, `tracks`; W3 `rx`; W4 `tx`; W5 `testing`.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing
    )
)]

pub mod clock;
pub mod dsp;
pub mod error;
pub mod format;
pub mod limits;
pub mod log;
pub mod runtime;
pub mod sync;
pub mod wake;

pub mod conn;
pub mod ctrl;
pub mod events;
pub mod rx;
pub mod tracks;
pub mod tx;

#[cfg(feature = "test-hooks")]
pub mod testing;

/// Per-thread allocation counting for tests (§13.1). Installed as the global allocator only under
/// `cfg(test)` or the `test-hooks` feature.
#[cfg(any(test, feature = "test-hooks"))]
pub mod alloc_count;

#[cfg(any(test, feature = "test-hooks"))]
#[global_allocator]
static GLOBAL: alloc_count::CountingAlloc = alloc_count::CountingAlloc;

pub use error::{CloseReason, CoreError};
pub use format::{Encoding, Format, SampleRate};
pub use limits::{LimitError, LimitNum};
