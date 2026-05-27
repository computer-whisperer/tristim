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

use std::collections::HashMap;
use std::thread::sleep;
use std::time::Duration;

use tristim_capture as cap;
use tristim_color::metrics::{delta_e76, xyz_to_lab};
use tristim_display::PatchSurface;
use tristim_driver::{Calibration, Colorimeter, MeasurementConfidence, Setup, TrustFlag, Xyz};

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
    /// A probe point was just measured (coarse probe — fixed point set).
    Point {
        index: usize,
        total: usize,
        label: &'static str,
        code_value: [f64; 3],
        measured: Xyz,
        flags: Vec<TrustFlag>,
    },
    /// A point was just measured during adaptive refinement (no fixed total).
    Measured {
        index: usize,
        code_value: [f64; 3],
        measured: Xyz,
        flags: Vec<TrustFlag>,
    },
}

/// Open the colorimeter and a patch surface for the configured format, run the
/// puck-placement countdown, and return the pieces a probe loop needs. Shared
/// by the coarse and refined probes. Errors if the compositor rejects the
/// format outright — we can't probe an encoding it won't put on screen.
fn open_session(
    config: &GamutConfig,
    on_event: &mut impl FnMut(GamutEvent),
    should_cancel: &impl Fn() -> bool,
) -> Result<(Colorimeter, Calibration, Setup, PatchSurface), GatherError> {
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

    Ok((device, cal, setup, surface))
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
    let (mut device, cal, setup, mut surface) =
        open_session(config, &mut on_event, &should_cancel)?;

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

// ── adaptive refinement ─────────────────────────────────────────────────────

/// The 6 cube faces as `(fixed axis, value)`: axis 0=R, 1=G, 2=B.
const FACE_DEFS: [(usize, f64); 6] = [(0, 0.0), (0, 1.0), (1, 0.0), (1, 1.0), (2, 0.0), (2, 1.0)];

/// Code value on face `(axis, value)` at face coordinates `(s, t)`: the fixed
/// axis is held at `value`, the other two sweep `s` and `t` in axis order.
fn face_cv(axis: usize, value: f64, s: f64, t: f64) -> [f64; 3] {
    let mut cv = [0.0; 3];
    cv[axis] = value;
    let others = match axis {
        0 => [1, 2],
        1 => [0, 2],
        _ => [0, 1],
    };
    cv[others[0]] = s;
    cv[others[1]] = t;
    cv
}

/// Human label for a face, e.g. `"R=1"`.
pub fn face_label(axis: usize, value: f64) -> String {
    let name = ["R", "G", "B"][axis];
    format!("{name}={}", value as u8)
}

/// One measured point: what came back for a requested code value, plus the
/// coarse trust verdict the refinement logic gates on. The hardware path fills
/// this from a [`MeasurementConfidence`]; tests inject a synthetic display.
#[derive(Debug, Clone, Copy)]
pub struct ProbeSample {
    pub measured: Xyz,
    /// Don't subdivide on a deviation from an untrustworthy point — it may be
    /// noise. Low-trust corners cap subdivision instead.
    pub trustworthy: bool,
}

/// Tunable thresholds for [`refine_gamut`]. All ΔE figures are CIE76 in the
/// measured-white-referenced Lab the mesh lives in.
#[derive(Debug, Clone, Copy)]
pub struct RefineParams {
    /// Maximum subdivision depth per face.
    pub max_depth: u32,
    /// Stop subdividing once the patch center is within this ΔE of the bilinear
    /// average of its corners (the patch is planar enough).
    pub flat_eps: f64,
    /// A patch whose corners all fall within this ΔE of each other has
    /// collapsed in measured space — the pipeline clamped it.
    pub fold_eps: f64,
    /// …but only call that a fold (vs. ordinary convergence) when the patch
    /// still spans at least this much code-value side length.
    pub fold_min_side: f64,
}

impl Default for RefineParams {
    fn default() -> Self {
        Self {
            max_depth: 3,
            flat_eps: 2.0,
            fold_eps: 0.5,
            fold_min_side: 0.25,
        }
    }
}

/// Why a leaf patch stopped subdividing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchStatus {
    /// Planar enough (center within `flat_eps` of the corner bilinear average).
    Flat,
    /// Corners collapsed in measured space over a non-trivial code-value area —
    /// the pipeline clamped this region onto the gamut boundary.
    Folded,
    /// Hit the depth cap while still curved.
    MaxDepth,
    /// A corner was untrustworthy; stopped rather than chase noise.
    LowTrust,
}

