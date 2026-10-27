//! Signal level measurement (chunk `level`, voice threshold).

/// The floor reported for silence and for an empty buffer.
pub const LEVEL_FLOOR_DBFS: f32 = -100.0;

/// RMS level in dBFS (floored at −100) and absolute peak (full scale = 1.0).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Level {
    pub rms_dbfs: f32,
    pub peak: f32,
}

/// Measures `samples` (any channel layout). NaN samples are ignored for the peak.
pub fn measure(samples: &[f32]) -> Level {
    if samples.is_empty() {
        return Level {
            rms_dbfs: LEVEL_FLOOR_DBFS,
            peak: 0.0,
        };
    }
    let (mut sum, mut peak) = (0.0f64, 0.0f32);
    for s in samples {
        sum += f64::from(*s) * f64::from(*s);
        peak = peak.max(s.abs());
    }
    let rms = (sum / samples.len() as f64).sqrt();
    let db = if rms > 0.0 {
        20.0 * rms.log10()
    } else {
        f64::NEG_INFINITY
    };
    Level {
        rms_dbfs: (db as f32).max(LEVEL_FLOOR_DBFS),
        peak,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_and_empty() {
        assert_eq!(measure(&[]).rms_dbfs, LEVEL_FLOOR_DBFS);
        let l = measure(&[0.0; 960]);
        assert_eq!(l.rms_dbfs, LEVEL_FLOOR_DBFS);
        assert_eq!(l.peak, 0.0);
    }

    #[test]
    fn full_scale_square_and_sine() {
        let sq: Vec<f32> = (0..960)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let l = measure(&sq);
        assert!(l.rms_dbfs.abs() < 1e-4);
        assert_eq!(l.peak, 1.0);
        let sine: Vec<f32> = (0..48_000)
            .map(|i| 0.1 * (2.0 * std::f32::consts::PI * 1_000.0 * i as f32 / 48_000.0).sin())
            .collect();
        let l = measure(&sine);
        // A sine at 0.1 peak is −20 dBFS peak, −23.01 dBFS RMS.
        assert!((l.rms_dbfs + 23.0103).abs() < 0.01, "{}", l.rms_dbfs);
        assert!((l.peak - 0.1).abs() < 1e-4);
    }
}
