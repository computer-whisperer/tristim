//! Interpret a [`tristim_capture::Capture`]: for each measured sample derive
//! the **expected** output from the trial's color description and score how far
//! the measurement landed from it.
//!
//! This is the bridge between the gatherer's facts and a verdict. It applies
//! the agreed ground-truth rules:
//!
//! - **Accepted** trial → expected is computed from the negotiated description.
//! - **Unmanaged** trial → assume sRGB (a color-unmanaged compositor's default).
//! - **Rejected** trial → no basis to score; samples are passed through raw.
//!
//! And the luminance-anchoring rule:
//!
//! - **PQ (absolute)** → the code value decodes to an absolute cd/m² target;
//!   measured-vs-expected is compared in absolute cd/m².
//! - **Relative** encodings (sRGB, gamma, BT.1886) → expected is normalized so
//!   white is 1.0 and compared against the measurement normalized to the
//!   trial's brightest measured patch (its measured white).
//!
//! Chromaticity error (Δu'v') is scale-independent and reported in both cases.
//! ΔE\*ab is computed against the trial's measured white as the reference.

use tristim_capture::{Capture, FormatTrial, Negotiation};
use tristim_color::{ColorSpace, mat3_mul_vec, metrics, transfer, xyz_to_chromaticity};

/// A capture with each trial interpreted.
#[derive(Debug, Clone)]
pub struct AnalyzedCapture {
    pub trials: Vec<AnalyzedTrial>,
}

/// One trial's interpretation.
#[derive(Debug, Clone)]
pub struct AnalyzedTrial {
    /// `wl_shm` pixel format the trial used (copied from the capture).
    pub pixel_format: String,
    /// The basis used to compute "expected", or why none was available.
    pub ground_truth: GroundTruth,
    pub samples: Vec<AnalyzedSample>,
    /// Aggregate error over the scored samples (`None` if nothing was scored).
    pub summary: Option<TrialSummary>,
    /// XYZ of the reference white used to place samples in L\*a\*b\* (the
    /// brightest measured patch). `None` for an unscored trial. Lets a
    /// presenter embed both the samples and an ideal-gamut reference in the
    /// *same* Lab frame the ΔE\*ab scores live in.
    pub reference_white_xyz: Option<[f64; 3]>,
}

