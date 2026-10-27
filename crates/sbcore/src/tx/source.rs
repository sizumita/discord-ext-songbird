//! Source specs for `play` and the queue (W4, §7.9, §7.10). Entries are turned into songbird inputs
//! only when they start, so queued inputs are not resident in the mixer.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::encoded::{EncodedShared, EncodedSpec, spawn_encoded_with};
use super::memory::{EncodedBytes, MemPcm, OpusFrames};
use crate::clock::Clock;
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
    /// when the entry becomes current) and allocates a wake token from `wake`. An encoded stream
    /// built here does not follow the connection's self-mute and uses the real clock; see
    /// [`SourceSpec::to_input_with`].
    pub fn to_input(
        &self,
        done: DoneGuard,
        wake: &Arc<WakeHub>,
    ) -> Result<(songbird::input::Input, Option<Arc<EncodedShared>>), CoreError> {
        self.to_input_with(done, wake, Arc::new(AtomicBool::new(false)), Clock::Real)
    }

    /// [`SourceSpec::to_input`] with the connection's send-mute flag and clock (used by `Encoded`
    /// only, see `spawn_encoded_with`).
    pub fn to_input_with(
        &self,
        done: DoneGuard,
        wake: &Arc<WakeHub>,
        send_muted: Arc<AtomicBool>,
        clock: Clock,
    ) -> Result<(songbird::input::Input, Option<Arc<EncodedShared>>), CoreError> {
        match self {
            SourceSpec::File(b) => Ok((b.to_input(done), None)),
            SourceSpec::Pcm(m) => Ok((m.to_input(done)?, None)),
            SourceSpec::Opus(f) => Ok((f.to_input(done)?, None)),
            SourceSpec::Encoded(fac) => {
                let token = wake.alloc()?;
                let (shared, input) = spawn_encoded_with(fac.spec, token, done, send_muted, clock)?;
                Ok((input, Some(shared)))
            }
        }
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
