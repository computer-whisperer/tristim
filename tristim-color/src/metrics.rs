//! Color-difference and summary metrics.
//!
//! These operate on plain XYZ / xy values so they're reusable regardless of
//! how the capture was produced. ΔE\*ab and the u'v' distance are the workhorses
//! for scoring how far a measurement landed from its expected target.

use crate::Chromaticity;

/// xy → CIE 1976 UCS u'v' (roughly perceptually uniform). Returns NaNs for a
/// degenerate denominator (shouldn't occur for real chromaticities).
pub fn xy_to_uv_prime(xy: Chromaticity) -> [f64; 2] {
    let [x, y] = xy;
    let denom = -2.0 * x + 12.0 * y + 3.0;
    if denom.abs() < 1e-12 {
        return [f64::NAN, f64::NAN];
    }
    [4.0 * x / denom, 9.0 * y / denom]
}

/// Δu'v': Euclidean distance between two chromaticities in CIE 1976 UCS.
/// `> 0.005` is perceptible, `> 0.015` obvious, `> 0.030` severe.
pub fn delta_uv(a: Chromaticity, b: Chromaticity) -> f64 {
    let a = xy_to_uv_prime(a);
    let b = xy_to_uv_prime(b);
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// McCamy's correlated-colour-temperature approximation (K). Valid only near
/// the Planckian locus; returns `None` when the result is out of a sane range.
pub fn cct_mccamy(xy: Chromaticity) -> Option<f64> {
    let [x, y] = xy;
    let denom = 0.1858 - y;
    if denom.abs() < 1e-6 {
        return None;
    }
    let n = (x - 0.3320) / denom;
    let cct = 437.0 * n.powi(3) + 3601.0 * n.powi(2) + 6831.0 * n + 5517.0;
    if cct.is_finite() && (1000.0..=50_000.0).contains(&cct) {
        Some(cct)
    } else {
        None
    }
}

/// Area of a triangle in the CIE 1931 xy plane (e.g. a gamut's primaries).
/// The sRGB primary triangle is ≈ 0.1121.
pub fn triangle_area_xy(p1: Chromaticity, p2: Chromaticity, p3: Chromaticity) -> f64 {
    0.5 * (p1[0] * (p2[1] - p3[1]) + p2[0] * (p3[1] - p1[1]) + p3[0] * (p1[1] - p2[1])).abs()
}

/// CIE XYZ → L\*a\*b\* relative to a white reference (also XYZ, `Y` arbitrary
/// but consistent with `xyz`). Used for ΔE\*ab.
pub fn xyz_to_lab(xyz: [f64; 3], white: [f64; 3]) -> [f64; 3] {
    fn f(t: f64) -> f64 {
        const DELTA: f64 = 6.0 / 29.0;
        if t > DELTA * DELTA * DELTA {
            t.cbrt()
        } else {
            t / (3.0 * DELTA * DELTA) + 4.0 / 29.0
        }
    }
    let fx = f(xyz[0] / white[0]);
    let fy = f(xyz[1] / white[1]);
    let fz = f(xyz[2] / white[2]);
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// CIE76 ΔE\*ab between two L\*a\*b\* values.
pub fn delta_e76(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColorSpace, white};

    fn close(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "expected ~{b}, got {a} (tol {tol})");
    }

    #[test]
    fn d65_uv_prime_known() {
        let uv = xy_to_uv_prime(white::D65);
        close(uv[0], 0.1978, 1e-3);
        close(uv[1], 0.4683, 1e-3);
    }

    #[test]
    fn delta_uv_zero_for_same_point() {
        assert_eq!(delta_uv(white::D65, white::D65), 0.0);
    }

    #[test]
    fn d65_cct_near_6500() {
        let cct = cct_mccamy(white::D65).unwrap();
        close(cct, 6500.0, 100.0);
    }

    #[test]
    fn srgb_gamut_area() {
        let a = triangle_area_xy(
            ColorSpace::SRGB.red,
            ColorSpace::SRGB.green,
            ColorSpace::SRGB.blue,
        );
        close(a, 0.1121, 2e-3);
    }

    #[test]
    fn delta_e_zero_and_positive() {
        let w = ColorSpace::SRGB.white_xyz();
        let lab_w = xyz_to_lab(w, w);
        close(delta_e76(lab_w, lab_w), 0.0, 1e-9);
        close(lab_w[0], 100.0, 1e-6); // white is L* = 100
        let darker = xyz_to_lab([w[0] * 0.5, w[1] * 0.5, w[2] * 0.5], w);
        assert!(delta_e76(lab_w, darker) > 1.0);
    }
}
