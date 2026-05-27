//! Per-measurement confidence: how much to trust a colorimeter reading.
//!
//! A reading's trustworthiness has two failure modes, both worst near black:
//!
//! * **The black-cal floor.** `raw_to_xyz` subtracts the per-channel black-cal
//!   `s5` and clamps at zero (`max(0, raw − s5)`). As a channel approaches its
//!   floor, that clamp rectifies noise and biases the result upward. `floor_σ`
//!   measures how many noise-σ the dimmest *signal* channel sits above its floor.
//!
//! * **Quantization.** Raw counts are integers. At low light a channel sits at a
//!   handful of counts, so ±½-count discretization is a large *relative*
//!   uncertainty — one that repeat-variance can't see when the counts happen to
//!   agree across repeats. We fold a quantization floor (`q/√12` per signal
//!   channel) into both the luminance and chromaticity uncertainties.
//!
//! Chromaticity (u'v') degrades far faster than luminance toward black, because
//! a fixed XYZ wobble swings the ratios that define chromaticity more when
//! `X + Y + Z` is small — so the two are reported separately.
//!
//! Repeats reduce *temporal* noise but not the quantization floor; only longer
//! integration (more photons) can lift the counts. So near-black chromaticity
//! is irreducibly uncertain at a fixed exposure — this module reports that
//! honestly rather than hiding it.

use crate::measurement::{Calibration, RawMeasurement, Setup, Xyz, raw_to_xyz};

/// 1σ-equivalent uncertainty of a single integer count (uniform quantization
/// noise, `q/√12`). Used as a noise floor that repeat-variance can't reveal:
/// when the integer counts happen to agree across repeats, the spread reads as
/// zero even though the reading is only good to ±½ count.
pub const QUANT_SIGMA: f64 = 0.288_675_13; // 1.0 / 12_f64.sqrt()

/// `floor_σ` below this ⇒ a signal channel is close enough to its black-cal
/// floor that the `max(0, raw − s5)` clamp is starting to bias the reading.
pub const FLOOR_SIGMA_MIN: f64 = 3.0;

/// Relative luminance uncertainty (σY/Y) above this reads as noisy.
pub const NOISY_REL: f64 = 0.05;

/// Chromaticity uncertainty (Δu'v') above this is roughly perceptible.
pub const DUV_PERCEPTIBLE: f64 = 0.003;

/// A reason a reading is less than fully trustworthy. Thresholds are the
/// `*_MIN` / `*_REL` / `*_PERCEPTIBLE` consts in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustFlag {
    /// A signal channel is near (or at) its black-cal floor — clamp bias.
    Floor,
    /// Relative luminance uncertainty exceeds [`NOISY_REL`].
    Noisy,
    /// Chromaticity uncertainty exceeds [`DUV_PERCEPTIBLE`].
    Chroma,
}

/// Confidence statistics for a set of repeated measurements taken at one fixed
/// operating point (same code value / aim). Built with
/// [`from_repeats`](MeasurementConfidence::from_repeats).
#[derive(Debug, Clone)]
pub struct MeasurementConfidence {
    /// Number of repeats this was computed from.
    pub n: usize,
    /// Mean measured XYZ across the repeats (full raw→XYZ pipeline).
    pub mean: Xyz,
    /// Per-channel mean / sample-std of the raw sensor counts.
    pub raw_mean: [f64; 6],
    pub raw_std: [f64; 6],
    /// Per-channel black-cal-corrected counts, `max(0, raw_mean − s5)`.
    pub corrected: [f64; 6],
    /// Which channels carry signal: corrected count above ~1% of the brightest
    /// channel and at least one count. The Spyder's dark/unused channels fall
    /// out here and are kept out of every trust metric.
    pub is_signal: [bool; 6],
    /// Per-channel headroom above the black-cal floor in noise-σ units,
    /// `corrected / hypot(raw_std, QUANT_SIGMA)`.
    pub floor_sigma: [f64; 6],
    /// The worst *signal* channel's `floor_sigma` — what limits trust. `0` when
    /// nothing carries signal.
    pub min_floor_sigma: f64,
    /// Brightest channel's corrected count: the overall signal level in counts.
    pub max_corrected: f64,
    /// Temporal (repeat) σ of Y, and the quantization-floor σ that repeats
    /// can't see. [`y_std`](Self::y_std) combines them in quadrature.
    pub y_repeat_std: f64,
    pub y_quant_std: f64,
    /// Chromaticity (u'v') uncertainty split: temporal repeat scatter and the
    /// quantization floor. `chroma_defined` is false at true black, where
    /// chromaticity has no meaning.
    pub uv_temporal: f64,
    pub uv_quant: f64,
    pub chroma_defined: bool,
}

