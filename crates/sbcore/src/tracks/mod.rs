//! Tracks, handles and the queue (W2, §7.10, §12.3).
//!
//! * [`flags`]: per-track completion state, resolved by priority (Dispatch `End`/`Error` is
//!   authoritative; a reader's drop is only a provisional hint).
//! * [`registry`]: every track of one connection, polled by the housekeeper.
//! * [`queue`]: the sequential queue; only the current entry is handed to songbird.

pub mod flags;
pub mod queue;
pub mod registry;

pub use flags::{
    DoneGuard, PendingChange, PlayOpts, TrackEndReason, TrackFlags, TrackKind, TrackRef,
};
pub use queue::TrackQueue;
pub use registry::TrackRegistry;
/// songbird's track handle types, re-exported for the binding (which has no direct songbird
/// dependency).
pub use songbird::tracks::{ControlError, TrackHandle};
