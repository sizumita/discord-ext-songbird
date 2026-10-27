//! Opus payload extraction from songbird's Decrypt-mode `RtpData` (W3, §8.3). Pure; fuzzed.
//!
//! In Decrypt mode `payload_end_pad` is an end index, the RTP extension and padding are not
//! stripped, and after DAVE decryption the leftover supplemental block is still attached. The whole
//! workaround is isolated here; any `None` is treated as a lost packet (`bad_packets`).

use std::ops::Range;

use songbird::driver::opus;
use songbird::packet::rtp::{RtpExtensionPacket, RtpPacket};
use songbird::packet::{Packet, PacketSize};

/// Opus `SILENT_FRAME` (`F8 FF FE`).
pub const SILENT_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];

/// DAVE magic marker that ends a protocol frame (`FA FA`).
const DAVE_MARKER: [u8; 2] = [0xFA, 0xFA];

/// Smallest DAVE supplemental block: 8-byte tag + 1-byte nonce + size byte + marker.
const DAVE_MIN_SUPPLEMENTAL: usize = 11;

/// What [`extract`] found in one packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extracted {
    /// Byte range of the Opus payload inside the packet.
    pub range: Range<usize>,
    /// RTP sequence number.
    pub seq: u16,
    /// A DAVE leftover supplemental block was stripped (`dave_trailers_stripped`).
    pub dave_stripped: bool,
}

/// The byte range of the Opus payload inside `pkt`, or `None` for a malformed packet (§8.3).
pub fn extract_opus(pkt: &[u8], payload_offset: usize, payload_end: usize) -> Option<Range<usize>> {
    extract(pkt, payload_offset, payload_end).map(|x| x.range)
}

/// [`extract_opus`] plus the RTP sequence number and whether a DAVE leftover was stripped.
///
/// Every access uses `get`/`checked_*`: no input can panic or read out of bounds.
pub fn extract(pkt: &[u8], payload_offset: usize, payload_end: usize) -> Option<Extracted> {
    let rtp = RtpPacket::new(pkt)?;
    let seq = u16::from(rtp.get_sequence());
    let hdr = pkt.len().checked_sub(rtp.payload().len())?;
    let (mut s, mut e) = (
        hdr.checked_add(payload_offset)?,
        hdr.checked_add(payload_end)?,
    );
    if e > pkt.len() || s >= e {
        return None;
    }
    if rtp.get_extension() != 0 {
        s = s.checked_add(RtpExtensionPacket::new(pkt.get(s..e)?)?.packet_size())?;
    }
    if rtp.get_padding() != 0 {
        e = e.checked_sub(usize::from(*pkt.get(e.checked_sub(1)?)?))?;
    }
    let body = pkt.get(s..e)?;
    let mut dave_stripped = false;
    if body.len() >= DAVE_MIN_SUPPLEMENTAL && body.ends_with(&DAVE_MARKER) {
        // DAVE leftover (tag8 | nonce | ranges | size | FA FA): songbird copies the plaintext to the
        // front of the buffer but keeps the full length (upstream #315).
        let n = usize::from(*body.get(body.len().checked_sub(3)?)?);
        if (DAVE_MIN_SUPPLEMENTAL..body.len()).contains(&n) {
            let stripped = body.get(..body.len().checked_sub(n)?)?;
            // TOC sanity: the remainder must still be a well-formed Opus packet.
            if matches!(
                opus::packet::get_nb_samples(stripped, 48_000),
                Ok(120..=5760)
            ) {
                e = e.checked_sub(n)?;
                dave_stripped = true;
            }
        }
    }
    (s < e).then_some(Extracted {
        range: s..e,
        seq,
        dave_stripped,
    })
}

