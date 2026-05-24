//! Color science for interpreting tristim captures.
//!
//! Pure, dependency-free building blocks the analysis/presentation tools use
//! to turn a capture into a verdict:
//!
//! - [`ColorSpace`] — standard RGB primaries + white point, and the derived
//!   linear-RGB ↔ CIE XYZ matrices.
//! - [`transfer`] — encode/decode transfer functions (sRGB, pure gamma, BT.1886,
//!   PQ). PQ decodes to absolute cd/m²; the others to relative `0..=1` linear.
//! - [`metrics`] — CIE 1976 u'v', ΔE\*ab, McCamy CCT, xy-plane gamut area.
//!
//! Names match the strings recorded in `tristim-capture` color descriptions
//! (e.g. `"srgb"`, `"st2084_pq"`, `"bt2020"`) so the presenter can look a
//! space / transfer function up directly from a capture.

pub mod metrics;
pub mod transfer;

/// CIE 1931 xy chromaticity coordinate.
pub type Chromaticity = [f64; 2];

/// Standard white points (CIE 1931 xy).
pub mod white {
    use super::Chromaticity;
    /// CIE D65 — sRGB / BT.709 / BT.2020 / Display-P3 reference white.
    pub const D65: Chromaticity = [0.3127, 0.3290];
    /// DCI white (theatrical P3).
    pub const DCI: Chromaticity = [0.314, 0.351];
}

/// An RGB color space: primaries + white point. Enough to derive the
/// linear-RGB ↔ XYZ matrices; the transfer function is tracked separately
/// (see [`transfer`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorSpace {
    pub red: Chromaticity,
    pub green: Chromaticity,
    pub blue: Chromaticity,
    pub white: Chromaticity,
}

impl ColorSpace {
    /// sRGB / BT.709 primaries, D65 white.
    pub const SRGB: ColorSpace = ColorSpace {
        red: [0.640, 0.330],
        green: [0.300, 0.600],
        blue: [0.150, 0.060],
        white: white::D65,
    };
    /// BT.2020 primaries, D65 white.
    pub const BT2020: ColorSpace = ColorSpace {
        red: [0.708, 0.292],
        green: [0.170, 0.797],
        blue: [0.131, 0.046],
        white: white::D65,
    };
    /// Display-P3 (P3 primaries, D65 white).
    pub const DISPLAY_P3: ColorSpace = ColorSpace {
        red: [0.680, 0.320],
        green: [0.265, 0.690],
        blue: [0.150, 0.060],
        white: white::D65,
    };
    /// DCI-P3 (P3 primaries, DCI white).
    pub const DCI_P3: ColorSpace = ColorSpace {
        red: [0.680, 0.320],
        green: [0.265, 0.690],
        blue: [0.150, 0.060],
        white: white::DCI,
    };
    /// Adobe RGB (1998), D65 white.
    pub const ADOBE_RGB: ColorSpace = ColorSpace {
        red: [0.640, 0.330],
        green: [0.210, 0.710],
        blue: [0.150, 0.060],
        white: white::D65,
    };

    /// Look up a color space by its capture/protocol primaries name.
    pub fn from_name(name: &str) -> Option<ColorSpace> {
        Some(match name {
            "srgb" => Self::SRGB,
            "bt2020" => Self::BT2020,
            "display_p3" => Self::DISPLAY_P3,
            "dci_p3" => Self::DCI_P3,
            "adobe_rgb" => Self::ADOBE_RGB,
            _ => return None,
        })
    }

    /// XYZ of the white point, normalized to `Y = 1`.
    pub fn white_xyz(&self) -> [f64; 3] {
        chromaticity_to_xyz(self.white)
    }