impl PatchStatus {
    /// Stable string for the capture schema.
    pub fn as_str(&self) -> &'static str {
        match self {
            PatchStatus::Flat => "flat",
            PatchStatus::Folded => "folded",
            PatchStatus::MaxDepth => "max_depth",
            PatchStatus::LowTrust => "low_trust",
        }
    }
}

/// A measured boundary vertex: the code value we asked for and what came back.
#[derive(Debug, Clone, Copy)]
pub struct MeshVertex {
    pub code_value: [f64; 3],
    pub measured: Xyz,
    /// CIELAB relative to the measured white.
    pub lab: [f64; 3],
    pub trustworthy: bool,
}

/// A leaf patch of a refined face: its 4 corner vertices (CCW) and why it stopped.
#[derive(Debug, Clone, Copy)]
pub struct Patch {
    pub axis: usize,
    pub value: f64,
    pub corners: [usize; 4],
    pub status: PatchStatus,
}

impl Patch {
    pub fn face_label(&self) -> String {
        face_label(self.axis, self.value)
    }
}

/// The refined measured gamut: deduped boundary vertices + the quadtree leaf
/// patches, in the measured-white Lab frame.
#[derive(Debug, Clone)]
pub struct GamutMesh {
    pub white: Xyz,
    pub vertices: Vec<MeshVertex>,
    pub patches: Vec<Patch>,
}

impl GamutMesh {
    /// Count leaf patches with a given status.
    pub fn count(&self, status: PatchStatus) -> usize {
        self.patches.iter().filter(|p| p.status == status).count()
    }

    /// The measured vertex for an exact code value, if it was probed.
    pub fn vertex_at(&self, cv: [f64; 3]) -> Option<&MeshVertex> {
        self.vertices.iter().find(|v| v.code_value == cv)
    }

    /// Convert to the serializable capture-schema form.
    pub fn to_capture(&self) -> cap::MeasuredGamut {
        cap::MeasuredGamut {
            white_xyz: [self.white.x, self.white.y, self.white.z],
            vertices: self
                .vertices
                .iter()
                .map(|v| cap::GamutVertex {
                    code_value: v.code_value,
                    xyz: [v.measured.x, v.measured.y, v.measured.z],
                    lab: v.lab,
                    trustworthy: v.trustworthy,
                })
                .collect(),
            patches: self
                .patches
                .iter()
                .map(|p| cap::GamutPatch {
                    face: p.face_label(),
                    corners: p.corners,
                    status: p.status.as_str().to_string(),
                })
                .collect(),
        }
    }
}

/// Adaptive cube-surface refinement, generic over how a code value is measured
/// (`measure`) and that operation's error type — so the hardware path injects a
/// colorimeter and tests inject a synthetic display. Measures the white corner
/// first to fix the Lab reference, then refines the 6 faces.
pub fn refine_gamut<M, E>(params: &RefineParams, mut measure: M) -> Result<GamutMesh, E>
where
    M: FnMut([f64; 3]) -> Result<ProbeSample, E>,
{
    let white_ps = measure(WHITE_CV)?;
    let white = white_ps.measured;

    let mut ctx = RefineCtx {
        params,
        white,
        vertices: Vec::new(),
        cache: HashMap::new(),
        patches: Vec::new(),
    };
    // Pre-insert white as vertex 0 so the faces sharing it hit the cache.
    let wlab = xyz_to_lab([white.x, white.y, white.z], [white.x, white.y, white.z]);
    ctx.vertices.push(MeshVertex {
        code_value: WHITE_CV,
        measured: white,
        lab: wlab,
        trustworthy: white_ps.trustworthy,
    });
    ctx.cache.insert(cv_key(WHITE_CV), 0);

    for &(axis, value) in &FACE_DEFS {
        ctx.refine(axis, value, 0.0, 1.0, 0.0, 1.0, 0, &mut measure)?;
    }

    Ok(GamutMesh {
        white,
        vertices: ctx.vertices,
        patches: ctx.patches,
    })
}

