//! Sample conversion and channel mapping. Allocation-free; lengths are `min` of the operands.

/// One f32 sample (±1.0 full scale) → i16: scale by 32768, round half away from zero, clamp.
/// NaN maps to 0.
#[inline]
pub fn f32_sample_to_i16(x: f32) -> i16 {
    (x * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
}

/// i16 → f32 (`/ 32768`).
pub fn i16_to_f32(src: &[i16], dst: &mut [f32]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = f32::from(*s) / 32_768.0;
    }
}

/// f32 → i16 with rounding and clamping.
pub fn f32_to_i16(src: &[f32], dst: &mut [i16]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = f32_sample_to_i16(*s);
    }
}

/// Interleaved stereo → mono as `(L + R) · 0.5`. `dst.len()` frames are written (at most
/// `src.len() / 2`).
pub fn downmix_stereo(src: &[f32], dst: &mut [f32]) {
    let (frames, _) = src.as_chunks::<2>();
    for (d, [l, r]) in dst.iter_mut().zip(frames) {
        *d = (l + r) * 0.5;
    }
}

/// Mono → interleaved stereo (both channels equal).
pub fn upmix_mono(src: &[f32], dst: &mut [f32]) {
    let (frames, _) = dst.as_chunks_mut::<2>();
    for (d, s) in frames.iter_mut().zip(src) {
        *d = [*s, *s];
    }
}

/// `dst += src`, element-wise.
pub fn add_into(dst: &mut [f32], src: &[f32]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d += *s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i16_f32_round_trip() {
        let src: Vec<i16> = vec![i16::MIN, -1, 0, 1, 12_345, i16::MAX];
        let mut f = vec![0.0; src.len()];
        i16_to_f32(&src, &mut f);
        assert_eq!(f[0], -1.0);
        let mut back = vec![0; src.len()];
        f32_to_i16(&f, &mut back);
        assert_eq!(back, src);
    }

    #[test]
    fn f32_to_i16_clamps_and_rounds() {
        let mut d = [0i16; 6];
        f32_to_i16(
            &[
                2.0,
                -2.0,
                0.5 / 32_768.0,
                -0.5 / 32_768.0,
                f32::NAN,
                0.4 / 32_768.0,
            ],
            &mut d,
        );
        assert_eq!(d, [32_767, -32_768, 1, -1, 0, 0]);
    }

    #[test]
    fn identical_channel_downmix_preserves_level() {
        let stereo: Vec<f32> = (0..100)
            .flat_map(|i| {
                let x = (i as f32 * 0.1).sin() * 0.8;
                [x, x]
            })
            .collect();
        let mut mono = vec![0.0; 100];
        downmix_stereo(&stereo, &mut mono);
        for (i, m) in mono.iter().enumerate() {
            assert_eq!(*m, stereo[2 * i]);
        }
        let mut st = vec![0.0; 200];
        upmix_mono(&mono, &mut st);
        assert_eq!(st, stereo);
    }

    #[test]
    fn add_and_short_operands() {
        let mut d = vec![1.0, 2.0, 3.0];
        add_into(&mut d, &[1.0, 1.0]);
        assert_eq!(d, vec![2.0, 3.0, 3.0]);
        let mut m = vec![9.0; 3];
        downmix_stereo(&[1.0, 3.0, 5.0], &mut m);
        assert_eq!(m, vec![2.0, 9.0, 9.0]);
    }
}
