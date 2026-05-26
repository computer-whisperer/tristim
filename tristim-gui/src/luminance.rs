//! The luminance view: measured vs. expected luminance for each scored sample.
//!
//! Each sample is a point at (expected, measured) luminance; the diagonal is
//! the ideal (measured == expected). Points above the line read too bright,
//! below too dim, and the ΔE\*ab coloring ties back to the chromaticity view.
//! Units are absolute cd/m² for a PQ trial, otherwise a fraction of the trial's
//! measured white. For a grey-ramp capture this is the tone-response curve
//! relinearized onto the expected-luminance axis; for a scatter capture it
//! shows luminance accuracy across the probed colors.

use aetna_core::prelude::*;
use tristim_analyze::{AnalyzedTrial, GroundTruth};

use crate::chart::{circle, heat};

/// Design view_box; the renderer scales it to the responsive `plot_px` square.
const SIZE: f32 = 440.0;
// Plot-area insets inside the view_box (room for the axes).
const L: f32 = 16.0;
const R: f32 = 16.0;
const T: f32 = 16.0;
const B: f32 = 16.0;

const AXIS: Color = Color::srgb_u8(150, 150, 160);
const IDEAL: Color = Color::srgb_u8(150, 190, 255);
const UNSCORED: Color = Color::srgb_u8(140, 140, 150);

/// Build the luminance chart El for `t`, rendered as a `plot_px`-square.
pub fn luminance_chart(t: &AnalyzedTrial, plot_px: f32) -> El {
    // (expected, measured, ΔE) for every sample that was scored for luminance.
    let pts: Vec<(f64, f64, Option<f64>)> = t
        .samples
        .iter()
        .filter_map(|s| s.luminance.map(|l| (l.expected, l.measured, s.delta_e)))
        .collect();

    if pts.is_empty() {
        return vector(VectorAsset::from_paths([0.0, 0.0, SIZE, SIZE], vec![]))
            .width(Size::Fixed(plot_px))
            .height(Size::Fixed(plot_px));
    }

    // Equal axes (ideal is y = x), scaled to the data with a little headroom.
    let max = pts
        .iter()
        .map(|&(e, m, _)| e.max(m))
        .fold(0.0_f64, f64::max)
        .max(1e-6)
        * 1.05;

    let px = |e: f64| L + (e / max) as f32 * (SIZE - L - R);
    let py = |m: f64| (SIZE - B) - (m / max) as f32 * (SIZE - T - B);

    let mut paths: Vec<VectorPath> = Vec::new();

    // Axes (left + bottom) and the ideal diagonal.
    paths.push(line(px(0.0), py(0.0), px(0.0), py(max), AXIS, 1.0));
    paths.push(line(px(0.0), py(0.0), px(max), py(0.0), AXIS, 1.0));
    paths.push(line(px(0.0), py(0.0), px(max), py(max), IDEAL, 1.5));

    // One dot per sample at (expected, measured), colored by ΔE.
    for (e, m, de) in pts {
        let color = de.map_or(UNSCORED, heat);
        paths.push(circle([px(e), py(m)], 4.0).fill_solid(color).build());
    }

    vector(VectorAsset::from_paths([0.0, 0.0, SIZE, SIZE], paths))
        .width(Size::Fixed(plot_px))
        .height(Size::Fixed(plot_px))
}

/// Units label for the axes, given the trial's ground truth.
pub fn luminance_units(t: &AnalyzedTrial) -> &'static str {
    match &t.ground_truth {
        GroundTruth::Known { absolute: true, .. } => "cd/m²",
        _ => "fraction of white",
    }
}

fn line(x0: f32, y0: f32, x1: f32, y1: f32, color: Color, width: f32) -> VectorPath {
    PathBuilder::new()
        .move_to(x0, y0)
        .line_to(x1, y1)
        .stroke_solid(color, width)
        .build()
}
