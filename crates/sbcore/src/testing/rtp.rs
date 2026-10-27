//! RTP and Opus helpers for receive tests (§13.2).
//!
//! [`rtp_wrap`] builds packets exactly as songbird 0.6.1 hands them to a `VoiceTick` in
//! `DecodeMode::Decrypt` with an `*_rtpsize` AEAD mode: the transport-decrypted plaintext sits at the
//! start of the RTP payload, the RTP header extension and padding are **not** stripped, an optional
//! DAVE supplemental block is still attached after the Opus frame, and the 20-byte AEAD suffix
//! (16-byte tag + 4-byte nonce) is still at the end. `payload_offset` is therefore 0 and
//! `payload_end` (an **end index**, §8.3) is `payload.len() − 20`.

use songbird::driver::opus::{Application, Channels, Encoder};
use songbird::packet::Packet;
use songbird::packet::rtp::RtpPacket;

use crate::error::CoreError;
use crate::limits::{self, FRAMES_PER_TICK_48K};

/// Bytes after the plaintext in songbird's supported crypto modes (`payload_suffix_len()`:
/// 4-byte nonce + 16-byte AEAD tag, `driver/crypto.rs`).
pub const CRYPTO_SUFFIX: usize = 20;
/// RTP payload type Discord uses for Opus.
pub const OPUS_PAYLOAD_TYPE: u8 = 120;
/// The fixed RTP header length (no CSRCs).
pub const RTP_HEADER_LEN: usize = 12;
/// Largest packet [`rtp_wrap`] builds (a UDP datagram on a 1500-byte MTU path).
pub const MAX_RTP_PACKET: usize = 1_460;
/// Longest input [`encode_opus`] accepts: 60 s of 48 kHz stereo PCM16LE (bounded test helper).
pub const MAX_ENCODE_MS: u64 = 60_000;
/// Length of the DAVE supplemental block [`rtp_wrap`] appends:
/// 8-byte truncated tag + 1-byte ULEB128 nonce + size byte + `FA FA`.
pub const DAVE_TRAILER_LEN: usize = 12;

const STEREO_FRAME_BYTES: usize = 4;
const TICK_BYTES: usize = FRAMES_PER_TICK_48K * STEREO_FRAME_BYTES;
const MAX_OPUS_PACKET: usize = 1_275;

/// Options of [`rtp_wrap`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtpOpts {
    /// Add a one-word RTP header extension (profile `0xBEDE`).
    pub extension: bool,
    /// Padding bytes (RTP P bit; the last byte is the count). 0 = no padding.
    pub padding: u8,
    /// Append a DAVE supplemental block (`tag8 | nonce | size | FA FA`).
    pub dave_trailer: bool,
}

/// Encodes interleaved 48 kHz stereo PCM16LE into 20 ms Opus packets (zero-padding the last one).
///
/// An empty input yields no packets. Inputs longer than [`MAX_ENCODE_MS`] raise `LimitExceeded`.
pub fn encode_opus(pcm48_stereo: &[u8]) -> Result<Vec<Vec<u8>>, CoreError> {
    let max_bytes = limits::mul_checked(
        "pcm48_stereo_bytes",
        MAX_ENCODE_MS,
        (TICK_BYTES as u64) / 20,
    )?;
    limits::check(
        "pcm48_stereo_bytes",
        pcm48_stereo.len() as u64,
        0..=max_bytes,
    )?;
    let mut enc = Encoder::new(48_000, Channels::Stereo, Application::Audio)
        .map_err(|_| CoreError::Internal("testing::encode_opus (encoder)"))?;
    let ticks = pcm48_stereo.len().div_ceil(TICK_BYTES);
    let mut out = Vec::new();
    out.try_reserve_exact(ticks)?;
    let mut samples = [0i16; FRAMES_PER_TICK_48K * 2];
    let mut packet = [0u8; MAX_OPUS_PACKET];
    for chunk in pcm48_stereo.chunks(TICK_BYTES) {
        samples.fill(0);
        let (pairs, _odd) = chunk.as_chunks::<2>();
        for (dst, src) in samples.iter_mut().zip(pairs) {
            *dst = i16::from_le_bytes(*src);
        }
        let n = enc
            .encode(&samples, &mut packet)
            .map_err(|_| CoreError::Internal("testing::encode_opus (encode)"))?;
        let body = packet
            .get(..n)
            .ok_or(CoreError::Internal("testing::encode_opus (length)"))?;
        let mut p = Vec::new();
        p.try_reserve_exact(n)?;
        p.extend_from_slice(body);
        out.push(p);
    }
    Ok(out)
}

