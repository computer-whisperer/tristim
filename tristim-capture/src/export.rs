//! Export a capture's facts in external interchange formats.
//!
//! Two renderings of the same recorded facts — no interpretation (no
//! expected values, no scoring; that stays in the analysis layer):
//!
//! - [`trial_to_ti3`] — one trial's samples as ArgyllCMS/CGATS `.ti3`
//!   display measurement data, the interchange format of the open-source
//!   calibration ecosystem. `colprof` builds an ICC profile from it,
//!   `profcheck` verifies against it, DisplayCAL imports it.
//! - [`to_csv`] — every sample of every trial as one flat CSV table, for
//!   spreadsheets / dataframes.
//!
//! ## What a `.ti3` from tristim means
//!
//! tristim measures through the compositor's normal client path, so the
//! exported data characterizes *the display as driven by this compositor
//! for this trial's encoding* — for an unmanaged trial that is the classic
//! display characterization; for a color-managed trial it characterizes the
//! composited pipeline. An ICC profile built from it describes exactly
//! that.

use crate::{Capture, Negotiation, SampleSource};
use std::fmt::Write as _;

/// Code-value epsilon for recognizing the white patch (`requested ≈ 1,1,1`).
const WHITE_EPS: f64 = 1e-9;

/// Render one trial's samples as a CGATS `.ti3` file (Argyll `dispread`'s
/// output format): device RGB scaled to `0..=100`, XYZ normalized so the
/// white patch's Y is 100, with the absolute white XYZ recorded under
/// `LUMINANCE_XYZ_CDM2`.
///
/// The white reference is the measured `requested = (1,1,1)` patch, falling
/// back to the brightest-Y sample when the sweep didn't include full white.
/// Returns `None` when the trial has no samples or no usable white
/// (everything measured black) — there is nothing to normalize against.
pub fn trial_to_ti3(capture: &Capture, trial_index: usize) -> Option<String> {
    let trial = capture.trials.get(trial_index)?;
    if trial.samples.is_empty() {
        return None;
    }
    let white = trial
        .samples
        .iter()
        .find(|s| s.requested.iter().all(|&c| (c - 1.0).abs() < WHITE_EPS))
        .or_else(|| {
            trial
                .samples
                .iter()
                .max_by(|a, b| a.measured.xyz[1].total_cmp(&b.measured.xyz[1]))
        })?;
    let wxyz = white.measured.xyz;
    if wxyz[1] <= 0.0 {
        return None;
    }
    let nn = 100.0 / wxyz[1];

    let encoding = match &trial.requested {
        Some(d) => format!("{} / {}", d.transfer_function, d.primaries),
        None => "unmanaged".to_string(),
    };
    let descriptor = format!(
        "tristim measurement of {} ({}, {})",
        capture.output.name, encoding, trial.pixel_format
    );

    let mut o = String::new();
    // File identifier, %-7s + blank line, as Argyll's cgats writer emits it.
    o.push_str("CTI3   \n\n");
    kword(&mut o, "DESCRIPTOR", &descriptor);
    kword(
        &mut o,
        "ORIGINATOR",
        &format!("{} {}", capture.tool.name, capture.tool.version),
    );
    kword(&mut o, "CREATED", &capture.timestamp);
    kword(&mut o, "DEVICE_CLASS", "DISPLAY");
    kword(&mut o, "TARGET_INSTRUMENT", &capture.device.product);
    kword(&mut o, "INSTRUMENT_TYPE_SPECTRAL", "NO");
    kword(&mut o, "COLOR_REP", "RGB_XYZ");
    kword(
        &mut o,
        "LUMINANCE_XYZ_CDM2",
        &format!("{:.6} {:.6} {:.6}", wxyz[0], wxyz[1], wxyz[2]),
    );
    kword(&mut o, "NORMALIZED_TO_Y_100", "YES");

    o.push_str("\nNUMBER_OF_FIELDS 7\nBEGIN_DATA_FORMAT\n");
    o.push_str("SAMPLE_ID RGB_R RGB_G RGB_B XYZ_X XYZ_Y XYZ_Z \nEND_DATA_FORMAT\n");
    let _ = write!(o, "\nNUMBER_OF_SETS {}\nBEGIN_DATA\n", trial.samples.len());
    for (i, s) in trial.samples.iter().enumerate() {
        let r = s.requested;
        let m = s.measured.xyz;
        let _ = writeln!(
            o,
            "{} {:.5} {:.5} {:.5} {:.6} {:.6} {:.6}",
            i + 1,
            100.0 * r[0],
            100.0 * r[1],
            100.0 * r[2],
            nn * m[0],
            nn * m[1],
            nn * m[2],
        );
    }
    o.push_str("END_DATA\n");
    Some(o)
}

/// One CGATS keyword line: `SYMBOL "value"`.
fn kword(o: &mut String, sym: &str, value: &str) {
    let _ = writeln!(o, "{sym} \"{value}\"");
}

