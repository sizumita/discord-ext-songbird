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

/// The byte range of the Opus payload inside `pkt`, or `None` for a malformed packet.
pub fn extract_opus(pkt: &[u8], payload_offset: usize, payload_end: usize) -> Option<Range<usize>> {
    let rtp = RtpPacket::new(pkt)?;
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
    if body.len() >= 11 && body.ends_with(&[0xFA, 0xFA]) {
        // DAVE leftover (tag8 | nonce | ranges | size | FA FA).
        let n = usize::from(*body.get(body.len().checked_sub(3)?)?);
        if (11..body.len()).contains(&n) {
            let stripped = body.get(..body.len().checked_sub(n)?)?;
            if matches!(
                opus::packet::get_nb_samples(stripped, 48_000),
                Ok(120..=5760)
            ) {
                e = e.checked_sub(n)?;
            }
        }
    }
    (s < e).then_some(s..e)
}

/// Whether `p` is Opus `SILENT_FRAME` (not voiced, §8.5).
pub fn is_silent_frame(p: &[u8]) -> bool {
    p == SILENT_FRAME
}
