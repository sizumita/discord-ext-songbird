//! Source specs for `play` and the queue (W4, §7.9, §7.10). Entries are turned into songbird inputs
//! only when they start, so queued inputs are not resident in the mixer.
#![allow(unused_variables)] // phase-0 skeleton

use std::sync::Arc;

use super::encoded::{EncodedShared, EncodedSpec};
use super::memory::{EncodedBytes, MemPcm, OpusFrames};
use crate::error::CoreError;
use crate::tracks::flags::{DoneGuard, TrackKind};
use crate::wake::WakeHub;

/// A lazy encoded-stream entry: nothing is allocated until it becomes current.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EncodedFactory {
    pub spec: EncodedSpec,
}

/// What a track plays.
#[derive(Clone, Debug)]
pub enum SourceSpec {
    File(Arc<EncodedBytes>),
    Pcm(Arc<MemPcm>),
    Opus(Arc<OpusFrames>),
    Encoded(EncodedFactory),
}

impl SourceSpec {
    /// Builds the songbird input. `Encoded` spawns the ByteRing + decode thread now (only called
    /// when the entry becomes current) and allocates a wake token from `wake`.
    pub fn to_input(
        &self,
        done: DoneGuard,
        wake: &Arc<WakeHub>,
    ) -> Result<(songbird::input::Input, Option<Arc<EncodedShared>>), CoreError> {
        Err(CoreError::Internal("unimplemented"))
    }

    pub fn kind(&self) -> TrackKind {
        match self {
            SourceSpec::File(_) => TrackKind::File,
            SourceSpec::Pcm(_) => TrackKind::Pcm,
            SourceSpec::Opus(_) => TrackKind::Opus,
            SourceSpec::Encoded(_) => TrackKind::EncodedStream,
        }
    }

    /// Memory sources seek and loop; encoded streams do not.
    pub fn seekable(&self) -> bool {
        !matches!(self, SourceSpec::Encoded(_))
    }
}

/// Codec and container names compiled into this build (mirrors the cargo features, as in 0.5).
pub fn supported_codecs() -> Vec<&'static str> {
    let mut values = vec!["opus", "dca", "raw-pcm"];
    let full = cfg!(feature = "codec-full");
    let minimal = cfg!(feature = "codec-minimal");
    let table: [(&str, bool); 15] = [
        ("aac", cfg!(feature = "codec-aac")),
        ("adpcm", cfg!(feature = "codec-adpcm")),
        ("alac", cfg!(feature = "codec-alac")),
        ("flac", cfg!(feature = "codec-flac")),
        ("mp1", cfg!(feature = "codec-mp1")),
        ("mp2", cfg!(feature = "codec-mp2")),
        ("mp3", cfg!(feature = "codec-mp3")),
        ("pcm", minimal || cfg!(feature = "codec-pcm")),
        ("vorbis", cfg!(feature = "codec-vorbis")),
        ("format:aiff", cfg!(feature = "format-aiff")),
        ("format:caf", cfg!(feature = "format-caf")),
        ("format:isomp4", cfg!(feature = "format-isomp4")),
        ("format:mkv", cfg!(feature = "format-mkv")),
        ("format:ogg", cfg!(feature = "format-ogg")),
        ("format:wav", minimal || cfg!(feature = "format-wav")),
    ];
    values.extend(
        table
            .iter()
            .filter(|(_, on)| full || *on)
            .map(|(name, _)| *name),
    );
    values
}