/// Render every sample of every trial as one flat CSV table. Optional fields
/// are empty cells; variable-length raw counts are `;`-joined inside one
/// cell. Always returns at least the header row.
pub fn to_csv(capture: &Capture) -> String {
    let mut o = String::from(
        "trial,pixel_format,transfer_function,primaries,outcome,source,repeats,\
         adaptive_tier,elapsed_ms,window_fraction,settle_ms,\
         req_r,req_g,req_b,xyz_x,xyz_y,xyz_z,xy_x,xy_y,raw\n",
    );
    for (ti, trial) in capture.trials.iter().enumerate() {
        let (tf, prim) = match &trial.requested {
            Some(d) => (d.transfer_function.as_str(), d.primaries.as_str()),
            None => ("", ""),
        };
        let outcome = match &trial.outcome {
            Negotiation::Accepted { .. } => "accepted",
            Negotiation::Rejected { .. } => "rejected",
            Negotiation::Unmanaged => "unmanaged",
        };
        for s in &trial.samples {
            let source = match s.source {
                SampleSource::Sweep => "sweep",
                SampleSource::GamutProbe => "gamut_probe",
            };
            let tier = match s.adaptive_tier {
                Some(crate::AdaptiveTier::Fast) => "fast",
                Some(crate::AdaptiveTier::EscalatedFull) => "escalated_full",
                Some(crate::AdaptiveTier::SingleFull) => "single_full",
                None => "",
            };
            let elapsed = s.elapsed_ms.map(|v| v.to_string()).unwrap_or_default();
            let (xy_x, xy_y) = match s.measured.xy {
                Some([x, y]) => (format!("{x:.6}"), format!("{y:.6}")),
                None => (String::new(), String::new()),
            };
            let raw = s
                .measured
                .raw
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(";");
            let _ = writeln!(
                o,
                "{ti},{pf},{tf},{prim},{outcome},{source},{repeats},{tier},{elapsed},\
                 {wf},{settle},{rr:.6},{rg:.6},{rb:.6},{x:.6},{y:.6},{z:.6},{xy_x},{xy_y},{raw}",
                pf = trial.pixel_format,
                repeats = s.repeats,
                wf = s.context.window_fraction,
                settle = s.context.settle_ms,
                rr = s.requested[0],
                rg = s.requested[1],
                rb = s.requested[2],
                x = s.measured.xyz[0],
                y = s.measured.xyz[1],
                z = s.measured.xyz[2],
            );
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Measured, Sample, SampleContext};

    fn sample(requested: [f64; 3], xyz: [f64; 3]) -> Sample {
        Sample {
            requested,
            measured: Measured {
                raw: vec![1, 2, 3],
                xyz,
                xy: Some([0.31, 0.33]),
            },
            context: SampleContext {
                window_fraction: 1.0,
                border: None,
                settle_ms: 250,
            },
            source: SampleSource::Sweep,
            repeats: 1,
            adaptive_tier: None,
            elapsed_ms: Some(1000),
        }
    }

    fn capture_with_samples(samples: Vec<Sample>) -> Capture {
        let mut c = crate::tests::sample_capture();
        c.trials[0].samples = samples;
        c
    }

    #[test]
    fn ti3_normalizes_to_white_and_records_absolute_luminance() {
        let c = capture_with_samples(vec![
            sample([1.0, 1.0, 1.0], [190.0, 200.0, 210.0]),
            sample([0.5, 0.5, 0.5], [38.0, 40.0, 42.0]),
        ]);
        let ti3 = trial_to_ti3(&c, 0).expect("exportable");
        assert!(ti3.starts_with("CTI3   \n\n"));
        assert!(ti3.contains("LUMINANCE_XYZ_CDM2 \"190.000000 200.000000 210.000000\""));
        assert!(ti3.contains("NORMALIZED_TO_Y_100 \"YES\""));
        assert!(ti3.contains("NUMBER_OF_SETS 2"));
        // White row normalizes to Y = 100; the half patch to Y = 20.
        assert!(ti3.contains("1 100.00000 100.00000 100.00000 95.000000 100.000000 105.000000"));
        assert!(ti3.contains("2 50.00000 50.00000 50.00000 19.000000 20.000000 21.000000"));
    }

    #[test]
    fn ti3_falls_back_to_brightest_sample_without_a_white_patch() {
        let c = capture_with_samples(vec![
            sample([0.25, 0.25, 0.25], [9.5, 10.0, 10.5]),
            sample([0.75, 0.75, 0.75], [95.0, 100.0, 105.0]),
        ]);
        let ti3 = trial_to_ti3(&c, 0).expect("exportable");
        assert!(ti3.contains("LUMINANCE_XYZ_CDM2 \"95.000000 100.000000 105.000000\""));
    }

    #[test]
    fn ti3_refuses_empty_or_black_trials() {
        let c = capture_with_samples(vec![]);
        assert!(trial_to_ti3(&c, 0).is_none());
        assert!(trial_to_ti3(&c, 99).is_none());
        let c = capture_with_samples(vec![sample([1.0, 1.0, 1.0], [0.0, 0.0, 0.0])]);
        assert!(trial_to_ti3(&c, 0).is_none());
    }

    #[test]
    fn csv_emits_one_row_per_sample_with_stable_columns() {
        let c = capture_with_samples(vec![
            sample([1.0, 1.0, 1.0], [190.0, 200.0, 210.0]),
            sample([0.0, 0.0, 0.0], [0.1, 0.1, 0.1]),
        ]);
        let csv = to_csv(&c);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 samples
        let cols = lines[0].split(',').count();
        for l in &lines[1..] {
            assert_eq!(l.split(',').count(), cols, "ragged row: {l}");
        }
        assert!(lines[1].contains("xbgr16161616f"));
        assert!(lines[1].contains("1;2;3"));
        assert!(lines[1].ends_with("1;2;3"));
    }
}