impl MeasurementConfidence {
    /// Compute confidence for `raws`, a set of repeated raw measurements taken
    /// at one operating point with the given `setup` and calibration `cal`.
    pub fn from_repeats(raws: &[RawMeasurement], setup: &Setup, cal: &Calibration) -> Self {
        let mut raw_mean = [0.0; 6];
        let mut raw_std = [0.0; 6];
        for ch in 0..6 {
            let vals: Vec<f64> = raws.iter().map(|r| r.0[ch] as f64).collect();
            raw_mean[ch] = mean(&vals);
            raw_std[ch] = sample_std(&vals, raw_mean[ch]);
        }

        // Black-cal-corrected counts and the signal-channel mask. A channel
        // counts as signal only if it rises meaningfully above its own floor
        // (>1% of the brightest channel, and ≥1 count); this drops the dark
        // channels that otherwise pin every floor metric at zero.
        let mut corrected = [0.0; 6];
        for ch in 0..6 {
            corrected[ch] = (raw_mean[ch] - setup.s5[ch] as f64).max(0.0);
        }
        let max_corrected = corrected.iter().copied().fold(0.0, f64::max);
        let signal_threshold = (max_corrected * 0.01).max(1.0);
        let mut is_signal = [false; 6];
        for ch in 0..6 {
            is_signal[ch] = corrected[ch] >= signal_threshold;
        }

        // Headroom above the floor in σ-units (repeat noise ⊕ quantization),
        // limited by the worst signal channel.
        let mut floor_sigma = [0.0; 6];
        let mut min_floor_sigma = f64::INFINITY;
        for ch in 0..6 {
            floor_sigma[ch] = corrected[ch] / raw_std[ch].hypot(QUANT_SIGMA);
            if is_signal[ch] {
                min_floor_sigma = min_floor_sigma.min(floor_sigma[ch]);
            }
        }
        if !min_floor_sigma.is_finite() {
            min_floor_sigma = 0.0; // nothing above the floor — we're measuring it
        }

        // Full raw→XYZ pipeline per repeat, reused for luminance and chromaticity.
        let xyzs: Vec<Xyz> = raws.iter().map(|r| raw_to_xyz(r, setup, cal)).collect();
        let xs: Vec<f64> = xyzs.iter().map(|p| p.x).collect();
        let ys: Vec<f64> = xyzs.iter().map(|p| p.y).collect();
        let zs: Vec<f64> = xyzs.iter().map(|p| p.z).collect();
        let mean_xyz = Xyz {
            x: mean(&xs),
            y: mean(&ys),
            z: mean(&zs),
        };
        let y_repeat_std = sample_std(&ys, mean_xyz.y);

        // A ½-count quantum on channel `ch` displaces XYZ along that channel's
        // matrix column (× gain) — the gradient ∂XYZ/∂count_ch. Used for both
        // the Y floor and (through the nonlinear u'v' map) the chromaticity floor.
        let xyz_grad = |ch: usize| {
            (
                cal.matrix[0][ch] * cal.gain[0],
                cal.matrix[1][ch] * cal.gain[1],
                cal.matrix[2][ch] * cal.gain[2],
            )
        };

        // Quantization floor on Y: the quantum on each signal channel through
        // the Y gradient. Repeats can't reveal this when the counts agree.
        let mut quant_var = 0.0;
        for (ch, &signal) in is_signal.iter().enumerate() {
            if signal {
                let (_, gy, _) = xyz_grad(ch);
                quant_var += (gy * QUANT_SIGMA).powi(2);
            }
        }
        let y_quant_std = quant_var.sqrt();

        // Chromaticity (u'v') uncertainty. Temporal: RMS scatter of the
        // per-repeat u'v' points about their mean. Quantization: each signal
        // channel's quantum nudges XYZ, and the u'v' displacement is summed in
        // quadrature — this is what blows up near black.
        let (uv_temporal, uv_quant, chroma_defined) = match mean_xyz.uv_prime() {
            None => (0.0, 0.0, false),
            Some((u0, v0)) => {
                let uvs: Vec<(f64, f64)> = xyzs.iter().filter_map(|p| p.uv_prime()).collect();
                let uv_temporal = if uvs.len() >= 2 {
                    let nn = uvs.len();
                    let um = uvs.iter().map(|p| p.0).sum::<f64>() / nn as f64;
                    let vm = uvs.iter().map(|p| p.1).sum::<f64>() / nn as f64;
                    let ss: f64 = uvs
                        .iter()
                        .map(|&(u, v)| (u - um).powi(2) + (v - vm).powi(2))
                        .sum();
                    (ss / (nn - 1) as f64).sqrt()
                } else {
                    0.0
                };
                let mut quant_uv_var = 0.0;
                for (ch, &signal) in is_signal.iter().enumerate() {
                    if signal {
                        let (gx, gy, gz) = xyz_grad(ch);
                        let perturbed = Xyz {
                            x: mean_xyz.x + gx * QUANT_SIGMA,
                            y: mean_xyz.y + gy * QUANT_SIGMA,
                            z: mean_xyz.z + gz * QUANT_SIGMA,
                        };
                        if let Some((u1, v1)) = perturbed.uv_prime() {
                            quant_uv_var += (u1 - u0).powi(2) + (v1 - v0).powi(2);
                        }
                    }
                }
                (uv_temporal, quant_uv_var.sqrt(), true)
            }
        };

        MeasurementConfidence {
            n: raws.len(),
            mean: mean_xyz,
            raw_mean,
            raw_std,
            corrected,
            is_signal,
            floor_sigma,
            min_floor_sigma,
            max_corrected,
            y_repeat_std,
            y_quant_std,
            uv_temporal,
            uv_quant,
            chroma_defined,
        }
    }

