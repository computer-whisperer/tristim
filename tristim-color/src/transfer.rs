//! Transfer functions (EOTF = decode, OETF = encode).
//!
//! Decoding maps an encoded code value to light:
//! - sRGB / gamma / BT.1886 decode to **relative** linear luminance `0..=1`.
//! - PQ (ST 2084) decodes to **absolute** luminance in cd/m² (`0..=10000`).
//!
//! Encoding is the inverse, useful for predicting the code value that should
//! produce a target light level.

// ── sRGB ────────────────────────────────────────────────────────────────────

/// sRGB EOTF: encoded `0..=1` → relative linear `0..=1`.
pub fn srgb_eotf(v: f64) -> f64 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB OETF: relative linear `0..=1` → encoded `0..=1`.
pub fn srgb_oetf(l: f64) -> f64 {
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

// ── pure power / BT.1886 ─────────────────────────────────────────────────────

/// Pure-power EOTF with exponent `gamma` (negatives clamped to 0).
pub fn gamma_eotf(v: f64, gamma: f64) -> f64 {
    v.max(0.0).powf(gamma)
}

/// BT.1886 reference EOTF for an ideal display (black level 0, white 1),
/// which reduces to a pure 2.4 power curve.
pub fn bt1886_eotf(v: f64) -> f64 {
    gamma_eotf(v, 2.4)
}

// ── PQ (SMPTE ST 2084) ───────────────────────────────────────────────────────

const M1: f64 = 2610.0 / 16384.0;
const M2: f64 = 2523.0 / 4096.0 * 128.0;
const C1: f64 = 3424.0 / 4096.0;
const C2: f64 = 2413.0 / 4096.0 * 32.0;
const C3: f64 = 2392.0 / 4096.0 * 32.0;

/// PQ EOTF: encoded `0..=1` → absolute luminance in cd/m² (`0..=10000`).
pub fn pq_eotf(v: f64) -> f64 {
    let v = v.clamp(0.0, 1.0);
    let vp = v.powf(1.0 / M2);
    let num = (vp - C1).max(0.0);
    let den = C2 - C3 * vp;
    10_000.0 * (num / den).powf(1.0 / M1)
}

/// PQ inverse EOTF (OETF): luminance in cd/m² → encoded `0..=1`. Inputs are
/// clamped to `[0, 10000]`.
pub fn pq_oetf(nits: f64) -> f64 {
    let y = (nits.max(0.0) / 10_000.0).min(1.0);
    let ym1 = y.powf(M1);
    ((C1 + C2 * ym1) / (1.0 + C3 * ym1)).powf(M2)
}

/// Decode a code value using the transfer function named as in a capture
/// (`"srgb"`, `"st2084_pq"`, `"bt1886"`, `"gamma22"`, `"gamma28"`). Returns
/// relative linear `0..=1` for SDR curves and absolute cd/m² for PQ; `None`
/// for an unrecognized name.
pub fn decode_named(name: &str, v: f64) -> Option<f64> {
    Some(match name {
        "srgb" | "ext_srgb" => srgb_eotf(v),
        "st2084_pq" => pq_eotf(v),
        "bt1886" => bt1886_eotf(v),
        "gamma22" => gamma_eotf(v, 2.2),
        "gamma28" => gamma_eotf(v, 2.8),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "expected ~{b}, got {a} (tol {tol})");
    }

    #[test]
    fn srgb_endpoints_and_round_trip() {
        close(srgb_eotf(0.0), 0.0, 1e-12);
        close(srgb_eotf(1.0), 1.0, 1e-12);
        for &v in &[0.0, 0.02, 0.2, 0.5, 0.8, 1.0] {
            close(srgb_oetf(srgb_eotf(v)), v, 1e-9);
        }
    }

    #[test]
    fn srgb_mid_is_dim() {
        // 0.5 encoded sRGB ≈ 0.214 linear.
        close(srgb_eotf(0.5), 0.2140, 1e-3);
    }

    #[test]
    fn pq_anchor_values() {
        // BT.2408 / ST 2084 anchors.
        close(pq_eotf(0.5081), 100.0, 0.5);
        close(pq_eotf(0.5806), 203.0, 0.5);
        close(pq_eotf(0.7518), 1000.0, 2.0);
        close(pq_eotf(1.0), 10_000.0, 1e-6);
        close(pq_eotf(0.0), 0.0, 1e-6);
    }

    #[test]
    fn pq_round_trips() {
        for &nits in &[0.0, 1.0, 100.0, 203.0, 1000.0, 4000.0, 10_000.0] {
            close(pq_eotf(pq_oetf(nits)), nits, 1e-3);
        }
    }

    #[test]
    fn decode_named_dispatch() {
        close(decode_named("srgb", 1.0).unwrap(), 1.0, 1e-9);
        close(decode_named("st2084_pq", 1.0).unwrap(), 10_000.0, 1e-6);
        close(
            decode_named("gamma22", 0.5).unwrap(),
            0.5f64.powf(2.2),
            1e-12,
        );
        assert!(decode_named("nope", 0.5).is_none());
    }
}