/// Whether `p` is Opus `SILENT_FRAME` (not voiced, §8.5).
pub fn is_silent_frame(p: &[u8]) -> bool {
    p == SILENT_FRAME
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rx::test_util::{RtpBuild, XorShift, opus_frames_at, sine48};

    /// Bytes songbird leaves after the payload in Decrypt mode (AEAD tag + nonce suffix).
    const SUFFIX: usize = 20;

    fn extract_built(b: &RtpBuild, opus: &[u8]) -> (Vec<u8>, Option<Extracted>) {
        let (pkt, off, end) = b.build(opus, SUFFIX);
        let x = extract(&pkt, off, end);
        (pkt, x)
    }

    fn sample_opus() -> Vec<u8> {
        let p = opus_frames_at(&sine48(440.0, 0.3, 960 * 3), 24_000).remove(1);
        assert!(p.len() < 200, "test packets must stay well below 255 bytes");
        p
    }

    #[test]
    fn plain_packet() {
        let opus = sample_opus();
        let (pkt, x) = extract_built(&RtpBuild::new(7, 0x1234), &opus);
        let x = x.expect("plain packet extracts");
        assert_eq!(&pkt[x.range.clone()], &opus[..]);
        assert_eq!(x.seq, 7);
        assert!(!x.dave_stripped);
        assert_eq!(
            extract_opus(&pkt, 0, pkt.len() - 12 - SUFFIX),
            Some(x.range)
        );
    }

    #[test]
    fn extension_and_padding_are_stripped() {
        let opus = sample_opus();
        for (ext, pad) in [(1, 0), (0, 5), (3, 9), (2, 1), (0, 255)] {
            let b = RtpBuild {
                ext_words: ext,
                padding: pad,
                ..RtpBuild::new(100, 9)
            };
            let (pkt, x) = extract_built(&b, &opus);
            let x = x.unwrap_or_else(|| panic!("ext={ext} pad={pad}"));
            assert_eq!(&pkt[x.range], &opus[..], "ext={ext} pad={pad}");
        }
    }

    #[test]
    fn dave_leftover_is_stripped() {
        let opus = sample_opus();
        for nonce in [0u64, 1, 127, 128, 300_000] {
            let b = RtpBuild {
                dave_nonce: Some(nonce),
                ext_words: 1,
                padding: 3,
                ..RtpBuild::new(5, 77)
            };
            let (pkt, x) = extract_built(&b, &opus);
            let x = x.expect("DAVE packet extracts");
            assert!(x.dave_stripped, "nonce {nonce}");
            assert_eq!(&pkt[x.range], &opus[..], "nonce {nonce}");
        }
    }

    #[test]
    fn fake_dave_marker_is_kept() {
        // A payload that merely ends in FA FA but whose stripped remainder is not a valid Opus
        // packet (TOC check fails) is kept whole: 20 bytes of 0xFF are a code-3 packet claiming
        // 63 × 20 ms frames, which libopus rejects.
        let mut fake = vec![0xFFu8; 40];
        fake.extend_from_slice(&[0u8; 10]);
        let n = fake.len() + 3 - 20; // stripping would leave 20 bytes of 0xFF
        fake.push(u8::try_from(n).unwrap());
        fake.extend_from_slice(&DAVE_MARKER);
        let (pkt, x) = extract_built(&RtpBuild::new(1, 1), &fake);
        let x = x.expect("kept");
        assert!(!x.dave_stripped);
        assert_eq!(&pkt[x.range], &fake[..]);

        // Size byte out of range (< 11 or ≥ body length): kept.
        for size in [Some(0u8), Some(10), None] {
            let mut p = sample_opus();
            p.extend_from_slice(&[0u8; 8]);
            let body_len = p.len() + 3;
            p.push(size.unwrap_or(u8::try_from(body_len).unwrap()));
            p.extend_from_slice(&DAVE_MARKER);
            let (pkt, x) = extract_built(&RtpBuild::new(1, 1), &p);
            let x = x.expect("kept");
            assert!(!x.dave_stripped, "size {size:?}");
            assert_eq!(&pkt[x.range], &p[..]);
        }
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        assert_eq!(extract_opus(&[], 0, 0), None);
        assert_eq!(extract_opus(&[0x80; 5], 0, 1), None);
        let opus = sample_opus();
        let (pkt, off, end) = RtpBuild::new(1, 1).build(&opus, SUFFIX);
        // End beyond the packet, start at/after end, overflowing offsets.
        assert_eq!(extract_opus(&pkt, off, pkt.len()), None);
        assert_eq!(extract_opus(&pkt, end, end), None);
        assert_eq!(extract_opus(&pkt, usize::MAX, usize::MAX), None);
        assert_eq!(extract_opus(&pkt, 0, usize::MAX), None);
        // Extension bit set but no room for the extension header.
        let mut bad = pkt.clone();
        bad[0] |= 0x10;
        assert_eq!(extract_opus(&bad[..14], 0, 2), None);
        // Padding count larger than the payload.
        let mut padded = pkt.clone();
        padded[0] |= 0x20;
        let last = end + 12 - 1;
        padded[last] = 0xFF;
        assert_eq!(extract_opus(&padded, off, end), None);
    }

    #[test]
    fn silent_frame() {
        assert!(is_silent_frame(&SILENT_FRAME));
        assert!(!is_silent_frame(&[0xF8, 0xFF]));
        assert!(!is_silent_frame(&[0xF8, 0xFF, 0xFE, 0x00]));
    }

    /// §8.3 / §13.1: 10⁶ random packets with a fixed-seed xorshift; no panic, every returned
    /// range lies inside the packet.
    #[test]
    fn fuzz_one_million_packets() {
        let mut rng = XorShift::new(0x005E_ED0F_0FF5);
        let opus = sample_opus();
        let mut buf = Vec::with_capacity(1600);
        let mut accepted = 0u32;
        for i in 0..1_000_000u32 {
            buf.clear();
            match i % 4 {
                // Fully random bytes.
                0 => {
                    let len = (rng.next() % 1500) as usize;
                    buf.extend((0..len).map(|_| rng.next() as u8));
                }
                // A valid header with random flags, CSRCs and body.
                1 | 2 => {
                    let cc = (rng.next() % 16) as u8;
                    let flags = (rng.next() & 0x30) as u8;
                    buf.push(0x80 | flags | cc);
                    buf.push(0x78);
                    let len = 10 + (rng.next() % 1400) as usize;
                    buf.extend((0..len).map(|_| rng.next() as u8));
                }
                // A real packet with random corruption and DAVE-like tails.
                _ => {
                    let b = RtpBuild {
                        ext_words: (rng.next() % 3) as usize,
                        padding: (rng.next() % 4) as u8,
                        dave_nonce: rng.next().is_multiple_of(2).then(|| rng.next() % 100_000),
                        ..RtpBuild::new(rng.next() as u16, rng.next() as u32)
                    };
                    let (pkt, _, _) = b.build(&opus, (rng.next() % 24) as usize);
                    buf.extend_from_slice(&pkt);
                    for _ in 0..(rng.next() % 4) {
                        if !buf.is_empty() {
                            let at = (rng.next() as usize) % buf.len();
                            buf[at] = rng.next() as u8;
                        }
                    }
                    if rng.next().is_multiple_of(3) {
                        buf.extend_from_slice(&[0u8; 8]);
                        buf.push((rng.next() % 64) as u8);
                        buf.extend_from_slice(&DAVE_MARKER);
                    }
                }
            }
            let len = buf.len();
            let (off, end) = match rng.next() % 4 {
                0 => (0, len.saturating_sub(12)),
                1 => (
                    (rng.next() % 32) as usize,
                    len.saturating_sub((rng.next() % 40) as usize),
                ),
                2 => (rng.next() as usize, rng.next() as usize),
                _ => ((rng.next() % 2000) as usize, (rng.next() % 2000) as usize),
            };
            if let Some(x) = extract(&buf, off, end) {
                assert!(x.range.start < x.range.end && x.range.end <= len, "{i}");
                accepted += 1;
            }
        }
        assert!(
            accepted > 1_000,
            "the fuzzer should also reach the accepting paths"
        );
    }
}
