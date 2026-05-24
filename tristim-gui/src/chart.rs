//! Render an analyzed trial as an aetna vector chart: the CIE 1976 u'v'
//! chromaticity diagram with the spectral locus, the target gamut triangle and
//! white point, and each measured sample joined by an error vector to where it
//! should have landed — colored by ΔE\*ab.
//!
//! The plot is a fixed-size square (the vector view_box), so it stays
//! aspect-correct regardless of where layout places it.

use aetna_core::prelude::*;
use tristim_analyze::{AnalyzedTrial, GroundTruth};
use tristim_color::metrics::xy_to_uv_prime;

use crate::plot::{Projector, UvView, gamut_uv, locus_uv, white_uv};

/// Side length (px) of the square plot; also the vector view_box extent.
const SIZE: f32 = 440.0;

/// Build the chromaticity chart El for `t`. Always draws the locus and the
/// measured samples; the gamut triangle, white marker, and error vectors
/// appear only when the trial has a known ground-truth color space.
pub fn chromaticity_chart(t: &AnalyzedTrial) -> El {
    let proj = Projector::new([0.0, 0.0, SIZE, SIZE], UvView::DEFAULT);
    let mut paths: Vec<VectorPath> = Vec::new();

    // Plot frame + spectral locus outline.
    paths.push(frame_path());
    paths.push(locus_path(&proj));

    // Target gamut triangle + white point, when we have a basis.
    if let GroundTruth::Known { space, .. } = &t.ground_truth {
        paths.push(triangle_path(&proj, &gamut_uv(space)));
        white_marker(&proj, white_uv(space), &mut paths);
    }

    // Per-sample: error vector (target → measured) + measured dot, by ΔE.
    for s in &t.samples {
        let Some(m_xy) = s.measured_xy else { continue };
        let m = proj.project(xy_to_uv_prime(m_xy));
        let color = s.delta_e.map_or(UNSCORED, heat);
        if let Some(e_xy) = s.expected_xy {
            let e = proj.project(xy_to_uv_prime(e_xy));
            paths.push(line_path(e, m, color, 1.5));
        }
        paths.push(circle(m, 4.0).fill_solid(color).build());
    }

    let asset = VectorAsset::from_paths([0.0, 0.0, SIZE, SIZE], paths);
    vector(asset)
}

const LOCUS: Color = Color::srgb_u8(150, 150, 160);
const TRIANGLE: Color = Color::srgb_u8(150, 190, 255);
const WHITE: Color = Color::srgb_u8(245, 245, 245);
const UNSCORED: Color = Color::srgb_u8(140, 140, 150);

/// Faint border just inside the view_box so the stroke isn't clipped.
fn frame_path() -> VectorPath {
    let m = 0.5;
    PathBuilder::new()
        .move_to(m, m)
        .line_to(SIZE - m, m)
        .line_to(SIZE - m, SIZE - m)
        .line_to(m, SIZE - m)
        .close()
        .stroke_solid(tokens::BORDER, 1.0)
        .build()
}

fn locus_path(proj: &Projector) -> VectorPath {
    let uv = locus_uv();
    let p0 = proj.project(uv[0]);
    let mut pb = PathBuilder::new().move_to(p0[0], p0[1]);
    for &c in &uv[1..] {
        let p = proj.project(c);
        pb = pb.line_to(p[0], p[1]);
    }
    pb.close().stroke_solid(LOCUS, 1.5).build()
}

fn triangle_path(proj: &Projector, g: &[[f64; 2]; 3]) -> VectorPath {
    let a = proj.project(g[0]);
    let b = proj.project(g[1]);
    let c = proj.project(g[2]);
    PathBuilder::new()
        .move_to(a[0], a[1])
        .line_to(b[0], b[1])
        .line_to(c[0], c[1])
        .close()
        .stroke_solid(TRIANGLE, 1.5)
        .build()
}

/// A hollow ring + center dot at the target white point.
fn white_marker(proj: &Projector, uv: [f64; 2], out: &mut Vec<VectorPath>) {
    let c = proj.project(uv);
    out.push(circle(c, 5.0).stroke_solid(WHITE, 1.5).build());
    out.push(circle(c, 1.5).fill_solid(WHITE).build());
}

fn line_path(a: [f32; 2], b: [f32; 2], color: Color, width: f32) -> VectorPath {
    PathBuilder::new()
        .move_to(a[0], a[1])
        .line_to(b[0], b[1])
        .stroke_solid(color, width)
        .build()
}

/// An unbuilt circular path centered at `c` with radius `r`, approximated by
/// four cubic Béziers. The caller adds the fill or stroke.
fn circle(c: [f32; 2], r: f32) -> PathBuilder {
    const K: f32 = 0.552_285; // 4/3 * (sqrt(2) - 1)
    let (x, y) = (c[0], c[1]);
    let k = K * r;
    PathBuilder::new()
        .move_to(x, y - r)
        .cubic_to(x + k, y - r, x + r, y - k, x + r, y)
        .cubic_to(x + r, y + k, x + k, y + r, x, y + r)
        .cubic_to(x - k, y + r, x - r, y + k, x - r, y)
        .cubic_to(x - r, y - k, x - k, y - r, x, y - r)
        .close()
}

/// ΔE\*ab → heat color: green (faithful) through amber to red (severe). The
/// scale saturates near ΔE 15, well above the ~2.3 just-noticeable threshold.
fn heat(de: f64) -> Color {
    let good = Color::srgb_u8(90, 200, 130);
    let amber = Color::srgb_u8(240, 200, 70);
    let bad = Color::srgb_u8(235, 80, 80);
    let t = (de / 15.0).clamp(0.0, 1.0) as f32;
    if t < 0.5 {
        good.mix(amber, t * 2.0)
    } else {
        amber.mix(bad, (t - 0.5) * 2.0)
    }
}
