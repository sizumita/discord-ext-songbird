//! One probe per input, on the creating thread (W4, §7.1 rule 6).
//!
//! `ProbedMetadata` has no public constructor and no `Clone`, so every `Parsed` needs its own probe:
//! the 16-byte header `b"SbirdRaw" + le32(48000) + le32(ch)` that songbird's `RawReader` recognises
//! supplies `Parsed.meta`, and the drained `MediaSourceStream` (one 64 KiB ring) is what the reader
//! returns from `into_inner`.

use std::io::Cursor;

use songbird::input::codecs::get_probe;
use songbird::input::core::formats::FormatOptions;
use songbird::input::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use songbird::input::core::meta::MetadataOptions;
use songbird::input::core::probe::{Hint, ProbedMetadata};

use crate::error::CoreError;
use crate::limits::MIX_RATE;

/// Probes the raw header for `channels` (1 or 2) and returns its metadata and drained stream.
/// Never call it on a mixer thread (it allocates the 64 KiB ring).
pub fn probe_raw_header(channels: u8) -> Result<(ProbedMetadata, MediaSourceStream), CoreError> {
    if !(1..=2).contains(&channels) {
        return Err(CoreError::invalid("channels must be 1 or 2"));
    }
    let mut header = Vec::with_capacity(16);
    header.extend_from_slice(b"SbirdRaw");
    header.extend_from_slice(&MIX_RATE.to_le_bytes());
    header.extend_from_slice(&u32::from(channels).to_le_bytes());
    let mss = MediaSourceStream::new(
        Box::new(Cursor::new(header)),
        MediaSourceStreamOptions::default(),
    );
    let probed = get_probe()
        .format(
            &Hint::new(),
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|_| CoreError::Internal("probe_raw_header"))?;
    Ok((probed.metadata, probed.format.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use songbird::input::core::io::ReadBytes;

    #[test]
    fn probes_mono_and_stereo() {
        for ch in [1u8, 2] {
            let (_meta, mss) = probe_raw_header(ch).unwrap();
            // The drained stream of the probe: positioned after the 16-byte header.
            assert_eq!(mss.pos(), 16);
        }
        assert!(probe_raw_header(3).is_err());
    }
}
