//! Default calibration-matrix computation for the i1d3.
//!
//! The instrument reports per-channel frequencies (Hz); the per-unit factory
//! data is the spectral sensitivity of each channel. The default ("maximum
//! ignorance") calibration solves for the 3×3 matrix that best maps sensor
//! response to CIE XYZ when the *sensor curves themselves* are used as the
//! sample spectra — `i1d3_comp_calmat()` with the MIbLSr default in ArgyllCMS.
//!
//! With exactly 3 sample spectra the fit is an exact solve:
//!
//! ```text
//! A[c][i]  = Σ_w  obs_c(w) · sens_i(w)     (XYZ response to sensor curve i)
//! B[i][j]  = Σ_w  sens_i(w) · sens_j(w)    (sensor response to sensor curve j)
//! mat      = (Km · A) · B⁻¹,   Km = 0.683002 lm/mW
//! XYZ      = mat · rgb_hz      (Y in cd/m² for an emissive measurement)
//! ```
//!
//! Two normalization details matter for matching ArgyllCMS's absolute
//! numbers: the XYZ-side integral runs over the *observer's* range (360–830
//! nm) with the sensor curves extended by their edge values outside 380–730
//! (Argyll's resampling semantics), while the sensor-side Gram matrix runs
//! over the sensor range only; and Km is 0.683 — not 683 — because the
//! sensitivities are stored per mW.

use super::eeprom::{SPECTRAL_BANDS, SPECTRAL_WL_SHORT};
use super::observer::{BANDS, WL_SHORT, X_BAR, Y_BAR, Z_BAR};

/// lm/mW — the photometric constant matching sensitivities in Hz per mW/nm.
const KM_PER_MW: f64 = 0.683002;

/// Compute the emissive calibration matrix from the three sensor
/// sensitivity curves. Returns `None` if the sensor Gram matrix is singular
/// (degenerate/garbage calibration data).
// Wavelength-indexed sums across parallel tables; explicit indices read
// clearer than iterator chains here (matches the Spyder conversion code).
#[allow(clippy::needless_range_loop)]
pub fn comp_calmat(sens: &[[f64; SPECTRAL_BANDS]; 3]) -> Option<[[f64; 3]; 3]> {
    let observer = [&X_BAR, &Y_BAR, &Z_BAR];

    // XYZ response to each sensor curve, integrated over the observer's
    // range with edge-value extension of the sensor curves.
    let mut a = [[0.0f64; 3]; 3];
    for w in 0..BANDS {
        let wl = WL_SHORT + w; // nm
        let si = wl.saturating_sub(SPECTRAL_WL_SHORT).min(SPECTRAL_BANDS - 1);
        for c in 0..3 {
            for i in 0..3 {
                a[c][i] += observer[c][w] * sens[i][si];
            }
        }
    }

    // Sensor response to each sensor curve (Gram matrix), sensor range only.
    let mut b = [[0.0f64; 3]; 3];
    for k in 0..SPECTRAL_BANDS {
        for i in 0..3 {
            for j in 0..3 {
                b[i][j] += sens[i][k] * sens[j][k];
            }
        }
    }

    let b_inv = inverse3x3(&b)?;

    // mat = (Km · A) · B⁻¹
    let mut mat = [[0.0f64; 3]; 3];
    for c in 0..3 {
        for j in 0..3 {
            for k in 0..3 {
                mat[c][j] += KM_PER_MW * a[c][k] * b_inv[k][j];
            }
        }
    }
    Some(mat)
}

