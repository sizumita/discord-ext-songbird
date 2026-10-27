//! `LiveReader`: the songbird `FormatReader` of every live PCM input (W4, §7.1, §7.4).
//!
//! `next_packet` reads atomics only and returns an empty 960-frame-duration packet (no
//! allocation); the paired `PcmDecoder` pulls the audio. `Err(IoError(UnexpectedEof))` is returned
//! **iff** the source should end, because any `Err` ends the track.
//!
//! The same reader serves in-memory PCM (`RawPcm`, the design's `MemPcmReader`): with
//! `backseek = true`, `seek` forwards to [`PcmProvider::seek_to_frame`].

use std::io::{Cursor, ErrorKind};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use songbird::input::core::codecs::CodecParameters;
use songbird::input::core::errors::{Error as SymphError, Result as SymphResult};
use songbird::input::core::formats::{
    Cue, FormatOptions, FormatReader, Packet, SeekMode, SeekTo, SeekedTo, Track,
};
use songbird::input::core::io::MediaSourceStream;
use songbird::input::core::meta::{Metadata, MetadataLog};
use songbird::input::core::units::TimeBase;
use songbird::input::{Input, LiveInput, Parsed};

use super::decoder::PcmDecoder;
use super::probe::probe_raw_header;
use super::{PcmProvider, enter_mixer};
use crate::error::CoreError;
use crate::limits::{FRAMES_PER_TICK_48K, MIX_RATE};
use crate::runtime::{LiveCounter, live};
use crate::tracks::flags::{DECODE_ERROR, DoneGuard, EOF_SEEN, PANICKED};

fn eof() -> SymphError {
    SymphError::IoError(ErrorKind::UnexpectedEof.into())
}

/// The reader half of a live (or memory) PCM input.
pub struct LiveReader {
    p: Arc<dyn PcmProvider>,
    track: Track,
    ts: u64,
    done: DoneGuard,
    /// From this input's own probe (§7.1 rule 6); returned by `into_inner`.
    mss: Option<MediaSourceStream>,
    meta: MetadataLog,
    /// Shared with the paired decoder: set when a panic was caught.
    poison: Arc<AtomicBool>,
    backseek: bool,
    _live: LiveCounter,
    _input: LiveCounter,
}

impl LiveReader {
    pub(crate) fn new(
        p: Arc<dyn PcmProvider>,
        done: DoneGuard,
        mss: MediaSourceStream,
        poison: Arc<AtomicBool>,
        params: CodecParameters,
        backseek: bool,
    ) -> Self {
        LiveReader {
            p,
            track: Track::new(0, params),
            ts: 0,
            done,
            mss: Some(mss),
            meta: MetadataLog::default(),
            poison,
            backseek,
            _live: LiveCounter::new(&live::LIVE_READER),
            _input: LiveCounter::new(&live::SONGBIRD_INPUT),
        }
    }

    fn next_inner(&mut self) -> SymphResult<Packet> {
        if self.poison.load(Ordering::Acquire) {
            // The decoder (or this reader) caught a panic: end the track (§5.7).
            self.done.flags().hint(DECODE_ERROR | PANICKED);
            return Err(eof());
        }
        if self.p.should_end() {
            self.done.flags().hint(EOF_SEEN);
            return Err(eof());
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
}

impl FormatReader for LiveReader {
    fn try_new(_source: MediaSourceStream, _options: &FormatOptions) -> SymphResult<Self>
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

    /// Live sources: a no-op (`supports_backseek: false`). Memory sources: frame-accurate.
    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> SymphResult<SeekedTo> {
        let required_ts = match to {
            SeekTo::TimeStamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => TimeBase::new(1, MIX_RATE).calc_timestamp(time),
        };
        if self.backseek {
            let rate = u64::from(self.p.format().rate.hz());
            let frame = u128::from(required_ts) * u128::from(rate) / u128::from(MIX_RATE);
            let frame = u64::try_from(frame).unwrap_or(u64::MAX);
            if let Some(actual) = self.p.seek_to_frame(frame) {
                let ts = u128::from(actual) * u128::from(MIX_RATE) / u128::from(rate.max(1));
                self.ts = u64::try_from(ts).unwrap_or(u64::MAX);
                // songbird loops a track by seeking it back: the end this reader saw is no longer
                // its end, so a later `stop_all()` (no `End` event) must not finalise as
                // `finished`. Seeking to the end records it again at the next pull.
                self.done.flags().clear_hint(EOF_SEEN);
            }
        }
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
        enter_mixer();
        match catch_unwind(AssertUnwindSafe(|| self.next_inner())) {
            Ok(r) => r,
            Err(_) => {
                self.poison.store(true, Ordering::Release);
                self.done.flags().hint(DECODE_ERROR | PANICKED);
                self.p.on_poison();
                Err(eof())
            }
        }
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
) -> Result<Input, CoreError> {
    let (meta, mss) = probe_raw_header(p.format().channels)?;
    let poison = Arc::new(AtomicBool::new(false));
    let decoder = PcmDecoder::new(p.clone(), poison.clone())?;
    let params = decoder.params().clone();
    let reader = LiveReader::new(p, done, mss, poison, params, backseek);
    Ok(Input::Live(
        LiveInput::Parsed(Parsed {
            format: Box::new(reader),
            decoder: Box::new(decoder),
            track_id: 0,
            meta,
            supports_backseek: backseek,
        }),
        None,
    ))
}
