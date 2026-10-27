//! G.711 µ-law / A-law, bit-exact with Sun Microsystems' public-domain `g711.c` (16-bit variant).
//!
//! Encoders compute the segment from the bit length instead of the reference linear search;
//! decoders are 256-entry tables built at compile time. Tests compare both against a literal port.

/// µ-law digital silence.
pub const ULAW_SILENCE: u8 = 0xFF;
/// A-law digital silence (decodes to +8).
pub const ALAW_SILENCE: u8 = 0xD5;

const BIAS: i32 = 0x84;
const ULAW_CLIP: i32 = 8159;

#[inline]
fn bit_len(x: i32) -> i32 {
    32 - x.leading_zeros() as i32
}

/// Linear 16-bit PCM → µ-law (`linear2ulaw`).
#[inline]
pub fn ulaw_encode(pcm: i16) -> u8 {
    let mut x = i32::from(pcm) >> 2;
    let mask: u8 = if x < 0 {
        x = -x;
        0x7F
    } else {
        0xFF
    };
    if x > ULAW_CLIP {
        x = ULAW_CLIP;
    }
    x += BIAS >> 2;
    // search(x, seg_uend = {0x3F, 0x7F, …, 0x1FFF}) == max(0, bit_len(x) - 6)
    let seg = (bit_len(x) - 6).max(0);
    if seg >= 8 {
        return 0x7F ^ mask;
    }
    let uval = ((seg << 4) | ((x >> (seg + 1)) & 0xF)) as u8;
    uval ^ mask
}

/// Linear 16-bit PCM → A-law (`linear2alaw`).
#[inline]
pub fn alaw_encode(pcm: i16) -> u8 {
    let mut x = i32::from(pcm) >> 3;
    let mask: u8 = if x >= 0 {
        0xD5
    } else {
        x = -x - 1;
        0x55
    };
    // search(x, seg_aend = {0x1F, 0x3F, …, 0xFFF}) == max(0, bit_len(x) - 5)
    let seg = (bit_len(x) - 5).max(0);
    if seg >= 8 {
        return 0x7F ^ mask;
    }
    let quant = if seg < 2 {
        (x >> 1) & 0xF
    } else {
        (x >> seg) & 0xF
    };
    (((seg << 4) | quant) as u8) ^ mask
}

const fn ulaw2linear(u: u8) -> i16 {
    let u = !u;
    let mut t: i32 = (((u & 0x0F) as i32) << 3) + BIAS;
    t <<= ((u & 0x70) >> 4) as i32;
    (if u & 0x80 != 0 { BIAS - t } else { t - BIAS }) as i16
}

const fn alaw2linear(a: u8) -> i16 {
    let a = a ^ 0x55;
    let mut t: i32 = ((a & 0x0F) as i32) << 4;
    let seg = ((a & 0x70) >> 4) as i32;
    match seg {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= seg - 1;
        }
    }
    (if a & 0x80 != 0 { t } else { -t }) as i16
}

// The loop index is bounded by the table length.
#[allow(clippy::indexing_slicing)]
const fn build(alaw: bool) -> [i16; 256] {
    let mut t = [0i16; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = if alaw {
            alaw2linear(i as u8)
        } else {
            ulaw2linear(i as u8)
        };
        i += 1;
    }
    t
}

/// µ-law → linear 16-bit PCM.
pub static ULAW_DECODE: [i16; 256] = build(false);
/// A-law → linear 16-bit PCM.
pub static ALAW_DECODE: [i16; 256] = build(true);

#[inline]
pub fn ulaw_decode(b: u8) -> i16 {
    ULAW_DECODE.get(usize::from(b)).copied().unwrap_or(0)
}

