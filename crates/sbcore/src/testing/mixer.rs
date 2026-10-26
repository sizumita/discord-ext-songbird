//! `MixerHarness` (§13.2): takes ownership of a detached playback's reader/decoder pair and drives
//! it exactly as songbird's `mix_symph_indiv` does (`next_packet` → `decode`), asserting 960 frames.
#![allow(dead_code, unused_variables, clippy::ptr_arg)] // phase-0 skeleton

use std::sync::Arc;

use parking_lot::Mutex;
use songbird::input::core::codecs::Decoder;
use songbird::input::core::formats::FormatReader;

use crate::error::CoreError;
use crate::tx::queue::PcmQueue;

/// The parked reader/decoder pair of one playback.
type Pair = (Box<dyn FormatReader>, Box<dyn Decoder>);

/// Drives one playback's songbird input on the calling thread (`Role::Harness`).
pub struct MixerHarness {
    pair: Mutex<Option<Pair>>,
}

impl MixerHarness {
    /// Takes the parked input of `playback` (detached connections only).
    pub fn take(playback: &Arc<PcmQueue>) -> Result<Arc<Self>, CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    /// Pulls `ticks` ticks; appends 960 × ch interleaved 48 kHz `f32` samples per tick to `out`.
    /// Dropping the pair (EOF, `stop()`, `close()`) runs the real `DoneGuard` path.
    pub fn pull(&self, ticks: usize, out: &mut Vec<f32>) -> Result<(), CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }
}
