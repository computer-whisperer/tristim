//! Decision logic for adaptive emissive measurement — pure planning, no I/O.
//!
//! The instrument offers two measurement primitives (see
//! [`mod`](super)-level docs): *frequency* mode counts L2F edges over a host-
//! chosen integration time, *period* mode counts 12 MHz clocks until a
//! host-chosen number of edges has been seen. Frequency mode has a fixed,
//! predictable duration but ±1-count quantization (bad when dim); period mode
//! has negligible quantization but a duration inversely proportional to the
//! light level (unbounded when dark).
//!
//! The strategy here drives every channel toward a bounded-time, bounded-error
//! reading in at most three commands:
//!
//! 1. **Survey** — one frequency measurement of [`SURVEY_INTTIME`] for all
//!    channels. A channel whose count already meets [`MIN_ACCEPT_COUNT`]
//!    (quantization error ≤ 1/[`MIN_ACCEPT_COUNT`]) is done. A channel with at
//!    least [`MIN_ESTIMATE_COUNT`] counts yields a frequency estimate for
//!    step 3. Anything darker is effectively unmeasured.
//! 2. **Probe** — unmeasured channels get a minimal 2-edge period measurement,
//!    purely to obtain an estimate. A probe that sees no edges within the
//!    firmware's window means the channel is dark: report 0 Hz.
//! 3. **Refine** — channels with an estimate get one period measurement sized
//!    to span [`REFINE_SPAN`] seconds of integration (enough to average
//!    display flicker/PWM; quantization in period mode is a non-issue). A
//!    channel too dim to refine within [`REFINE_BUDGET`] seconds keeps its
//!    estimate — by then the estimate itself integrated longer than the span
//!    target anyway.
//!
//! The planner is deliberately decoupled from USB so the policy is unit-
//! testable; [`super::I1d3::measure_emissive_hz`] is the I/O loop around it.

/// Sensor clock frequency (Hz) — the time base of period measurements.
pub(super) const CLK_FREQ: f64 = 12e6;

/// L2F sensor saturation frequency (Hz). Readings above this are unreliable.
pub(super) const SAT_FREQ: f64 = 250e3;

/// Survey integration time (seconds). Long enough that any channel a display
/// can plausibly produce mid-gray on finishes in one command (≥ 1.6 kHz hits
/// [`MIN_ACCEPT_COUNT`]); short enough to keep the bright-patch path snappy.
pub(super) const SURVEY_INTTIME: f64 = 0.25;

/// Survey counts at which a channel is accepted outright: quantization error
/// is at most `1/MIN_ACCEPT_COUNT` (0.25%), comparable to the instrument's
/// other error sources.
const MIN_ACCEPT_COUNT: f64 = 400.0;

/// Minimum survey counts that still make a usable refinement seed. Below
/// this the relative error of the estimate exceeds ~25% and edge targets
/// derived from it are guesswork — probe in period mode instead.
const MIN_ESTIMATE_COUNT: f64 = 4.0;

/// Target integration span (seconds) of a refinement period measurement.
/// Sized to average out display refresh/PWM modulation, the dominant error
/// for dim period-mode readings.
const REFINE_SPAN: f64 = 1.0;

/// Ceiling (seconds) on what we'll spend refining one channel. If even the
/// minimal 2-edge measurement is expected to exceed this, the channel is so
/// dim that its probe already integrated longer than [`REFINE_SPAN`] — keep
/// the estimate.
const REFINE_BUDGET: f64 = 4.0;

/// Largest edge target the period command accepts (u16, even, leaving
/// headroom below 0xFFFF).
const MAX_EDGES: f64 = 65534.0;

/// Where a channel stands after the survey measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Disposition {
    /// Survey counts suffice; the frequency (Hz) is final.
    Done(f64),
    /// Too dark for the survey to even seed a refinement — needs a 2-edge
    /// period probe.
    Probe,
    /// Survey yields a frequency estimate (Hz) good enough to size a
    /// refinement measurement.
    Refine(f64),
}