/// Exact dedup key for a code value. The bisection only ever produces dyadic
/// rationals, so the bit pattern is a stable exact key.
fn cv_key(cv: [f64; 3]) -> [u64; 3] {
    [cv[0].to_bits(), cv[1].to_bits(), cv[2].to_bits()]
}

struct RefineCtx<'a> {
    params: &'a RefineParams,
    white: Xyz,
    vertices: Vec<MeshVertex>,
    cache: HashMap<[u64; 3], usize>,
    patches: Vec<Patch>,
}

impl RefineCtx<'_> {
    /// Measure (or recall from the cache) the vertex at face coordinates, return
    /// its index.
    fn sample<M, E>(&mut self, cv: [f64; 3], measure: &mut M) -> Result<usize, E>
    where
        M: FnMut([f64; 3]) -> Result<ProbeSample, E>,
    {
        if let Some(&idx) = self.cache.get(&cv_key(cv)) {
            return Ok(idx);
        }
        let ps = measure(cv)?;
        let lab = xyz_to_lab(
            [ps.measured.x, ps.measured.y, ps.measured.z],
            [self.white.x, self.white.y, self.white.z],
        );
        let idx = self.vertices.len();
        self.vertices.push(MeshVertex {
            code_value: cv,
            measured: ps.measured,
            lab,
            trustworthy: ps.trustworthy,
        });
        self.cache.insert(cv_key(cv), idx);
        Ok(idx)
    }

    #[allow(clippy::too_many_arguments)]
    fn refine<M, E>(
        &mut self,
        axis: usize,
        value: f64,
        s0: f64,
        s1: f64,
        t0: f64,
        t1: f64,
        depth: u32,
        measure: &mut M,
    ) -> Result<(), E>
    where
        M: FnMut([f64; 3]) -> Result<ProbeSample, E>,
    {
        let c = [
            self.sample(face_cv(axis, value, s0, t0), measure)?,
            self.sample(face_cv(axis, value, s0, t1), measure)?,
            self.sample(face_cv(axis, value, s1, t1), measure)?,
            self.sample(face_cv(axis, value, s1, t0), measure)?,
        ];
        let labs: [[f64; 3]; 4] = [
            self.vertices[c[0]].lab,
            self.vertices[c[1]].lab,
            self.vertices[c[2]].lab,
            self.vertices[c[3]].lab,
        ];
        // Measured-space size of the patch = the widest corner-to-corner ΔE.
        let mut spread = 0.0_f64;
        for i in 0..4 {
            for j in (i + 1)..4 {
                spread = spread.max(delta_e76(labs[i], labs[j]));
            }
        }
        let side = s1 - s0;
        let emit = |this: &mut Self, status| {
            this.patches.push(Patch {
                axis,
                value,
                corners: c,
                status,
            });
        };

        if spread < self.params.fold_eps {
            let status = if side >= self.params.fold_min_side {
                PatchStatus::Folded
            } else {
                PatchStatus::Flat
            };
            emit(self, status);
            return Ok(());
        }
        if depth >= self.params.max_depth {
            emit(self, PatchStatus::MaxDepth);
            return Ok(());
        }
        if !c.iter().all(|&i| self.vertices[i].trustworthy) {
            emit(self, PatchStatus::LowTrust);
            return Ok(());
        }

        let sm = 0.5 * (s0 + s1);
        let tm = 0.5 * (t0 + t1);
        let center = self.sample(face_cv(axis, value, sm, tm), measure)?;
        let bilinear = [
            0.25 * (labs[0][0] + labs[1][0] + labs[2][0] + labs[3][0]),
            0.25 * (labs[0][1] + labs[1][1] + labs[2][1] + labs[3][1]),
            0.25 * (labs[0][2] + labs[1][2] + labs[2][2] + labs[3][2]),
        ];
        if delta_e76(self.vertices[center].lab, bilinear) < self.params.flat_eps {
            emit(self, PatchStatus::Flat);
            return Ok(());
        }

        // Curved and trustworthy: split into 4 quadrants.
        self.refine(axis, value, s0, sm, t0, tm, depth + 1, measure)?;
        self.refine(axis, value, sm, s1, t0, tm, depth + 1, measure)?;
        self.refine(axis, value, s0, sm, tm, t1, depth + 1, measure)?;
        self.refine(axis, value, sm, s1, tm, t1, depth + 1, measure)?;
        Ok(())
    }
}

