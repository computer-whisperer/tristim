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

use crate::sample::{RawRepeats, Sample, Xyz};

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
///
/// Non-exhaustive: new confidence checks may add flags; treat unknown flags
/// as "less trustworthy for a reason this build doesn't classify".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrustFlag {
    /// A signal channel is near (or at) its black-cal floor — clamp bias.
    Floor,
    /// Relative luminance uncertainty exceeds [`NOISY_REL`].
    Noisy,
    /// Chromaticity uncertainty exceeds [`DUV_PERCEPTIBLE`].
    Chroma,
}

/// Per-channel raw statistics — present only when the device exposed integer
/// sensor counts (the floor / quantization analysis the Spyder family supports).
/// XYZ-only instruments leave [`MeasurementConfidence::raw`] as `None`.
///
/// Every per-channel `Vec` shares the device's channel width.
#[derive(Debug, Clone)]
pub struct RawStats {
    /// Per-channel mean / sample-std of the raw sensor counts.
    pub raw_mean: Vec<f64>,
    pub raw_std: Vec<f64>,
    /// Per-channel black-cal-corrected counts, `max(0, raw_mean − floor)`.
    pub corrected: Vec<f64>,
    /// Which channels carry signal: corrected count above ~1% of the brightest
    /// channel and at least one count. Dark/unused channels fall out here and
    /// are kept out of every trust metric.
    pub is_signal: Vec<bool>,
    /// Per-channel headroom above the black-cal floor in noise-σ units,
    /// `corrected / hypot(raw_std, QUANT_SIGMA)`.
    pub floor_sigma: Vec<f64>,
    /// The worst *signal* channel's `floor_sigma` — what limits trust. `0` when
    /// nothing carries signal.
    pub min_floor_sigma: f64,
    /// Brightest channel's corrected count: the overall signal level in counts.
    pub max_corrected: f64,
}

/// Confidence statistics for a set of repeated measurements taken at one fixed
/// operating point (same code value / aim). Built with
/// [`from_sample`](MeasurementConfidence::from_sample).
///
/// Two paths feed it. When the [`Sample`] carries raw counts, the full
/// floor-σ + quantization analysis runs and [`raw`](Self::raw) is `Some`. When
/// the device returns XYZ only, trust rests on the XYZ-repeat scatter alone:
/// [`raw`](Self::raw) is `None`, the quantization floors are zero (no integer
/// counts to discretize), and the [`Floor`](TrustFlag::Floor) flag never fires —
/// the floor is something only raw counts can reveal.
#[derive(Debug, Clone)]
pub struct MeasurementConfidence {
    /// Number of repeats this was computed from.
    pub n: usize,
    /// Mean measured XYZ across the repeats.
    pub mean: Xyz,
    /// Temporal (repeat) σ of Y, and the quantization-floor σ that repeats
    /// can't see. [`y_std`](Self::y_std) combines them in quadrature. The
    /// quantization term is `0` on the XYZ-only path.
    pub y_repeat_std: f64,
    pub y_quant_std: f64,
    /// Chromaticity (u'v') uncertainty split: temporal repeat scatter and the
    /// quantization floor (the latter `0` on the XYZ-only path). `chroma_defined`
    /// is false at true black, where chromaticity has no meaning.
    pub uv_temporal: f64,
    pub uv_quant: f64,
    pub chroma_defined: bool,
    /// Per-channel raw statistics, `Some` only when the device exposed counts.
    pub raw: Option<RawStats>,
}

