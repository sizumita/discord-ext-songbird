//! Base64 (`engine::general_purpose::STANDARD`) for `to_base64` and the `write_base64*` paths.

use std::borrow::Cow;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::error::CoreError;

/// Upper bound of the decoded length of `encoded_len` base64 bytes (`⌈n/4⌉·3`).
pub fn decoded_len_estimate(encoded_len: usize) -> usize {
    encoded_len.div_ceil(4).saturating_mul(3)
}

/// Exact padded encoded length of `n` bytes, or `None` on overflow.
pub fn encoded_len(n: usize) -> Option<usize> {
    base64::encoded_len(n, true)
}

/// Appends the base64 encoding of `src` to `out`.
pub fn encode_into(src: &[u8], out: &mut String) {
    STANDARD.encode_string(src, out);
}

/// Like [`encode_into`], but reserves with `try_reserve` first (`Alloc` → `MemoryError`).
pub fn try_encode_into(src: &[u8], out: &mut String) -> Result<(), CoreError> {
    let n = encoded_len(src.len()).ok_or(CoreError::Alloc)?;
    out.try_reserve_exact(n)?;
    STANDARD.encode_string(src, out);
    Ok(())
}

/// Appends the decoded bytes to `out` (`try_reserve`). Invalid input → `InvalidAudio`, and `out` is
/// left exactly as it was (nothing consumed).
pub fn decode_into(src: &[u8], out: &mut Vec<u8>) -> Result<(), CoreError> {
    let start = out.len();
    out.try_reserve(decoded_len_estimate(src.len()))?;
    match STANDARD.decode_vec(src, out) {
        Ok(()) => Ok(()),
        Err(e) => {
            out.truncate(start);
            Err(CoreError::InvalidAudio(Cow::Owned(format!(
                "invalid base64: {e}"
            ))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for n in 0..64usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 % 251) as u8).collect();
            let mut s = String::new();
            encode_into(&data, &mut s);
            assert_eq!(s.len(), encoded_len(n).unwrap());
            let mut back = vec![0xAA];
            decode_into(s.as_bytes(), &mut back).unwrap();
            assert_eq!(&back[1..], &data[..]);
            let mut s2 = String::from(">");
            try_encode_into(&data, &mut s2).unwrap();
            assert_eq!(&s2[1..], s);
        }
        let mut s = String::new();
        encode_into(b"hello", &mut s);
        assert_eq!(s, "aGVsbG8=");
    }

    #[test]
    fn invalid_input_consumes_nothing() {
        let mut out = vec![1, 2, 3];
        for bad in [&b"####"[..], b"aGVsbG8", b"aGVs bG8=", b"=", b"aGVsbG8=="] {
            assert!(
                matches!(decode_into(bad, &mut out), Err(CoreError::InvalidAudio(_))),
                "{bad:?}"
            );
            assert_eq!(out, vec![1, 2, 3]);
        }
    }

    #[test]
    fn estimate_is_conservative() {
        for n in 0..100usize {
            let data = vec![7u8; n];
            let mut s = String::new();
            encode_into(&data, &mut s);
            assert!(decoded_len_estimate(s.len()) >= n);
            assert!(decoded_len_estimate(s.len()) < n + 3);
        }
    }
}