/// Multiply a 3-vector by a 3×3 matrix: `out = m · v`.
pub fn mul3x3_vec(m: &[[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// Invert a 3×3 matrix; `None` when (near-)singular.
fn inverse3x3(m: &[[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv_det = 1.0 / det;
    let mut inv = [[0.0f64; 3]; 3];
    inv[0][0] = (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv_det;
    inv[0][1] = (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv_det;
    inv[0][2] = (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv_det;
    inv[1][0] = (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv_det;
    inv[1][1] = (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv_det;
    inv[1][2] = (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv_det;
    inv[2][0] = (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv_det;
    inv[2][1] = (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv_det;
    inv[2][2] = (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv_det;
    Some(inv)
}

#[cfg(test)]
#[allow(clippy::needless_range_loop)] // index math mirrors the spec'd sums
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn inverse_recovers_identity() {
        let m = [[2.0, 0.0, 1.0], [0.0, 3.0, 0.0], [1.0, 0.0, 1.0]];
        let inv = inverse3x3(&m).unwrap();
        // m · m⁻¹ = I
        for i in 0..3 {
            for j in 0..3 {
                let mut s = 0.0;
                for k in 0..3 {
                    s += m[i][k] * inv[k][j];
                }
                assert!(approx(s, if i == j { 1.0 } else { 0.0 }, 1e-12));
            }
        }
    }

    #[test]
    fn singular_matrix_is_rejected() {
        let m = [[1.0, 2.0, 3.0], [2.0, 4.0, 6.0], [0.0, 0.0, 1.0]];
        assert!(inverse3x3(&m).is_none());
    }

    /// The exact-solve property: with 3 samples the fitted matrix must map
    /// each sample's sensor response to that sample's XYZ *exactly*
    /// (mat · B = Km · A), whatever the sensor curves are.
    #[test]
    fn calmat_is_exact_for_three_samples() {
        // Arbitrary smooth positive sensor curves.
        let mut sens = Box::new([[0.0f64; SPECTRAL_BANDS]; 3]);
        for k in 0..SPECTRAL_BANDS {
            let x = k as f64 / SPECTRAL_BANDS as f64;
            sens[0][k] = 10.0 * (-(x - 0.7) * (x - 0.7) / 0.02).exp() + 0.1;
            sens[1][k] = 12.0 * (-(x - 0.5) * (x - 0.5) / 0.03).exp() + 0.1;
            sens[2][k] = 9.0 * (-(x - 0.2) * (x - 0.2) / 0.015).exp() + 0.1;
        }
        let mat = comp_calmat(&sens).unwrap();

        // Recompute A and B the same way and verify mat · B == Km · A.
        let observer = [&X_BAR, &Y_BAR, &Z_BAR];
        let mut a = [[0.0f64; 3]; 3];
        for w in 0..BANDS {
            let wl = WL_SHORT + w;
            let si = wl.saturating_sub(SPECTRAL_WL_SHORT).min(SPECTRAL_BANDS - 1);
            for c in 0..3 {
                for i in 0..3 {
                    a[c][i] += observer[c][w] * sens[i][si];
                }
            }
        }
        let mut b = [[0.0f64; 3]; 3];
        for k in 0..SPECTRAL_BANDS {
            for i in 0..3 {
                for j in 0..3 {
                    b[i][j] += sens[i][k] * sens[j][k];
                }
            }
        }
        for c in 0..3 {
            for j in 0..3 {
                let mut got = 0.0;
                for k in 0..3 {
                    got += mat[c][k] * b[k][j];
                }
                let want = KM_PER_MW * a[c][j];
                assert!(
                    approx(got, want, want.abs() * 1e-9),
                    "({c},{j}): {got} vs {want}"
                );
            }
        }
    }

    /// End-to-end absolute-scale sanity: make the "sensors" the CIE curves
    /// themselves (over 380–730, scaled by 1/Km so the photometric constant
    /// must be applied to land on true XYZ). A flat 1 mW/nm test spectrum
    /// then has known XYZ — `Km · Σ obs` — and the calibrated chain
    /// `mat · rgb_hz` must reproduce it within the spectral-tail error the
    /// edge-extension introduces (well under 2%).
    #[test]
    fn calmat_reproduces_absolute_xyz_for_observer_sensors() {
        let mut sens = Box::new([[0.0f64; SPECTRAL_BANDS]; 3]);
        let observer = [&X_BAR, &Y_BAR, &Z_BAR];
        for i in 0..3 {
            for k in 0..SPECTRAL_BANDS {
                sens[i][k] = observer[i][SPECTRAL_WL_SHORT - WL_SHORT + k] / KM_PER_MW;
            }
        }
        let mat = comp_calmat(&sens).unwrap();

        // Flat spectrum, 1 mW/nm over the sensor range.
        let rgb = [
            sens[0].iter().sum::<f64>(),
            sens[1].iter().sum::<f64>(),
            sens[2].iter().sum::<f64>(),
        ];
        let got = mul3x3_vec(&mat, rgb);
        for c in 0..3 {
            let want: f64 = KM_PER_MW
                * observer[c][SPECTRAL_WL_SHORT - WL_SHORT..]
                    .iter()
                    .take(SPECTRAL_BANDS)
                    .sum::<f64>();
            assert!(
                approx(got[c], want, want * 0.02),
                "{c}: {} vs {}",
                got[c],
                want
            );
        }
    }
}