    /// Combined Y uncertainty: temporal repeat noise ⊕ quantization floor.
    pub fn y_std(&self) -> f64 {
        self.y_repeat_std.hypot(self.y_quant_std)
    }

    /// Relative luminance uncertainty σY/Y (a fraction); `inf` at true black.
    pub fn y_rel_uncertainty(&self) -> f64 {
        if self.mean.y.abs() < 1e-9 {
            f64::INFINITY
        } else {
            self.y_std() / self.mean.y
        }
    }

    /// Combined chromaticity uncertainty Δu'v': temporal ⊕ quantization. `None`
    /// when chromaticity is undefined (true black).
    pub fn uv_std(&self) -> Option<f64> {
        self.chroma_defined
            .then(|| self.uv_temporal.hypot(self.uv_quant))
    }

    /// Trust flags raised at the default thresholds. Empty ⇒ fully trustworthy.
    pub fn flags(&self) -> Vec<TrustFlag> {
        let mut f = Vec::new();
        // Near the black-cal floor: either nothing carries usable signal, or the
        // limiting signal channel is within a few σ of its floor.
        if self.max_corrected < 1.0 || self.min_floor_sigma < FLOOR_SIGMA_MIN {
            f.push(TrustFlag::Floor);
        }
        if self.y_rel_uncertainty() > NOISY_REL {
            f.push(TrustFlag::Noisy);
        }
        if self.uv_std().is_some_and(|d| d > DUV_PERCEPTIBLE) {
            f.push(TrustFlag::Chroma);
        }
        f
    }

