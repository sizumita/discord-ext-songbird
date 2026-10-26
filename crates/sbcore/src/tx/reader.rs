//! `LiveReader`: the songbird `FormatReader` of every live PCM input (W4, §7.1, §7.4).
//!
//! `next_packet` reads atomics only and returns an empty 960-frame-duration packet (no
//! allocation); the paired `PcmDecoder` pulls the audio. `Err(IoError(UnexpectedEof))` is returned
//! **iff** the source should end, because any `Err` ends the track.
#![allow(dead_code, unused_variables)] // phase-0 skeleton

use std::io::{Cursor, ErrorKind};
use std::sync::Arc;

use songbird::input::core::errors::{Error as SymphError, Result as SymphResult};
use songbird::input::core::formats::{
    Cue, FormatOptions, FormatReader, Packet, SeekMode, SeekTo, SeekedTo, Track,
};
use songbird::input::core::io::MediaSourceStream;
use songbird::input::core::meta::{Metadata, MetadataLog};

use super::PcmProvider;
use crate::error::CoreError;
use crate::limits::FRAMES_PER_TICK_48K;
use crate::runtime::LiveCounter;
use crate::tracks::flags::DoneGuard;

/// The reader half of a live input.
pub struct LiveReader {
    p: Arc<dyn PcmProvider>,
    track: Track,
    ts: u64,
    _done: DoneGuard,
    /// From this input's own probe (§7.1 rule 6); returned by `into_inner`.
    mss: Option<MediaSourceStream>,
    meta: MetadataLog,
    _live: LiveCounter,
}

impl FormatReader for LiveReader {
    fn try_new(source: MediaSourceStream, options: &FormatOptions) -> SymphResult<Self>
    where
        Self: Sized,
    {
        Err(SymphError::Unsupported(
            "LiveReader is constructed by sbcore, not probed",
        ))
    }

    fn cues(&self) -> &[Cue] {
        &[]
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.meta.metadata()
    }

    /// A no-op for live sources (`supports_backseek: false`).
    fn seek(&mut self, mode: SeekMode, to: SeekTo) -> SymphResult<SeekedTo> {
        let required_ts = match to {
            SeekTo::TimeStamp { ts, .. } => ts,
            SeekTo::Time { .. } => self.ts,
        };
        Ok(SeekedTo {
            track_id: 0,
            required_ts,
            actual_ts: self.ts,
        })
    }

    fn tracks(&self) -> &[Track] {
        std::slice::from_ref(&self.track)
    }

    fn next_packet(&mut self) -> SymphResult<Packet> {
        if self.p.should_end() {
            return Err(SymphError::IoError(ErrorKind::UnexpectedEof.into()));
        }
        let ts = self.ts;
        self.ts = self.ts.wrapping_add(FRAMES_PER_TICK_48K as u64);
        Ok(Packet::new_from_boxed_slice(
            0,
            ts,
            FRAMES_PER_TICK_48K as u64,
            Box::default(),
        ))
    }

    fn into_inner(mut self: Box<Self>) -> MediaSourceStream {
        self.mss.take().unwrap_or_else(|| {
            MediaSourceStream::new(Box::new(Cursor::new(Vec::<u8>::new())), Default::default())
        })
    }
}

impl Drop for LiveReader {
    fn drop(&mut self) {
        self.p.on_reader_drop();
    }
}

/// Builds `Input::Live(LiveInput::Parsed(..), None)` from a provider: probe once on the creating
/// thread, `LiveReader` + `PcmDecoder`, `supports_backseek = backseek`.
pub fn live_pcm_input(
    p: Arc<dyn PcmProvider>,
    done: DoneGuard,
    backseek: bool,
) -> Result<songbird::input::Input, CoreError> {
    Err(CoreError::Internal("unimplemented"))
}
