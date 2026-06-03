//! ICtCp (ITU-R BT.2100): a perceptually-uniform, absolute-luminance space.
//!
//! Unlike L\*a\*b\* — whose lightness is *relative* to a chosen white — ICtCp's
//! intensity axis is anchored to absolute luminance through the PQ curve, so a
//! 100 cd/m² white and a 1000 cd/m² white land at different `I`. That makes it
//! the natural frame for comparing SDR vs HDR / sRGB vs BT.2020 content where
//! the absolute brightness *is* the point.
//!
//! The pipeline is the BT.2100 one: absolute XYZ (cd/m²) → linear Rec.2020 RGB
//! (same absolute scale) → LMS cone signals → PQ-encode each → the `I/Ct/Cp`
//! mix. `I` runs `0..=1` (PQ of the achromatic luminance); `Ct` (tritan,
//! blue–yellow) and `Cp` (protan, red–green) are bipolar, roughly `±0.5` for
//! saturated wide-gamut content.

use crate::{ColorSpace, mat3_mul_vec, transfer};

/// Linear Rec.2020 RGB → LMS cone responses (BT.2100, the `/4096` integer
/// matrix). Rows sum to 4096, so a neutral `R=G=B` maps to `L=M=S` — the
/// achromatic axis stays achromatic.
const RGB_TO_LMS: [[f64; 3]; 3] = [
    [1688.0 / 4096.0, 2146.0 / 4096.0, 262.0 / 4096.0],
    [683.0 / 4096.0, 2951.0 / 4096.0, 462.0 / 4096.0],
    [99.0 / 4096.0, 309.0 / 4096.0, 3688.0 / 4096.0],
];

/// PQ-encoded L'M'S' → ICtCp (BT.2100). The first row is `0.5·(L'+M')` (the
/// achromatic intensity); the other two are the opponent chroma axes.
const LMS_TO_ICTCP: [[f64; 3]; 3] = [
    [2048.0 / 4096.0, 2048.0 / 4096.0, 0.0],
    [6610.0 / 4096.0, -13613.0 / 4096.0, 7003.0 / 4096.0],
    [17933.0 / 4096.0, -17390.0 / 4096.0, -543.0 / 4096.0],
];

/// Absolute CIE XYZ (`Y` in cd/m²) → ICtCp `[I, Ct, Cp]`.
///
/// XYZ is taken to linear Rec.2020 RGB (the matrix is normalized to `Y = 1`
/// white, so feeding absolute XYZ yields RGB in the same cd/m² scale), mixed to
/// LMS, PQ-encoded per cone, then combined. Out-of-Rec.2020 inputs can drive a
/// cone signal negative; PQ clamps those to 0, so extreme out-of-gamut colours
/// distort in hue but stay finite — fine for visualization.
pub fn xyz_to_ictcp(xyz: [f64; 3]) -> [f64; 3] {
    let rgb = mat3_mul_vec(&ColorSpace::BT2020.xyz_to_rgb(), &xyz);
    let lms = mat3_mul_vec(&RGB_TO_LMS, &rgb);
    let lms_pq = [
        transfer::pq_oetf(lms[0]),
        transfer::pq_oetf(lms[1]),
        transfer::pq_oetf(lms[2]),
    ];
    mat3_mul_vec(&LMS_TO_ICTCP, &lms_pq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{chromaticity_to_xyz, white};

    fn close(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "expected ~{b}, got {a} (tol {tol})");
    }

    /// A neutral (D65) patch is achromatic in ICtCp — Ct and Cp vanish — and
    /// its intensity is exactly the PQ encoding of its absolute luminance.
    #[test]
    fn neutral_is_achromatic_with_pq_intensity() {
        for &nits in &[1.0, 100.0, 203.0, 1000.0, 10_000.0] {
            let w = chromaticity_to_xyz(white::D65); // Y = 1
            let xyz = [w[0] * nits, nits, w[2] * nits];
            let [i, ct, cp] = xyz_to_ictcp(xyz);
            close(i, transfer::pq_oetf(nits), 1e-9);
            close(ct, 0.0, 1e-9);
            close(cp, 0.0, 1e-9);
        }
    }

    /// Brighter neutral ⇒ strictly larger intensity (the axis is absolute, not
    /// normalized to any white).
    #[test]
    fn intensity_is_monotonic_in_absolute_luminance() {
        let w = chromaticity_to_xyz(white::D65);
        let at = |nits: f64| xyz_to_ictcp([w[0] * nits, nits, w[2] * nits])[0];
        assert!(at(100.0) < at(203.0));
        assert!(at(203.0) < at(1000.0));
    }

    /// Saturated Rec.2020 primaries land off the achromatic axis, each in the
    /// expected opponent direction: red is +Cp, blue is +Ct.
    #[test]
    fn primaries_have_expected_chroma_sign() {
        let m = ColorSpace::BT2020.rgb_to_xyz();
        let red = mat3_mul_vec(&m, &[203.0, 0.0, 0.0]);
        let blue = mat3_mul_vec(&m, &[0.0, 0.0, 203.0]);
        let [_, _, red_cp] = xyz_to_ictcp(red);
        let [_, blue_ct, _] = xyz_to_ictcp(blue);
        assert!(red_cp > 0.0, "red should be +Cp (protan), got {red_cp}");
        assert!(blue_ct > 0.0, "blue should be +Ct (tritan), got {blue_ct}");
    }
}