    /// 3×3 matrix mapping linear RGB (`0..=1`, white → `[1,1,1]`) to CIE XYZ,
    /// normalized so the white point has `Y = 1`.
    pub fn rgb_to_xyz(&self) -> [[f64; 3]; 3] {
        let r = chromaticity_to_xyz(self.red);
        let g = chromaticity_to_xyz(self.green);
        let b = chromaticity_to_xyz(self.blue);
        // Columns = primary XYZ (each at Y = 1).
        let m = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
        let s = mat3_mul_vec(&mat3_inverse(&m), &self.white_xyz());
        [
            [m[0][0] * s[0], m[0][1] * s[1], m[0][2] * s[2]],
            [m[1][0] * s[0], m[1][1] * s[1], m[1][2] * s[2]],
            [m[2][0] * s[0], m[2][1] * s[1], m[2][2] * s[2]],
        ]
    }

    /// Inverse of [`rgb_to_xyz`](Self::rgb_to_xyz): CIE XYZ → linear RGB.
    pub fn xyz_to_rgb(&self) -> [[f64; 3]; 3] {
        mat3_inverse(&self.rgb_to_xyz())
    }
}

/// xy (with implicit `Y = 1`) → CIE XYZ.
pub fn chromaticity_to_xyz(xy: Chromaticity) -> [f64; 3] {
    let [x, y] = xy;
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// CIE XYZ → xy chromaticity. `None` for `X + Y + Z == 0` (black).
pub fn xyz_to_chromaticity(xyz: [f64; 3]) -> Option<Chromaticity> {
    let sum = xyz[0] + xyz[1] + xyz[2];
    if sum <= 0.0 {
        return None;
    }
    Some([xyz[0] / sum, xyz[1] / sum])
}

/// `M · v` for a 3×3 matrix and 3-vector.
pub fn mat3_mul_vec(m: &[[f64; 3]; 3], v: &[f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// Inverse of a 3×3 matrix via cofactors. Returns the identity-scaled-by-NaN
/// path only if the matrix is singular (determinant 0) — callers pass
/// well-conditioned color matrices, so this is not defended further.
pub fn mat3_inverse(m: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    let inv_det = 1.0 / det;
    [
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv_det,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv_det,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv_det,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv_det,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv_det,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv_det,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv_det,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv_det,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv_det,
        ],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "expected ~{b}, got {a} (tol {tol})");
    }

    #[test]
    fn srgb_matrix_matches_known_values() {
        // Bruce Lindbloom's sRGB D65 matrix (rounded).
        let m = ColorSpace::SRGB.rgb_to_xyz();
        close(m[0][0], 0.4124, 1e-3);
        close(m[1][0], 0.2126, 1e-3);
        close(m[2][0], 0.0193, 1e-3);
        close(m[0][2], 0.1805, 1e-3);
        close(m[2][2], 0.9505, 1e-3);
    }

    #[test]
    fn white_maps_to_d65_xyz() {
        let m = ColorSpace::SRGB.rgb_to_xyz();
        let w = mat3_mul_vec(&m, &[1.0, 1.0, 1.0]);
        close(w[0], 0.9505, 1e-3);
        close(w[1], 1.0, 1e-9);
        close(w[2], 1.0890, 1e-3);
    }

    #[test]
    fn rgb_xyz_round_trips() {
        let fwd = ColorSpace::BT2020.rgb_to_xyz();
        let inv = ColorSpace::BT2020.xyz_to_rgb();
        let rgb = [0.3, 0.6, 0.9];
        let back = mat3_mul_vec(&inv, &mat3_mul_vec(&fwd, &rgb));
        for i in 0..3 {
            close(back[i], rgb[i], 1e-9);
        }
    }

    #[test]
    fn from_name_known_and_unknown() {
        assert_eq!(ColorSpace::from_name("bt2020"), Some(ColorSpace::BT2020));
        assert!(ColorSpace::from_name("rec601").is_none());
    }

    #[test]
    fn bt2020_is_wider_than_srgb() {
        use metrics::triangle_area_xy;
        let srgb = triangle_area_xy(
            ColorSpace::SRGB.red,
            ColorSpace::SRGB.green,
            ColorSpace::SRGB.blue,
        );
        let bt2020 = triangle_area_xy(
            ColorSpace::BT2020.red,
            ColorSpace::BT2020.green,
            ColorSpace::BT2020.blue,
        );
        assert!(bt2020 > srgb);
        close(srgb, 0.1121, 2e-3); // matches the historical sRGB triangle area
    }
}
