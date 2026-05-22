//! PQ (SMPTE ST 2084) inverse OETF — convert absolute nits to the
//! [0, 1] code value the compositor + panel decode through the PQ
//! EOTF. The compositor's decode shader is the matching forward
//! EOTF (`crates/prism-renderer/shaders/decode.frag::pq_eotf`),
//! and the panel applies its own PQ EOTF in the scan-out chain, so a
//! patch we write with this encoder lands as the requested luminance
//! at the panel (modulo per-panel calibration error — what Spyder
//! exists to measure).
//!
//! ST 2084 references peak luminance at 10000 cd/m². Values >10000
//! clamp to 1.0. Values <0 clamp to 0.0. The forward mapping is the
//! "inverse PQ" / PQ OETF defined in BT.2100.

const M1: f64 = 2610.0 / 16384.0;
const M2: f64 = 2523.0 / 4096.0 * 128.0;
const C1: f64 = 3424.0 / 4096.0;
const C2: f64 = 2413.0 / 4096.0 * 32.0;
const C3: f64 = 2392.0 / 4096.0 * 32.0;

/// PQ inverse EOTF (OETF): luminance Y in cd/m² → encoded V in [0, 1].
/// Used to write PQ-encoded values into a scanout buffer that, after
/// the panel's PQ EOTF, emits the originally requested luminance.
///
/// Domain: [0, 10000] cd/m². Inputs outside this range are clamped
/// (negative → 0.0, >10000 → 1.0) — neither is meaningful as a
/// display target but we'd rather degrade than panic on stray data.
pub fn nits_to_pq(nits: f64) -> f64 {
    let y = (nits.max(0.0) / 10_000.0).min(1.0);
    let ym1 = y.powf(M1);
    let num = C1 + C2 * ym1;
    let den = 1.0 + C3 * ym1;
    (num / den).powf(M2)
}

/// Convenience for a per-channel triple.
pub fn nits_triple_to_pq(rgb_nits: [f64; 3]) -> [f64; 3] {
    [
        nits_to_pq(rgb_nits[0]),
        nits_to_pq(rgb_nits[1]),
        nits_to_pq(rgb_nits[2]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: f64, b: f64, tol: f64) {
        assert!(
            (a - b).abs() < tol,
            "expected ~{b}, got {a} (delta {}, tol {tol})",
            (a - b).abs()
        );
    }

    // PQ OETF anchor values from SMPTE ST 2084 Annex B / BT.2408.
    // 100 cd/m² → V ≈ 0.508 ("SDR reference white in PQ HDR" per
    // BT.2408 is 203 cd/m² → V ≈ 0.580).
    //
    // Note: the PQ OETF is not zero at zero — it has a small "lift"
    // V(0) = c1^m2 ≈ 7.31e-7. The inverse EOTF still recovers 0 from
    // that value (`max(V^(1/m2) - c1, 0)` floors at 0), so the patch
    // we write lands as 0 nits on a spec-compliant panel.

    #[test]
    fn anchor_0_nits_has_pq_lift() {
        assert_close(nits_to_pq(0.0), 7.31e-7, 1e-8);
    }

    #[test]
    fn anchor_100_nits() {
        assert_close(nits_to_pq(100.0), 0.5081, 1e-3);
    }

    #[test]
    fn anchor_203_nits_reference_white() {
        assert_close(nits_to_pq(203.0), 0.5806, 1e-3);
    }

    #[test]
    fn anchor_1000_nits() {
        assert_close(nits_to_pq(1000.0), 0.7518, 1e-3);
    }

    #[test]
    fn anchor_10000_nits_peak() {
        assert_close(nits_to_pq(10_000.0), 1.0, 1e-9);
    }

    #[test]
    fn clamp_below_zero_lands_at_pq_lift() {
        // Same as anchor_0_nits — clamp to 0, then encode → c1^m2.
        assert_close(nits_to_pq(-5.0), 7.31e-7, 1e-8);
    }

    #[test]
    fn clamp_above_peak() {
        assert_eq!(nits_to_pq(20_000.0), 1.0);
    }
}