    /// True if no trust flags are raised at the default thresholds.
    pub fn is_trustworthy(&self) -> bool {
        self.flags().is_empty()
    }
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

/// Sample standard deviation (Bessel-corrected, `n−1`); `0` for fewer than two
/// samples.
fn sample_std(v: &[f64], mean: f64) -> f64 {
    if v.len() < 2 {
        return 0.0;
    }
    let var = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (v.len() - 1) as f64;
    var.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_and_std_basic() {
        let v = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let m = mean(&v);
        assert!((m - 5.0).abs() < 1e-12);
        // Sample (n−1) std of this classic set is 2.138...
        assert!((sample_std(&v, m) - 2.1380899).abs() < 1e-6);
    }

    #[test]
    fn std_degenerate_cases() {
        assert_eq!(mean(&[]), 0.0);
        assert_eq!(sample_std(&[], 0.0), 0.0);
        assert_eq!(sample_std(&[3.0], 3.0), 0.0); // need ≥2 samples
        assert_eq!(sample_std(&[5.0, 5.0, 5.0], 5.0), 0.0); // no spread
    }

    /// Dark/unused channels must not poison the verdict: a reading whose live
    /// channels are well above the floor is trustworthy even with some channels
    /// pinned at their floor. A reading where *everything* sits at the floor
    /// carries no signal and flags Floor.
    #[test]
    fn excludes_dark_channels_and_flags_floor() {
        let cal = unit_cal();
        let setup = setup_with_floor([20; 6]);

        // ch0–2 well above the floor; ch3–5 pinned at it (dark).
        let bright = vec![
            RawMeasurement([200, 200, 200, 20, 20, 20]),
            RawMeasurement([202, 198, 201, 20, 20, 20]),
            RawMeasurement([198, 202, 199, 20, 20, 20]),
        ];
        let c = MeasurementConfidence::from_repeats(&bright, &setup, &cal);
        assert_eq!(c.is_signal, [true, true, true, false, false, false]);
        assert!(c.min_floor_sigma > FLOOR_SIGMA_MIN);
        assert!(c.is_trustworthy());

        // Every channel at/below its floor: nothing carries signal → Floor.
        let dark = vec![
            RawMeasurement([20, 18, 21, 19, 20, 22]),
            RawMeasurement([19, 21, 20, 18, 21, 19]),
            RawMeasurement([21, 19, 19, 20, 20, 20]),
        ];
        let c = MeasurementConfidence::from_repeats(&dark, &setup, &cal);
        assert!(
            c.is_signal.iter().all(|&s| !s),
            "no channel should count as signal at the floor"
        );
        assert!(c.flags().contains(&TrustFlag::Floor));
    }

    /// The chromaticity quantization floor must rise as the signal drops: the
    /// same ½-count quantum swings u'v' far more when X+Y+Z is small. Same
    /// chromaticity, 10× dimmer ⇒ markedly larger Δu'v'.
    #[test]
    fn chromaticity_uncertainty_grows_toward_black() {
        let cal = xyz_passthrough_cal();
        let setup = setup_with_floor([0; 6]);
        // Stable repeats (no temporal spread) so we isolate the quant floor.
        let bright = vec![RawMeasurement([100, 90, 80, 0, 0, 0]); 4];
        let dark = vec![RawMeasurement([10, 9, 8, 0, 0, 0]); 4];
        let b = MeasurementConfidence::from_repeats(&bright, &setup, &cal)
            .uv_std()
            .unwrap();
        let d = MeasurementConfidence::from_repeats(&dark, &setup, &cal)
            .uv_std()
            .unwrap();
        assert!(b > 0.0 && d > 0.0);
        assert!(
            d > 2.0 * b,
            "chromaticity floor should climb toward black (bright {b}, dark {d})"
        );
    }

    /// Channels map straight to X/Y/Z (ch0→X, ch1→Y, ch2→Z) so chromaticity
    /// is well-defined and exercised in tests.
    fn xyz_passthrough_cal() -> Calibration {
        let mut matrix = [[0.0; 6]; 3];
        matrix[0][0] = 1.0;
        matrix[1][1] = 1.0;
        matrix[2][2] = 1.0;
        Calibration {
            index: 0,
            v1: 0,
            v2: 0,
            v4: [0; 6],
            matrix,
            gain: [1.0; 3],
            offset: [0.0; 3],
            v3: 0,
        }
    }

    /// Y = sum of corrected counts (row 1 all ones); X/Z rows zero.
    fn unit_cal() -> Calibration {
        let mut matrix = [[0.0; 6]; 3];
        matrix[1] = [1.0; 6];
        Calibration {
            index: 0,
            v1: 0,
            v2: 0,
            v4: [0; 6],
            matrix,
            gain: [1.0; 3],
            offset: [0.0; 3],
            v3: 0,
        }
    }

    fn setup_with_floor(s5: [u8; 6]) -> Setup {
        Setup {
            s1: 0,
            s2: 0,
            s3: [0; 6],
            s4: [0; 6],
            s5,
        }
    }
}