/// Hardware entry: drive the colorimeter + patch surface through an adaptive
/// refinement. Per-measurement progress is reported as [`GamutEvent::Measured`].
pub fn probe_gamut_refined(
    config: &GamutConfig,
    params: &RefineParams,
    mut on_event: impl FnMut(GamutEvent),
    should_cancel: impl Fn() -> bool,
) -> Result<GamutMesh, GatherError> {
    let (mut device, cal, setup, mut surface) =
        open_session(config, &mut on_event, &should_cancel)?;

    let mut index = 0usize;
    let measure = |cv: [f64; 3]| -> Result<ProbeSample, GatherError> {
        surface.set_code_values(cv)?;
        sleep(config.settle);
        // Burst within a point; reset between points (auto_zero=false resets once).
        let raws = device.measure_raw_repeated(&setup, config.repeats, false)?;
        let confidence = MeasurementConfidence::from_repeats(&raws, &setup, &cal);
        on_event(GamutEvent::Measured {
            index,
            code_value: cv,
            measured: confidence.mean,
            flags: confidence.flags(),
        });
        index += 1;
        Ok(ProbeSample {
            measured: confidence.mean,
            trustworthy: confidence.is_trustworthy(),
        })
    };

    let mesh = refine_gamut(params, measure)?;
    let _ = surface.set_code_values([0.0, 0.0, 0.0]);
    Ok(mesh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    // sRGB linear-RGB → XYZ (D65). A smooth additive display for the synthetic
    // models below.
    const SRGB_TO_XYZ: [[f64; 3]; 3] = [
        [0.4124, 0.3576, 0.1805],
        [0.2126, 0.7152, 0.0722],
        [0.0193, 0.1192, 0.9505],
    ];

    fn mat_mul(m: [[f64; 3]; 3], v: [f64; 3]) -> Xyz {
        Xyz {
            x: m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
            y: m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
            z: m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
        }
    }

    /// A well-behaved additive display: cv maps straight to linear RGB → XYZ.
    /// The XYZ map is affine, but Lab is curved, so faces still need subdivision.
    fn smooth(cv: [f64; 3]) -> Result<ProbeSample, Infallible> {
        Ok(ProbeSample {
            measured: mat_mul(SRGB_TO_XYZ, cv),
            trustworthy: true,
        })
    }

    /// A display whose gamut is smaller than the container: each channel clamps
    /// at 0.5, so the high-saturation corner of every face collapses to one
    /// point — a fold the probe must detect (and not let collapse the gamut).
    fn clamped(cv: [f64; 3]) -> Result<ProbeSample, Infallible> {
        let disp = cv.map(|c| c.min(0.5));
        Ok(ProbeSample {
            measured: mat_mul(SRGB_TO_XYZ, disp),
            trustworthy: true,
        })
    }

    #[test]
    fn smooth_display_converges_no_folds_recovers_primaries() {
        let mesh = refine_gamut(&RefineParams::default(), smooth).unwrap();

        // Adaptivity ran (more than one leaf per face) and nothing folded.
        assert!(
            mesh.patches.len() > FACE_DEFS.len(),
            "expected subdivision beyond the 6 faces, got {} patches",
            mesh.patches.len()
        );
        assert_eq!(mesh.count(PatchStatus::Folded), 0, "no clamping expected");

        // Measured red corner lands on sRGB red xy (0.640, 0.330).
        let red = mesh.vertex_at([1.0, 0.0, 0.0]).unwrap();
        let (x, y) = red.measured.chromaticity().unwrap();
        assert!((x - 0.640).abs() < 1e-3, "red x {x}");
        assert!((y - 0.330).abs() < 1e-3, "red y {y}");
    }

    #[test]
    fn clamped_display_detects_folds() {
        let mesh = refine_gamut(&RefineParams::default(), clamped).unwrap();

        // The collapsed high-saturation corners must register as folds.
        assert!(
            mesh.count(PatchStatus::Folded) > 0,
            "expected at least one folded (clamped) patch"
        );

        // White clamps to half scale, so the gamut is genuinely smaller — not
        // collapsed to nothing.
        assert!(mesh.white.y > 0.0 && mesh.white.y < SRGB_TO_XYZ[1].iter().sum::<f64>());
    }
}