/// Classify one channel from its survey counts (`count` over `inttime`
/// seconds, both-edges convention: frequency = count / 2·inttime).
///
/// Saturation is the caller's job: a `Done` frequency may exceed
/// [`SAT_FREQ`] and must be checked before use (a saturated channel always
/// classifies `Done` — at [`SURVEY_INTTIME`] it is far above
/// [`MIN_ACCEPT_COUNT`]).
pub(super) fn assess_survey(count: f64, inttime: f64) -> Disposition {
    let hz = 0.5 * count / inttime;
    if count >= MIN_ACCEPT_COUNT {
        Disposition::Done(hz)
    } else if count >= MIN_ESTIMATE_COUNT {
        Disposition::Refine(hz)
    } else {
        Disposition::Probe
    }
}

/// What to do with a channel that has a frequency estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefinePlan {
    /// Keep the estimate; refining is pointless (dark) or over budget.
    Keep,
    /// Take a period measurement with this (even) edge target.
    Measure(u16),
}

/// Size the refinement for an estimated frequency.
pub(super) fn plan_refinement(est_hz: f64) -> RefinePlan {
    if est_hz <= 0.0 {
        return RefinePlan::Keep;
    }
    // Expected span of the cheapest possible measurement (2 edges ≈ one
    // full cycle): 1/est_hz seconds. Past the budget, don't bother.
    if 1.0 / est_hz > REFINE_BUDGET {
        return RefinePlan::Keep;
    }
    // Edges to integrate for REFINE_SPAN seconds (both-edges convention),
    // clamped to the command's legal range and rounded down to even.
    let edges = (2.0 * est_hz * REFINE_SPAN).clamp(2.0, MAX_EDGES);
    RefinePlan::Measure(2 * (edges / 2.0).floor() as u16)
}

/// Frequency (Hz) from a period measurement: `edges` seen over `clocks`
/// 12 MHz clocks. `None` when the measurement timed out without counting
/// (`clocks` ≈ 0) — the caller decides what that means (dark channel for a
/// probe, keep-the-estimate for a refinement).
pub(super) fn period_hz(edges: u16, clocks: f64) -> Option<f64> {
    if clocks < 0.5 {
        return None;
    }
    Some(0.5 * f64::from(edges) * CLK_FREQ / clocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bright_channel_is_done_at_survey() {
        // 10 kHz: 5000 counts over 0.25 s.
        let d = assess_survey(5000.0, 0.25);
        assert_eq!(d, Disposition::Done(10_000.0));
    }

    #[test]
    fn acceptance_boundary() {
        assert!(matches!(assess_survey(400.0, 0.25), Disposition::Done(_)));
        assert!(matches!(assess_survey(399.0, 0.25), Disposition::Refine(_)));
    }

    #[test]
    fn mid_channel_gets_estimate() {
        // 100 counts over 0.25 s → 200 Hz estimate.
        let d = assess_survey(100.0, 0.25);
        assert_eq!(d, Disposition::Refine(200.0));
    }

    #[test]
    fn dark_channel_needs_probe() {
        assert_eq!(assess_survey(0.0, 0.25), Disposition::Probe);
        assert_eq!(assess_survey(3.0, 0.25), Disposition::Probe);
    }

    #[test]
    fn refinement_spans_the_target() {
        // 200 Hz → 400 edges ≈ 1 s of integration.
        assert_eq!(plan_refinement(200.0), RefinePlan::Measure(400));
    }

    #[test]
    fn refinement_edge_target_is_even_and_clamped() {
        // 151.5 Hz → 303 edges, rounded down to even.
        assert_eq!(plan_refinement(151.5), RefinePlan::Measure(302));
        // Very bright estimate clamps at the command maximum.
        assert_eq!(plan_refinement(1e6), RefinePlan::Measure(65534));
        // Very dim (but within budget) clamps at the minimum.
        assert_eq!(plan_refinement(0.3), RefinePlan::Measure(2));
    }

    #[test]
    fn too_dim_keeps_estimate() {
        // 0.2 Hz: a 2-edge measurement is expected to take 5 s > budget.
        assert_eq!(plan_refinement(0.2), RefinePlan::Keep);
        assert_eq!(plan_refinement(0.0), RefinePlan::Keep);
    }

    #[test]
    fn period_conversion() {
        // 400 edges over 12e6 clocks (1 s) → 200 Hz.
        assert_eq!(period_hz(400, 12e6), Some(200.0));
        // Timed-out channel reports no frequency.
        assert_eq!(period_hz(400, 0.0), None);
    }
}