impl MeasurementConfidence {
    /// Compute confidence for a [`Sample`]: its device-computed XYZ repeats and,
    /// when present, the raw counts behind them.
    ///
    /// With raw counts, the full floor-σ + quantization analysis runs. Without
    /// them, trust rests on the XYZ-repeat scatter alone (see the type docs).
    pub fn from_sample(sample: &Sample) -> Self {
        let xyzs = &sample.xyz;
        let xs: Vec<f64> = xyzs.iter().map(|p| p.x).collect();
        let ys: Vec<f64> = xyzs.iter().map(|p| p.y).collect();
        let zs: Vec<f64> = xyzs.iter().map(|p| p.z).collect();
        let mean_xyz = Xyz {
            x: mean(&xs),
            y: mean(&ys),
            z: mean(&zs),
        };
        let y_repeat_std = sample_std(&ys, mean_xyz.y);

        // Temporal chromaticity scatter: RMS of the per-repeat u'v' points about
        // their mean. Defined whenever the mean has a chromaticity at all.
        let chroma_defined = mean_xyz.uv_prime().is_some();
        let uv_temporal = {
            let uvs: Vec<(f64, f64)> = xyzs.iter().filter_map(|p| p.uv_prime()).collect();
            if uvs.len() >= 2 {
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
            }
        };

        // The quantization floors and the per-channel raw stats only exist when
        // the device handed us integer counts.
        let (raw, y_quant_std, uv_quant) = match &sample.raw {
            None => (None, 0.0, 0.0),
            Some(rr) => raw_analysis(rr, &mean_xyz, chroma_defined),
        };

        MeasurementConfidence {
            n: sample.repeats(),
            mean: mean_xyz,
            y_repeat_std,
            y_quant_std,
            uv_temporal,
            uv_quant,
            chroma_defined,
            raw,
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
        // limiting signal channel is within a few σ of its floor. Only knowable
        // from raw counts — XYZ-only devices can't see their own floor.
        if let Some(rs) = &self.raw {
            if rs.max_corrected < 1.0 || rs.min_floor_sigma < FLOOR_SIGMA_MIN {
                f.push(TrustFlag::Floor);
            }
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

/// The raw-counts branch of [`MeasurementConfidence::from_sample`]: per-channel
/// statistics plus the Y and u'v' quantization floors that repeats can't reveal.
/// Returns `(stats, y_quant_std, uv_quant)`.
fn raw_analysis(
    rr: &RawRepeats,
    mean_xyz: &Xyz,
    chroma_defined: bool,
) -> (Option<RawStats>, f64, f64) {
    let channels = rr.channels();
    let mut raw_mean = vec![0.0; channels];
    let mut raw_std = vec![0.0; channels];
    for ch in 0..channels {
        let vals: Vec<f64> = rr.counts.iter().map(|r| r[ch] as f64).collect();
        raw_mean[ch] = mean(&vals);
        raw_std[ch] = sample_std(&vals, raw_mean[ch]);
    }

    // Black-cal-corrected counts and the signal-channel mask. A channel counts
    // as signal only if it rises meaningfully above its own floor (>1% of the
    // brightest channel, and ≥1 count); this drops the dark channels that
    // otherwise pin every floor metric at zero.
    let mut corrected = vec![0.0; channels];
    for ch in 0..channels {
        corrected[ch] = (raw_mean[ch] - rr.floor[ch]).max(0.0);
    }
    let max_corrected = corrected.iter().copied().fold(0.0, f64::max);
    let signal_threshold = (max_corrected * 0.01).max(1.0);
    let is_signal: Vec<bool> = corrected.iter().map(|&c| c >= signal_threshold).collect();

    // Headroom above the floor in σ-units (repeat noise ⊕ quantization), limited
    // by the worst signal channel.
    let mut floor_sigma = vec![0.0; channels];
    let mut min_floor_sigma = f64::INFINITY;
    for ch in 0..channels {
        floor_sigma[ch] = corrected[ch] / raw_std[ch].hypot(QUANT_SIGMA);
        if is_signal[ch] {
            min_floor_sigma = min_floor_sigma.min(floor_sigma[ch]);
        }
    }
    if !min_floor_sigma.is_finite() {
        min_floor_sigma = 0.0; // nothing above the floor — we're measuring it
    }

    // Quantization floor on Y: the ½-count quantum on each signal channel through
    // that channel's Y gradient. Repeats can't reveal this when the counts agree.
    let mut quant_var = 0.0;
    for (ch, &signal) in is_signal.iter().enumerate() {
        if signal {
            let gy = rr.grad[ch][1];
            quant_var += (gy * QUANT_SIGMA).powi(2);
        }
    }
    let y_quant_std = quant_var.sqrt();

    // Chromaticity quantization floor: each signal channel's quantum nudges XYZ,
    // and the resulting u'v' displacement is summed in quadrature — this is what
    // blows up near black.
    let uv_quant = if chroma_defined {
        let (u0, v0) = mean_xyz
            .uv_prime()
            .expect("chroma_defined implies uv_prime is Some");
        let mut quant_uv_var = 0.0;
        for (ch, &signal) in is_signal.iter().enumerate() {
            if signal {
                let g = rr.grad[ch];
                let perturbed = Xyz {
                    x: mean_xyz.x + g[0] * QUANT_SIGMA,
                    y: mean_xyz.y + g[1] * QUANT_SIGMA,
                    z: mean_xyz.z + g[2] * QUANT_SIGMA,
                };
                if let Some((u1, v1)) = perturbed.uv_prime() {
                    quant_uv_var += (u1 - u0).powi(2) + (v1 - v0).powi(2);
                }
            }
        }
        quant_uv_var.sqrt()
    } else {
        0.0
    };

    let stats = RawStats {
        raw_mean,
        raw_std,
        corrected,
        is_signal,
        floor_sigma,
        min_floor_sigma,
        max_corrected,
    };
    (Some(stats), y_quant_std, uv_quant)
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
        // Y = sum of corrected counts (every channel's Y gradient is 1); floor 20.
        let grad = [[0.0, 1.0, 0.0]; 6];
        let floor = [20.0; 6];

        // ch0–2 well above the floor; ch3–5 pinned at it (dark).
        let bright = sample_from(
            &[
                [200, 200, 200, 20, 20, 20],
                [202, 198, 201, 20, 20, 20],
                [198, 202, 199, 20, 20, 20],
            ],
            floor,
            grad,
        );
        let c = MeasurementConfidence::from_sample(&bright);
        let rs = c.raw.as_ref().expect("raw stats present for a raw sample");
        assert_eq!(rs.is_signal, [true, true, true, false, false, false]);
        assert!(rs.min_floor_sigma > FLOOR_SIGMA_MIN);
        assert!(c.is_trustworthy());

        // Every channel at/below its floor: nothing carries signal → Floor.
        let dark = sample_from(
            &[
                [20, 18, 21, 19, 20, 22],
                [19, 21, 20, 18, 21, 19],
                [21, 19, 19, 20, 20, 20],
            ],
            floor,
            grad,
        );
        let c = MeasurementConfidence::from_sample(&dark);
        let rs = c.raw.as_ref().expect("raw stats present for a raw sample");
        assert!(
            rs.is_signal.iter().all(|&s| !s),
            "no channel should count as signal at the floor"
        );
        assert!(c.flags().contains(&TrustFlag::Floor));
    }

    /// The chromaticity quantization floor must rise as the signal drops: the
    /// same ½-count quantum swings u'v' far more when X+Y+Z is small. Same
    /// chromaticity, 10× dimmer ⇒ markedly larger Δu'v'.
    #[test]
    fn chromaticity_uncertainty_grows_toward_black() {
        // ch0→X, ch1→Y, ch2→Z passthrough; ch3–5 dark. Floor 0.
        let grad = [
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
        ];
        let floor = [0.0; 6];
        // Stable repeats (no temporal spread) so we isolate the quant floor.
        let bright = sample_from(&[[100, 90, 80, 0, 0, 0]; 4], floor, grad);
        let dark = sample_from(&[[10, 9, 8, 0, 0, 0]; 4], floor, grad);
        let b = MeasurementConfidence::from_sample(&bright)
            .uv_std()
            .unwrap();
        let d = MeasurementConfidence::from_sample(&dark).uv_std().unwrap();
        assert!(b > 0.0 && d > 0.0);
        assert!(
            d > 2.0 * b,
            "chromaticity floor should climb toward black (bright {b}, dark {d})"
        );
    }

    /// Build a raw [`Sample`] the way a device would: corrected counts run
    /// through the per-channel gradient to absolute XYZ (zero offset), with the
    /// counts/floor/grad retained for the floor + quantization analysis.
    fn sample_from(counts: &[[u32; 6]], floor: [f64; 6], grad: [[f64; 3]; 6]) -> Sample {
        let xyz = counts
            .iter()
            .map(|c| {
                let mut acc = [0.0; 3];
                for ch in 0..6 {
                    let corrected = (c[ch] as f64 - floor[ch]).max(0.0);
                    for i in 0..3 {
                        acc[i] += grad[ch][i] * corrected;
                    }
                }
                Xyz {
                    x: acc[0],
                    y: acc[1],
                    z: acc[2],
                }
            })
            .collect();
        Sample {
            xyz,
            raw: Some(RawRepeats {
                counts: counts.iter().map(|c| c.to_vec()).collect(),
                floor: floor.to_vec(),
                grad: grad.to_vec(),
            }),
        }
    }
}