/// What "expected" was derived from for a trial.
#[derive(Debug, Clone)]
pub enum GroundTruth {
    /// Expected derived from a known color space + transfer function.
    Known {
        space: ColorSpace,
        /// Transfer-function name (e.g. `"srgb"`, `"st2084_pq"`).
        transfer: String,
        /// True when the transfer function decodes to absolute cd/m² (PQ).
        absolute: bool,
        source: GroundTruthSource,
    },
    /// No basis to score (rejected, or a description this build can't map).
    Unscored { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroundTruthSource {
    /// Taken from the compositor-accepted description.
    Negotiated,
    /// Assumed because the trial was unmanaged.
    AssumedSrgb,
}

/// One sample with its expected value + error (when scorable).
#[derive(Debug, Clone)]
pub struct AnalyzedSample {
    pub requested: [f64; 3],
    pub measured_xyz: [f64; 3],
    pub measured_xy: Option<[f64; 2]>,
    /// Expected chromaticity (`None` for an unscored trial or pure black).
    pub expected_xy: Option<[f64; 2]>,
    /// Expected colour in absolute CIE XYZ — the same value that feeds the ΔE
    /// Lab conversion (PQ: cd/m²; relative encodings: scaled to the measured
    /// white). `None` for an unscored trial. Pairs with [`Self::measured_xyz`]
    /// for rendering asked-vs-got colour swatches.
    pub expected_xyz: Option<[f64; 3]>,
    /// Measured colour in CIE L\*a\*b\* (against the trial's reference white).
    /// `None` when the trial is unscored. The same value that feeds `delta_e`.
    pub measured_lab: Option<[f64; 3]>,
    /// Expected colour in CIE L\*a\*b\* (same reference white as `measured_lab`),
    /// so the displacement between them *is* the reported ΔE\*ab.
    pub expected_lab: Option<[f64; 3]>,
    /// Δu'v' between measured and expected chromaticity.
    pub delta_uv: Option<f64>,
    /// ΔE\*ab (CIE76) against the trial's measured white.
    pub delta_e: Option<f64>,
    /// Luminance comparison (units depend on the encoding).
    pub luminance: Option<LuminanceComparison>,
}

/// Measured-vs-expected luminance for one sample.
#[derive(Debug, Clone, Copy)]
pub struct LuminanceComparison {
    /// Expected: absolute cd/m² when `absolute`, else fraction of white (`0..=1`).
    pub expected: f64,
    /// Measured, in the same units/space as `expected`.
    pub measured: f64,
    /// True for PQ (absolute cd/m²); false for relative-to-white.
    pub absolute: bool,
}

/// Aggregate error across a trial's scored samples.
#[derive(Debug, Clone, Copy)]
pub struct TrialSummary {
    pub scored_samples: usize,
    pub mean_delta_uv: f64,
    pub max_delta_uv: f64,
    pub mean_delta_e: f64,
    pub max_delta_e: f64,
    /// The luminance anchor used: brightest measured `Y` in the trial (cd/m²).
    pub measured_white_y: f64,
}

/// Interpret every trial in `capture`.
pub fn analyze(capture: &Capture) -> AnalyzedCapture {
    AnalyzedCapture {
        trials: capture.trials.iter().map(analyze_trial).collect(),
    }
}

/// Decide the ground truth for a trial from its outcome + requested description.
fn ground_truth_for(trial: &FormatTrial) -> GroundTruth {
    let (primaries, transfer, source) = match &trial.outcome {
        Negotiation::Rejected { cause, message } => {
            return GroundTruth::Unscored {
                reason: format!("rejected ({cause}: {message})"),
            };
        }
        // Unmanaged: the compositor is not applying our description (if any),
        // so the buffer is treated as its default — assume sRGB.
        Negotiation::Unmanaged => ("srgb", "srgb", GroundTruthSource::AssumedSrgb),
        Negotiation::Accepted { .. } => match &trial.requested {
            Some(d) => (
                d.primaries.as_str(),
                d.transfer_function.as_str(),
                GroundTruthSource::Negotiated,
            ),
            None => {
                return GroundTruth::Unscored {
                    reason: "accepted but no description recorded".into(),
                };
            }
        },
    };

    let Some(space) = ColorSpace::from_name(primaries) else {
        return GroundTruth::Unscored {
            reason: format!("unmapped primaries {primaries:?}"),
        };
    };
    // Validate the transfer function by probing the decoder.
    if transfer::decode_named(transfer, 0.5).is_none() {
        return GroundTruth::Unscored {
            reason: format!("unmapped transfer function {transfer:?}"),
        };
    }
    GroundTruth::Known {
        space,
        transfer: transfer.to_string(),
        absolute: transfer == "st2084_pq",
        source,
    }
}

fn analyze_trial(trial: &FormatTrial) -> AnalyzedTrial {
    let ground_truth = ground_truth_for(trial);

    let (space, transfer, absolute) = match &ground_truth {
        GroundTruth::Known {
            space,
            transfer,
            absolute,
            ..
        } => (*space, transfer.clone(), *absolute),
        GroundTruth::Unscored { .. } => {
            // Pass samples through with no expected/error.
            let samples = trial
                .samples
                .iter()
                .map(|s| AnalyzedSample {
                    requested: s.requested,
                    measured_xyz: s.measured.xyz,
                    measured_xy: s.measured.xy,
                    expected_xy: None,
                    expected_xyz: None,
                    measured_lab: None,
                    expected_lab: None,
                    delta_uv: None,
                    delta_e: None,
                    luminance: None,
                })
                .collect();
            return AnalyzedTrial {
                pixel_format: trial.pixel_format.clone(),
                ground_truth,
                samples,
                summary: None,
                reference_white_xyz: None,
            };
        }
    };

    let matrix = space.rgb_to_xyz();

    // Luminance anchor: the brightest measured Y in the trial = measured white.
    let anchor_y = trial
        .samples
        .iter()
        .map(|s| s.measured.xyz[1])
        .fold(0.0_f64, f64::max);
    // Reference white XYZ for ΔE: the measured XYZ of the brightest patch
    // (falls back to the space's white scaled to the anchor if degenerate).
    let white_xyz = trial
        .samples
        .iter()
        .max_by(|a, b| a.measured.xyz[1].total_cmp(&b.measured.xyz[1]))
        .map(|s| s.measured.xyz)
        .filter(|xyz| xyz[1] > 0.0)
        .unwrap_or_else(|| {
            let w = space.white_xyz();
            [
                w[0] * anchor_y.max(1.0),
                anchor_y.max(1.0),
                w[2] * anchor_y.max(1.0),
            ]
        });

    let samples: Vec<AnalyzedSample> = trial
        .samples
        .iter()
        .map(|s| {
            // Decode the requested code values, then matrix to XYZ. For PQ each
            // channel decodes to cd/m² so Y is absolute; otherwise white→1.
            let lin = [
                transfer::decode_named(&transfer, s.requested[0]).unwrap_or(0.0),
                transfer::decode_named(&transfer, s.requested[1]).unwrap_or(0.0),
                transfer::decode_named(&transfer, s.requested[2]).unwrap_or(0.0),
            ];
            let expected_xyz = mat3_mul_vec(&matrix, &lin);
            let expected_xy = xyz_to_chromaticity(expected_xyz);

            let delta_uv = match (s.measured.xy, expected_xy) {
                (Some(m), Some(e)) => Some(metrics::delta_uv(m, e)),
                _ => None,
            };

            // Expected absolute XYZ for ΔE: PQ is already absolute; a relative
            // encoding is scaled so white matches the measured anchor.
            let expected_abs = if absolute {
                expected_xyz
            } else {
                [
                    expected_xyz[0] * anchor_y,
                    expected_xyz[1] * anchor_y,
                    expected_xyz[2] * anchor_y,
                ]
            };
            // Both Labs share the trial's reference white, so their Euclidean
            // separation *is* the ΔE*ab below — and a presenter can plot the
            // pair directly. Computed once here, then reused for the score.
            let (measured_lab, expected_lab, delta_e) = if white_xyz[1] > 0.0 {
                let m = metrics::xyz_to_lab(s.measured.xyz, white_xyz);
                let e = metrics::xyz_to_lab(expected_abs, white_xyz);
                (Some(m), Some(e), Some(metrics::delta_e76(m, e)))
            } else {
                (None, None, None)
            };

            // expected_xyz[1] is absolute cd/m² for PQ, else already a
            // fraction of white (white→1). Match the measured side: absolute
            // cd/m² for PQ, normalized to the measured white otherwise.
            let measured_lum = if absolute {
                s.measured.xyz[1]
            } else if anchor_y > 0.0 {
                s.measured.xyz[1] / anchor_y
            } else {
                0.0
            };
            let luminance = Some(LuminanceComparison {
                expected: expected_xyz[1],
                measured: measured_lum,
                absolute,
            });

            AnalyzedSample {
                requested: s.requested,
                measured_xyz: s.measured.xyz,
                measured_xy: s.measured.xy,
                expected_xy,
                expected_xyz: Some(expected_abs),
                measured_lab,
                expected_lab,
                delta_uv,
                delta_e,
                luminance,
            }
        })
        .collect();

    let summary = summarize(&samples, anchor_y);

    AnalyzedTrial {
        pixel_format: trial.pixel_format.clone(),
        ground_truth,
        samples,
        summary,
        reference_white_xyz: Some(white_xyz),
    }
}

fn summarize(samples: &[AnalyzedSample], anchor_y: f64) -> Option<TrialSummary> {
    let duv: Vec<f64> = samples.iter().filter_map(|s| s.delta_uv).collect();
    let de: Vec<f64> = samples.iter().filter_map(|s| s.delta_e).collect();
    if duv.is_empty() && de.is_empty() {
        return None;
    }
    let mean = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    let max = |v: &[f64]| v.iter().copied().fold(0.0_f64, f64::max);
    Some(TrialSummary {
        scored_samples: duv.len().max(de.len()),
        mean_delta_uv: mean(&duv),
        max_delta_uv: max(&duv),
        mean_delta_e: mean(&de),
        max_delta_e: max(&de),
        measured_white_y: anchor_y,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tristim_capture::{ColorDescription, Measured, Sample, SampleContext};
    use tristim_color::{ColorSpace, mat3_mul_vec, transfer};

    /// Build a single-trial capture from samples + outcome.
    fn capture_with(trial: FormatTrial) -> Capture {
        Capture {
            schema_version: tristim_capture::SCHEMA_VERSION,
            timestamp: "2026-05-24T00:00:00Z".into(),
            tool: tristim_capture::ToolInfo {
                name: "test".into(),
                version: "0".into(),
                git_revision: None,
            },
            device: tristim_capture::DeviceInfo {
                product: "x".into(),
                usb_pid: 0,
                serial: "x".into(),
                hw_version: (0, 0),
                cal_index: 0,
            },
            output: tristim_capture::OutputInfo {
                name: "x".into(),
                make: String::new(),
                model: String::new(),
                description: String::new(),
                mode: None,
            },
            capabilities: Default::default(),
            compositor: Default::default(),
            trials: vec![trial],
        }
    }

    fn sample(requested: [f64; 3], xyz: [f64; 3]) -> Sample {
        let xy = tristim_color::xyz_to_chromaticity(xyz);
        Sample {
            requested,
            measured: Measured {
                raw: [0; 6],
                xyz,
                xy,
            },
            context: SampleContext {
                window_fraction: 1.0,
                border: None,
                settle_ms: 0,
            },
        }
    }

    /// Produce the *ideal* sRGB measurement for a code-value triple at a given
    /// white luminance (a perfect display).
    fn ideal_srgb(cv: [f64; 3], white_y: f64) -> [f64; 3] {
        let lin = [
            transfer::srgb_eotf(cv[0]),
            transfer::srgb_eotf(cv[1]),
            transfer::srgb_eotf(cv[2]),
        ];
        let xyz = mat3_mul_vec(&ColorSpace::SRGB.rgb_to_xyz(), &lin);
        [xyz[0] * white_y, xyz[1] * white_y, xyz[2] * white_y]
    }

    #[test]
    fn perfect_srgb_scores_near_zero() {
        let white_y = 200.0;
        let cvs = [
            [1.0, 1.0, 1.0],
            [0.5, 0.5, 0.5],
            [1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
        ];
        let samples = cvs
            .iter()
            .map(|&cv| sample(cv, ideal_srgb(cv, white_y)))
            .collect();
        let trial = FormatTrial {
            requested: None,
            pixel_format: "xrgb8888".into(),
            outcome: Negotiation::Unmanaged, // assumes sRGB
            gamut: None,
            samples,
        };
        let analyzed = analyze(&capture_with(trial));
        let t = &analyzed.trials[0];
        let s = t.summary.expect("scored");
        assert!(s.max_delta_uv < 1e-6, "Δu'v' = {}", s.max_delta_uv);
        assert!(s.max_delta_e < 1e-3, "ΔE = {}", s.max_delta_e);
        assert!(matches!(
            t.ground_truth,
            GroundTruth::Known {
                source: GroundTruthSource::AssumedSrgb,
                ..
            }
        ));
    }

    #[test]
    fn chromaticity_shift_is_detected() {
        // Measure red as if it were shifted toward orange.
        let white_y = 200.0;
        let mut red = ideal_srgb([1.0, 0.0, 0.0], white_y);
        // nudge it: add some green luminance → pulls xy toward yellow
        red[1] += 5.0;
        let samples = vec![
            sample([1.0, 1.0, 1.0], ideal_srgb([1.0, 1.0, 1.0], white_y)),
            sample([1.0, 0.0, 0.0], red),
        ];
        let trial = FormatTrial {
            requested: Some(ColorDescription {
                transfer_function: "srgb".into(),
                primaries: "srgb".into(),
                reference_white_nits: None,
                mastering: None,
            }),
            pixel_format: "xrgb8888".into(),
            outcome: Negotiation::Accepted { identity: 1 },
            gamut: None,
            samples,
        };
        let analyzed = analyze(&capture_with(trial));
        let red_sample = &analyzed.trials[0].samples[1];
        assert!(
            red_sample.delta_uv.unwrap() > 0.005,
            "should detect the shift"
        );
    }

    #[test]
    fn rejected_trial_is_unscored() {
        let trial = FormatTrial {
            requested: Some(ColorDescription {
                transfer_function: "st2084_pq".into(),
                primaries: "bt2020".into(),
                reference_white_nits: None,
                mastering: None,
            }),
            pixel_format: "xbgr16161616f".into(),
            outcome: Negotiation::Rejected {
                cause: "unsupported".into(),
                message: "no PQ".into(),
            },
            gamut: None,
            samples: vec![sample([1.0, 1.0, 1.0], [0.0, 0.0, 0.0])],
        };
        let analyzed = analyze(&capture_with(trial));
        let t = &analyzed.trials[0];
        assert!(matches!(t.ground_truth, GroundTruth::Unscored { .. }));
        assert!(t.summary.is_none());
        assert!(t.samples[0].delta_uv.is_none());
    }

    #[test]
    fn pq_luminance_is_absolute() {
        // PQ code 0.5081 ≈ 100 cd/m². A perfect grey at that level: measured Y
        // should match expected absolute nits.
        let cv = 0.5081;
        let nits = transfer::pq_eotf(cv);
        let xyz = mat3_mul_vec(&ColorSpace::BT2020.rgb_to_xyz(), &[nits, nits, nits]);
        let samples = vec![sample([cv, cv, cv], xyz)];
        let trial = FormatTrial {
            requested: Some(ColorDescription {
                transfer_function: "st2084_pq".into(),
                primaries: "bt2020".into(),
                reference_white_nits: None,
                mastering: None,
            }),
            pixel_format: "xbgr16161616f".into(),
            outcome: Negotiation::Accepted { identity: 1 },
            gamut: None,
            samples,
        };
        let analyzed = analyze(&capture_with(trial));
        let lum = analyzed.trials[0].samples[0].luminance.unwrap();
        assert!(lum.absolute);
        assert!(
            (lum.expected - 100.0).abs() < 0.5,
            "expected ≈100, got {}",
            lum.expected
        );
        assert!((lum.measured - lum.expected).abs() < 1e-6);
    }
}