#[inline]
pub fn alaw_decode(b: u8) -> i16 {
    ALAW_DECODE.get(usize::from(b)).copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A literal port of Sun's g711.c (search-based), kept independent of the code above.
    mod sun {
        const SEG_AEND: [i32; 8] = [0x1F, 0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF];
        const SEG_UEND: [i32; 8] = [0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF];

        fn search(val: i32, table: &[i32; 8]) -> i32 {
            for (i, v) in table.iter().enumerate() {
                if val <= *v {
                    return i as i32;
                }
            }
            8
        }

        pub fn linear2alaw(pcm_val: i16) -> u8 {
            let mut pcm_val = (pcm_val >> 3) as i32;
            let mask: i32;
            if pcm_val >= 0 {
                mask = 0xD5;
            } else {
                mask = 0x55;
                pcm_val = -pcm_val - 1;
            }
            let seg = search(pcm_val, &SEG_AEND);
            if seg >= 8 {
                (0x7F ^ mask) as u8
            } else {
                let mut aval = (seg << 4) as u8;
                if seg < 2 {
                    aval |= ((pcm_val >> 1) & 0xF) as u8;
                } else {
                    aval |= ((pcm_val >> seg) & 0xF) as u8;
                }
                aval ^ mask as u8
            }
        }

        pub fn alaw2linear(a_val: u8) -> i16 {
            let a_val = a_val ^ 0x55;
            let mut t: i32 = ((a_val & 0xF) as i32) << 4;
            let seg = ((a_val as u32 & 0x70) >> 4) as i32;
            match seg {
                0 => t += 8,
                1 => t += 0x108,
                _ => {
                    t += 0x108;
                    t <<= seg - 1;
                }
            }
            if a_val & 0x80 != 0 {
                t as i16
            } else {
                -t as i16
            }
        }

        pub fn linear2ulaw(pcm_val: i16) -> u8 {
            let mut pcm_val = (pcm_val >> 2) as i32;
            let mask: i32;
            if pcm_val < 0 {
                pcm_val = -pcm_val;
                mask = 0x7F;
            } else {
                mask = 0xFF;
            }
            if pcm_val > 8159 {
                pcm_val = 8159;
            }
            pcm_val += 0x84 >> 2;
            let seg = search(pcm_val, &SEG_UEND);
            if seg >= 8 {
                (0x7F ^ mask) as u8
            } else {
                let uval = ((seg << 4) | ((pcm_val >> (seg + 1)) & 0xF)) as u8;
                uval ^ mask as u8
            }
        }

        pub fn ulaw2linear(u_val: u8) -> i16 {
            let u_val = !u_val;
            let mut t: i32 = (((u_val & 0xF) as i32) << 3) + 0x84;
            t <<= (u_val as u32 & 0x70) >> 4;
            if u_val & 0x80 != 0 {
                (0x84 - t) as i16
            } else {
                (t - 0x84) as i16
            }
        }
    }

    #[test]
    fn all_encodes_match_reference() {
        for x in i16::MIN..=i16::MAX {
            assert_eq!(ulaw_encode(x), sun::linear2ulaw(x), "ulaw {x}");
            assert_eq!(alaw_encode(x), sun::linear2alaw(x), "alaw {x}");
        }
    }

    #[test]
    fn all_decodes_match_reference() {
        for b in 0..=255u8 {
            assert_eq!(
                ULAW_DECODE[usize::from(b)],
                sun::ulaw2linear(b),
                "ulaw {b:#x}"
            );
            assert_eq!(
                ALAW_DECODE[usize::from(b)],
                sun::alaw2linear(b),
                "alaw {b:#x}"
            );
            assert_eq!(ulaw_decode(b), sun::ulaw2linear(b));
            assert_eq!(alaw_decode(b), sun::alaw2linear(b));
        }
    }

    #[test]
    fn silence_bytes() {
        assert_eq!(ulaw_encode(0), ULAW_SILENCE);
        assert_eq!(alaw_encode(0), ALAW_SILENCE);
        assert_eq!(ulaw_decode(ULAW_SILENCE), 0);
        assert_eq!(alaw_decode(ALAW_SILENCE), 8);
        // Decode → encode is the identity on every code word except µ-law's negative zero (0x7F).
        for b in 0..=255u8 {
            if b != 0x7F {
                assert_eq!(ulaw_encode(ulaw_decode(b)), b, "ulaw {b:#x}");
            }
            assert_eq!(alaw_encode(alaw_decode(b)), b, "alaw {b:#x}");
        }
    }

    fn snr_db(encode: fn(i16) -> u8, decode: fn(u8) -> i16) -> f64 {
        let amp = 32768.0 * 10f64.powf(-20.0 / 20.0);
        let (mut sig, mut err) = (0.0f64, 0.0f64);
        for n in 0..8_000 {
            let x =
                (amp * (2.0 * std::f64::consts::PI * 1_011.0 * n as f64 / 8_000.0).sin()).round();
            let y = f64::from(decode(encode(x as i16)));
            sig += x * x;
            err += (x - y) * (x - y);
        }
        10.0 * (sig / err).log10()
    }

    #[test]
    fn round_trip_snr_at_minus_20_dbfs() {
        let u = snr_db(ulaw_encode, ulaw_decode);
        let a = snr_db(alaw_encode, alaw_decode);
        assert!(u >= 38.0, "µ-law SNR {u:.2} dB");
        assert!(a >= 38.0, "A-law SNR {a:.2} dB");
    }
}
