//! The lock-free half of `PcmQueue::write` (W4, §7.3 step 1): the size pre-check and parsing at
//! the input rate, all **outside** every lock.

use crate::error::CoreError;
use crate::format::PcmParser;
use crate::limits::{LimitError, LimitNum};

/// A write parsed outside the locks, ready for the L6w splice and the L6 commit.
#[derive(Debug, Default)]
pub(crate) struct ParsedWrite {
    /// Interleaved i16 (exactly sized). With `head_slot`, the first `channels` samples are a
    /// placeholder for the frame completed by the carry splice; the whole frames of the write
    /// follow.
    pub(crate) samples: Box<[i16]>,
    /// Whether `samples` starts with the head-frame placeholder.
    pub(crate) head_slot: bool,
    /// Bytes completing the carried partial frame (≤ 7).
    pub(crate) head: [u8; 8],
    pub(crate) head_len: u8,
    /// Trailing partial frame: the tentative new carry (≤ 7 bytes).
    pub(crate) tail: [u8; 8],
    pub(crate) tail_len: u8,
    /// The carry length this parse assumed (`st.carry_len` snapshot).
    pub(crate) carry_snapshot: u8,
    /// Whole frames parsed from the write itself (excluding the head frame).
    pub(crate) body_frames: u64,
}

impl ParsedWrite {
    #[cfg(test)]
    pub(crate) fn head(&self) -> &[u8] {
        self.head.get(..usize::from(self.head_len)).unwrap_or(&[])
    }

    pub(crate) fn tail(&self) -> &[u8] {
        self.tail.get(..usize::from(self.tail_len)).unwrap_or(&[])
    }
}

/// Size cap check before any allocation (`LimitExceeded`), then parse.
pub(crate) fn parse_write(
    parser: &PcmParser,
    data: &[u8],
    carry_len: u8,
    cap_bytes: u64,
) -> Result<ParsedWrite, CoreError> {
    let size = data.len() as u64;
    if size > cap_bytes {
        return Err(CoreError::Limit(LimitError {
            what: "write_bytes",
            value: LimitNum::Int(i128::from(size)),
            min: LimitNum::Int(0),
            max: LimitNum::Int(i128::from(cap_bytes)),
        }));
    }
    parse_raw(parser, data, carry_len)
}

/// Parses raw wire bytes assuming a carried partial frame of `carry_len` bytes.
pub(crate) fn parse_raw(
    parser: &PcmParser,
    src: &[u8],
    carry_len: u8,
) -> Result<ParsedWrite, CoreError> {
    let fmt = parser.format();
    let fb = fmt.frame_bytes();
    let ch = usize::from(fmt.channels);
    let k = if carry_len == 0 {
        0
    } else {
        parser.head_len(usize::from(carry_len))
    };
    let mut out = ParsedWrite {
        carry_snapshot: carry_len,
        ..ParsedWrite::default()
    };
    if k > 0 && src.len() < k {
        // The write only extends the carried partial frame.
        out.head_len = u8::try_from(src.len()).unwrap_or(0);
        if let Some(dst) = out.head.get_mut(..src.len()) {
            dst.copy_from_slice(src);
        }
        return Ok(out);
    }
    let (head, body) = src.split_at(k.min(src.len()));
    out.head_len = u8::try_from(head.len()).unwrap_or(0);
    if let Some(dst) = out.head.get_mut(..head.len()) {
        dst.copy_from_slice(head);
    }
    let body_frames = body.len() / fb;
    out.head_slot = k > 0;
    let total = (body_frames + usize::from(out.head_slot)).saturating_mul(ch);
    let mut samples: Vec<i16> = Vec::new();
    samples.try_reserve_exact(total)?;
    if out.head_slot {
        samples.extend(std::iter::repeat_n(0i16, ch));
    }
    let (frames, trailing) = parser.parse_frames(body, &mut samples)?;
    out.body_frames = frames as u64;
    let tail = body.get(body.len() - trailing..).unwrap_or(&[]);
    out.tail_len = u8::try_from(tail.len()).unwrap_or(0);
    if let Some(dst) = out.tail.get_mut(..tail.len()) {
        dst.copy_from_slice(tail);
    }
    out.samples = samples.into_boxed_slice();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{F32_48K_STEREO, PCM16_24K, PCM16_48K_STEREO};

    #[test]
    fn whole_frames_and_tail() {
        let p = PcmParser::new(PCM16_24K);
        let w = parse_raw(&p, &[1, 0, 2, 0, 3], 0).unwrap();
        assert!(!w.head_slot);
        assert_eq!(&*w.samples, &[1, 2]);
        assert_eq!(w.tail(), &[3]);
        assert_eq!(w.body_frames, 2);
    }

    #[test]
    fn head_completes_carry() {
        let p = PcmParser::new(PCM16_48K_STEREO); // 4-byte frames
        let w = parse_raw(&p, &[9, 9, 9, 1, 0, 2, 0, 7], 1).unwrap();
        assert!(w.head_slot);
        assert_eq!(w.head(), &[9, 9, 9]);
        assert_eq!(&*w.samples, &[0, 0, 1, 2]);
        assert_eq!(w.tail(), &[7]);
        // Shorter than the missing head: everything is head.
        let w = parse_raw(&p, &[5, 6], 1).unwrap();
        assert!(!w.head_slot);
        assert_eq!(w.head(), &[5, 6]);
        assert!(w.samples.is_empty());
    }

    #[test]
    fn cap_checked_before_parsing() {
        let p = PcmParser::new(F32_48K_STEREO);
        let data = vec![0u8; 2_001];
        // 2001 bytes > cap of 2000: rejected without allocating.
        let _g = crate::alloc_count::CountGuard::new();
        let before = crate::alloc_count::count();
        let r = parse_write(&p, &data, 0, 2_000);
        let after = crate::alloc_count::count();
        assert!(matches!(r, Err(CoreError::Limit(_))));
        assert_eq!(after, before, "no allocation before the cap check");
        let w = parse_write(&p, &data[..2_000], 0, 2_000).unwrap();
        assert_eq!(w.body_frames, 250);
    }

    #[test]
    fn parse_write_parses_raw_bytes() {
        let p = PcmParser::new(PCM16_24K);
        let w = parse_write(&p, &[1, 0, 2, 0], 0, 1_000).unwrap();
        assert_eq!(&*w.samples, &[1, 2]);
    }
}
