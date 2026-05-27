//! Closed-loop gamut probe — cube-surface form (coarse, non-adaptive).
//!
//! The measured gamut is the image of the code-value cube under the
//! (compositor + display + encoding) pipeline. For a continuous pipeline the
//! boundary of that solid is the image of the cube's *surface*, so we probe the
//! surface directly — every measurement is a boundary point, no inverse search.
//! This first cut measures a fixed coarse set (the 8 corners + 6 face centers);
//! adaptive subdivision and fold/clip detection come later.
//!
//! Each point is measured as a burst of repeats and scored with
//! [`MeasurementConfidence`], so a low-trust corner (e.g. a saturated primary
//! the display can't reach, read near the sensor floor) is *flagged* rather
//! than silently trusted — the protection the naive primary-scan lacked.

use std::thread::sleep;
use std::time::Duration;

use tristim_capture as cap;
use tristim_driver::{Colorimeter, MeasurementConfidence, TrustFlag, Xyz};

use crate::format::FormatSpec;
use crate::{GatherError, open_format};

/// The coarse cube-surface probe points: 8 corners (black / R,G,B / the three
/// secondaries / white) + the 6 face centers. Code-value triples.
const PROBE_POINTS: &[(&str, [f64; 3])] = &[
    ("black", [0.0, 0.0, 0.0]),
    ("red", [1.0, 0.0, 0.0]),
    ("green", [0.0, 1.0, 0.0]),
    ("blue", [0.0, 0.0, 1.0]),
    ("yellow", [1.0, 1.0, 0.0]),
    ("cyan", [0.0, 1.0, 1.0]),
    ("magenta", [1.0, 0.0, 1.0]),
    ("white", [1.0, 1.0, 1.0]),
    ("R=0", [0.0, 0.5, 0.5]),
    ("R=1", [1.0, 0.5, 0.5]),
    ("G=0", [0.5, 0.0, 0.5]),
    ("G=1", [0.5, 1.0, 0.5]),
    ("B=0", [0.5, 0.5, 0.0]),
    ("B=1", [0.5, 0.5, 1.0]),
];

const WHITE_CV: [f64; 3] = [1.0, 1.0, 1.0];

/// Everything the gamut probe needs. The gamut is per-encoding, so this carries
/// exactly one [`FormatSpec`].
#[derive(Debug, Clone)]
pub struct GamutConfig {
    /// Connector name of the output under test.
    pub output: String,
    /// On-device calibration index used for raw→XYZ.
    pub cal_index: u8,
    /// The encoding whose reproduced gamut we're probing.
    pub format: FormatSpec,
    /// Repeated measurements per probe point (burst within a point).
    pub repeats: usize,
    /// How long to wait after committing a patch before measuring.
    pub settle: Duration,
    /// Countdown given for puck placement before the first measurement.
    pub prep: Duration,
    /// Centered-window area fraction: `1.0` = fullscreen patch.
    pub window_fraction: f64,
    /// Surround code values when `window_fraction < 1.0` (`None` = black).
    pub border: Option<[f64; 3]>,
}

/// One probed boundary point: the code value we requested and what came back.
#[derive(Debug, Clone)]
pub struct GamutVertex {
    /// Human-readable name of the probe point (e.g. `"red"`, `"B=1"`).
    pub label: &'static str,
    /// Requested code value in the format's encoding.
    pub code_value: [f64; 3],
    /// Mean measured XYZ across the repeats.
    pub measured: Xyz,
    /// Trust statistics for this point's repeats.
    pub confidence: MeasurementConfidence,
}

/// The coarse measured gamut: boundary vertices plus the measured white (cv =
/// 1,1,1) kept as the Lab reference for downstream embedding.
#[derive(Debug, Clone)]
pub struct GamutProbe {
    pub white: Xyz,
    pub vertices: Vec<GamutVertex>,
}

/// Progress reported by [`probe_gamut`] as it proceeds.
#[derive(Debug, Clone)]
pub enum GamutEvent {
    DeviceReady {
        product: String,
        serial: String,
        hw_version: (u32, u32),
    },
    /// The compositor's response to the format's description.
    Negotiation(cap::Negotiation),
    /// Puck-placement countdown, fired once per second with a black patch up.
    Countdown { remaining: u64 },
    /// A probe point was just measured.
    Point {
        index: usize,
        total: usize,
        label: &'static str,
        code_value: [f64; 3],
        measured: Xyz,
        flags: Vec<TrustFlag>,
    },
}

/// Run the coarse cube-surface gamut probe, reporting progress through
/// `on_event` and stopping early (between points) if `should_cancel` returns
/// `true`. The colorimeter is opened first, so a missing device fails fast.
///
/// Errors if the compositor rejects the requested format outright — we can't
/// probe an encoding the pipeline won't put on screen.
pub fn probe_gamut(
    config: &GamutConfig,
    mut on_event: impl FnMut(GamutEvent),
    should_cancel: impl Fn() -> bool,
) -> Result<GamutProbe, GatherError> {
    let mut device = Colorimeter::open_any()?;
    let info = device.get_info()?;
    let cal = device.get_calibration(config.cal_index)?;
    let setup = device.get_setup(&cal)?;
    let product = if device.is_spyder_2024() {
        "Spyder 2024"
    } else {
        "SpyderX2"
    };
    on_event(GamutEvent::DeviceReady {
        product: product.to_string(),
        serial: info.serial.clone(),
        hw_version: info.hw_version,
    });

    let (surface, outcome) = open_format(&config.output, &config.format)?;
    on_event(GamutEvent::Negotiation(outcome.clone()));
    let mut surface = match surface {
        Some(s) => s,
        None => {
            let (cause, message) = match outcome {
                cap::Negotiation::Rejected { cause, message } => (cause, message),
                _ => ("no_surface".into(), "format produced no surface".into()),
            };
            return Err(GatherError::FormatRejected { cause, message });
        }
    };
    surface.set_window_fraction(config.window_fraction)?;
    if let Some(b) = config.border {
        surface.set_border(b)?;
    }
    surface.set_code_values([0.0, 0.0, 0.0])?;

    for remaining in (1..=config.prep.as_secs()).rev() {
        on_event(GamutEvent::Countdown { remaining });
        if should_cancel() {
            break;
        }
        sleep(Duration::from_secs(1));
    }

    let total = PROBE_POINTS.len();
    let mut vertices = Vec::with_capacity(total);
    for (index, (label, cv)) in PROBE_POINTS.iter().enumerate() {
        if should_cancel() {
            break;
        }
        surface.set_code_values(*cv)?;
        sleep(config.settle);
        // Burst within a point (reset once, then read): auto-zeroing between
        // readings is free accuracy-wise but the per-reading reset is pure
        // overhead in a tight repeat loop (see `characterize --burst`).
        let raws = device.measure_raw_repeated(&setup, config.repeats, false)?;
        let confidence = MeasurementConfidence::from_repeats(&raws, &setup, &cal);
        on_event(GamutEvent::Point {
            index,
            total,
            label,
            code_value: *cv,
            measured: confidence.mean,
            flags: confidence.flags(),
        });
        vertices.push(GamutVertex {
            label,
            code_value: *cv,
            measured: confidence.mean,
            confidence,
        });
    }
    // Leave the panel dark.
    let _ = surface.set_code_values([0.0, 0.0, 0.0]);

    let white = vertices
        .iter()
        .find(|v| v.code_value == WHITE_CV)
        .map(|v| v.measured)
        .unwrap_or(Xyz {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        });

    Ok(GamutProbe { white, vertices })
}