/// Wraps an Opus payload in an RTP packet as songbird's Decrypt mode delivers it (see the module
/// documentation). The RTP timestamp is `seq × 960`.
pub fn rtp_wrap(opus: &[u8], seq: u16, ssrc: u32, opts: RtpOpts) -> Result<Vec<u8>, CoreError> {
    let ext_len = if opts.extension { 8 } else { 0 };
    let dave_len = if opts.dave_trailer {
        DAVE_TRAILER_LEN
    } else {
        0
    };
    let total = RTP_HEADER_LEN
        .saturating_add(ext_len)
        .saturating_add(opus.len())
        .saturating_add(dave_len)
        .saturating_add(usize::from(opts.padding))
        .saturating_add(CRYPTO_SUFFIX);
    limits::check("rtp_packet_bytes", total, 0..=MAX_RTP_PACKET)?;

    let mut pkt = Vec::new();
    pkt.try_reserve_exact(total)?;
    let mut b0 = 0x80u8; // V=2, CC=0
    if opts.padding > 0 {
        b0 |= 0x20;
    }
    if opts.extension {
        b0 |= 0x10;
    }
    pkt.push(b0);
    pkt.push(OPUS_PAYLOAD_TYPE); // M=0
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&u32::from(seq).wrapping_mul(960).to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    if opts.extension {
        // Profile 0xBEDE, length 1 word; one-byte element id=1, len=0 (1 data byte), 2 pad bytes.
        pkt.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x01, 0x10, 0xAA, 0x00, 0x00]);
    }
    pkt.extend_from_slice(opus);
    if opts.dave_trailer {
        // 8-byte truncated AES-GCM tag, ULEB128 nonce, no unencrypted ranges, size, magic marker.
        pkt.extend_from_slice(&[0xD1, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8]);
        pkt.push(0x01);
        pkt.push(DAVE_TRAILER_LEN as u8);
        pkt.extend_from_slice(&[0xFA, 0xFA]);
    }
    if opts.padding > 0 {
        pkt.resize(pkt.len() + usize::from(opts.padding) - 1, 0);
        pkt.push(opts.padding);
    }
    pkt.resize(pkt.len() + CRYPTO_SUFFIX, 0);
    Ok(pkt)
}

/// `(payload_offset, payload_end)` of a packet built by [`rtp_wrap`] (or any RTP packet carrying
/// the 20-byte crypto suffix), as songbird's `RtpData` reports them; `(0, 0)` if `pkt` does not
/// parse as RTP or is too short (the pipeline then counts a bad packet).
pub fn payload_bounds(pkt: &[u8]) -> (usize, usize) {
    match RtpPacket::new(pkt) {
        Some(rtp) => (0, rtp.payload().len().saturating_sub(CRYPTO_SUFFIX)),
        None => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rx::opus::extract_opus;

    fn tone(ms: usize) -> Vec<u8> {
        let frames = 48 * ms;
        let mut out = Vec::with_capacity(frames * 4);
        for i in 0..frames {
            let s =
                ((i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * 8_000.0) as i16;
            out.extend_from_slice(&s.to_le_bytes());
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn encode_opus_packets_are_20ms() {
        let pkts = encode_opus(&tone(100)).unwrap();
        assert_eq!(pkts.len(), 5);
        for p in &pkts {
            assert!(!p.is_empty() && p.len() <= MAX_OPUS_PACKET);
            assert_eq!(
                songbird::driver::opus::packet::get_nb_samples(p, 48_000).unwrap(),
                960
            );
        }
        // A partial tick is zero-padded into one more packet; odd byte counts never fail.
        assert_eq!(encode_opus(&tone(30)).unwrap().len(), 2);
        assert_eq!(encode_opus(&[1, 2, 3]).unwrap().len(), 1);
        assert!(encode_opus(&[]).unwrap().is_empty());
    }

    #[test]
    fn encode_opus_is_bounded() {
        let too_long = vec![0u8; (MAX_ENCODE_MS as usize / 20 + 1) * TICK_BYTES];
        assert!(matches!(encode_opus(&too_long), Err(CoreError::Limit(_))));
    }

    #[test]
    fn rtp_wrap_round_trips_through_extract_opus() {
        let opus = encode_opus(&tone(20)).unwrap().remove(0);
        for extension in [false, true] {
            for padding in [0u8, 1, 4] {
                for dave_trailer in [false, true] {
                    let opts = RtpOpts {
                        extension,
                        padding,
                        dave_trailer,
                    };
                    let pkt = rtp_wrap(&opus, 7, 0xABCD, opts).unwrap();
                    let rtp = RtpPacket::new(&pkt).unwrap();
                    let seq: u16 = rtp.get_sequence().into();
                    assert_eq!(seq, 7);
                    assert_eq!(rtp.get_ssrc(), 0xABCD);
                    assert_eq!(rtp.get_extension() != 0, extension);
                    assert_eq!(rtp.get_padding() != 0, padding > 0);
                    let (off, end) = payload_bounds(&pkt);
                    let range = extract_opus(&pkt, off, end)
                        .unwrap_or_else(|| panic!("extract failed for {opts:?}"));
                    assert_eq!(&pkt[range], opus.as_slice(), "{opts:?}");
                }
            }
        }
    }

    #[test]
    fn rtp_wrap_header_fields() {
        let pkt = rtp_wrap(&[0xF8, 0xFF, 0xFE], 2, 9, RtpOpts::default()).unwrap();
        assert_eq!(pkt.len(), RTP_HEADER_LEN + 3 + CRYPTO_SUFFIX);
        assert_eq!(pkt[0], 0x80);
        assert_eq!(pkt[1], OPUS_PAYLOAD_TYPE);
        assert_eq!(
            u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]),
            2 * 960
        );
        assert_eq!(payload_bounds(&pkt), (0, 3));
    }

    #[test]
    fn rtp_wrap_is_bounded_and_garbage_has_no_bounds() {
        assert!(matches!(
            rtp_wrap(&[0u8; MAX_RTP_PACKET], 0, 1, RtpOpts::default()),
            Err(CoreError::Limit(_))
        ));
        assert_eq!(payload_bounds(&[1, 2, 3]), (0, 0));
    }
}
