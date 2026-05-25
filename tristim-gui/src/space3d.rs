//! The **3D sample space**: a [`chart3d`] view that embeds a trial's samples in
//! CIE L\*a\*b\* and shows, in one picture, where the compositor *should* have
//! put each colour versus where it landed.
//!
//! Three marks, all in the same Lab frame the ΔE\*ab scores live in (the trial's
//! reference white, from [`AnalyzedTrial::reference_white_xyz`]):
//!
//! - **points** — one per measured sample at its `measured_lab`, painted in its
//!   own (sRGB-clamped) colour so the cloud reads like the picture it is.
//! - **error vectors** (lines) — `expected_lab → measured_lab` per sample. Lab
//!   is perceptually near-uniform, so each vector's *length* is the reported
//!   ΔE\*ab and its *direction* shows the kind of error (hue / chroma / lightness).
//! - **gamut wireframe** (lines) — the ideal RGB cube of the trial's colour
//!   space, mapped corner-by-corner into the same Lab frame and subdivided
//!   (cube edges curve in Lab). It is the surface the measured points *would*
//!   sit on if the display were perfect, so drift off it is visible at a glance.
//!
//! Geometry handles are built once per trial and cached on the presenter (see
//! [`Space3dScene`]); `build` clones them into a fresh [`SceneSpec`] each frame,
//! so the backend re-uploads nothing while the camera moves.

use aetna_core::prelude::*;
use aetna_core::scene::glam::Vec3;
use aetna_core::scene::{
    GridPlanes, GridSettings, LineData, LineSegment, LineStyle, LinesHandle, PointData,
    PointLabels, PointShape, PointStyle, PointsHandle, ScenePoint, SceneSpec, SceneStyle, SizeMode,
};

use tristim_analyze::{AnalyzedSample, AnalyzedTrial, GroundTruth};
use tristim_color::{ColorSpace, mat3_mul_vec, metrics, transfer};

/// Cached scene geometry for one analyzed trial. Cheap to clone into a
/// [`SceneSpec`] each frame (the handles are `Arc`s); rebuilt only when the
/// selected trial changes.
pub struct Space3dScene {
    /// Trial index these handles were built for — the cache key.
    pub trial: usize,
    /// Measured samples, each at its `measured_lab`, coloured by measured colour.
    points: PointsHandle,
    /// Per-point hover labels (ΔE / L\*), aligned with `points`.
    point_labels: PointLabels,
    /// `expected → measured` displacement per scorable sample.
    vectors: LinesHandle,
    /// The trial colour space's RGB cube, mapped into the Lab frame.
    gamut: LinesHandle,
    /// Whether any sample could be embedded (false ⇒ show a placeholder note).
    has_data: bool,
}

impl Space3dScene {
    /// Build the cached geometry for `trial` (index `idx`).
    pub fn build(trial: &AnalyzedTrial, idx: usize) -> Self {
        // Reference white the analyzer placed samples against; 0 ⇒ unscored.
        let white = trial.reference_white_xyz;
        let white_y = white.map_or(0.0, |w| w[1]);

        let mut points = Vec::new();
        let mut labels = Vec::new();
        let mut vectors = Vec::new();
        for s in &trial.samples {
            let Some(mlab) = s.measured_lab else { continue };
            points.push(ScenePoint {
                position: lab_to_world(mlab),
                color: display_color(s.measured_xyz, white_y),
            });
            labels.push(point_label(s, mlab));
            if let Some(elab) = s.expected_lab {
                vectors.push(LineSegment {
                    start: lab_to_world(elab),
                    end: lab_to_world(mlab),
                    color: VECTOR_COLOR,
                });
            }
        }

        let gamut = match (&trial.ground_truth, white) {
            (GroundTruth::Known { space, .. }, Some(white_xyz)) => {
                gamut_wireframe(space, white_xyz)
            }
            _ => Vec::new(),
        };

        let has_data = !points.is_empty();
        Self {
            trial: idx,
            points: PointsHandle::new(PointData { points }),
            point_labels: PointLabels::new(labels).on_hover(),
            vectors: LinesHandle::new(LineData { segments: vectors }),
            gamut: LinesHandle::new(LineData { segments: gamut }),
            has_data,
        }
    }
}

/// Build the 3D-space El for a cached scene, sized to a `px` square so it lays
/// out like the 2D charts.
pub fn space_chart(scene: &Space3dScene, px: f32) -> El {
    if !scene.has_data {
        return column([
            text("No scored samples to embed.").muted(),
            text("This trial has no ground truth (rejected, or a description this build can't map), so there is no L*a*b* frame to place samples in.")
                .muted()
                .font_size(12.0)
                .wrap_text(),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fixed(px))
        .height(Size::Fixed(px));
    }

    // Size the reference grid to the Lab scale: lightness runs 0..100, a*/b*
    // span roughly ±100 for wide content. A dark viewport makes the colour
    // swatches read like a colour picker rather than washing out over the UI.
    let style = SceneStyle {
        grid: GridSettings {
            planes: GridPlanes::XZ,
            spacing: 20.0,
            extent: 100.0,
            subdivisions: 1,
            ..Default::default()
        },
        background: Some(Color::srgb_u8(26, 28, 34)),
        ..Default::default()
    };

    let spec = SceneSpec::new()
        .points_labeled(
            scene.points.clone(),
            PointStyle {
                size: 8.0,
                shape: PointShape::Circle,
                size_mode: SizeMode::ScreenSpace,
            },
            scene.point_labels.clone(),
        )
        .lines(scene.gamut.clone())
        .add_lines(aetna_core::scene::LineDraw {
            geometry: scene.vectors.clone(),
            transform: aetna_core::scene::glam::Mat4::IDENTITY,
            style: LineStyle {
                width: 2.0,
                ..Default::default()
            },
        })
        .style(style)
        // X = a* (green→red), Y = L* (dark→light, up), Z = b* (blue→yellow).
        .axis_titles("a*", "L*", "b*");

    // No `.key(...)`: a keyed node is an interactive hit-test target, which
    // makes the press land *on* the scene and skips aetna's camera-drag capture
    // (orbit/pan never begins — only wheel-zoom, which routes separately). The
    // scene sits at a stable spot in the tree, so its structural node id keys
    // the camera state across frames and trial switches without an explicit key.
    chart3d(spec)
        .width(Size::Fixed(px))
        .height(Size::Fixed(px))
}

/// The legend/detail card for the 3D view.
pub fn space_legend() -> El {
    titled_card(
        "CIELAB sample space",
        [
            legend_row("dots", "measured samples, in their own colour"),
            legend_row("lines", "expected → measured (length = ΔE*ab)"),
            legend_row("cage", "ideal gamut of the trial's colour space"),
            text("Drag to orbit · shift-drag to pan · wheel to zoom · hover a dot for ΔE.")
                .muted()
                .font_size(12.0)
                .wrap_text(),
        ],
    )
}

fn legend_row(head: &str, body: &str) -> El {
    row([
        text(head).font_size(12.0).width(Size::Fixed(44.0)),
        text(body).muted().font_size(12.0).wrap_text(),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Start)
}

// ── geometry helpers ─────────────────────────────────────────────────────────

/// Faint, semi-transparent neutral for the error vectors.
const VECTOR_COLOR: [f32; 4] = [0.92, 0.93, 0.97, 0.55];
/// Even fainter for the gamut cage so it reads as a reference, not data.
const GAMUT_COLOR: [f32; 4] = [0.70, 0.74, 0.84, 0.32];
/// Subdivisions per gamut-cube edge (edges curve in Lab).
const GAMUT_SEGMENTS: usize = 16;

/// Lab `[L*, a*, b*]` → scene world: **X = a\*, Y = L\* (up), Z = b\***. The
/// a\*/b\* plane is the floor; lightness rises. Distances are preserved, so
/// world distance ≈ ΔE\*ab.
fn lab_to_world(lab: [f64; 3]) -> Vec3 {
    Vec3::new(lab[1] as f32, lab[0] as f32, lab[2] as f32)
}

/// A measured XYZ as an authoring-space sRGBA swatch: normalise to the
/// reference white, matrix into linear sRGB, clamp to gamut, sRGB-encode.
/// Out-of-sRGB content clips but keeps its hue — enough to read the dot.
fn display_color(xyz: [f64; 3], white_y: f64) -> [f32; 4] {
    let s = if white_y > 0.0 { 1.0 / white_y } else { 1.0 };
    let xyz_n = [xyz[0] * s, xyz[1] * s, xyz[2] * s];
    let lin = mat3_mul_vec(&ColorSpace::SRGB.xyz_to_rgb(), &xyz_n);
    let enc = |c: f64| transfer::srgb_oetf(c.clamp(0.0, 1.0)) as f32;
    [enc(lin[0]), enc(lin[1]), enc(lin[2]), 1.0]
}

/// Short hover label for a measured sample.
fn point_label(s: &AnalyzedSample, mlab: [f64; 3]) -> String {
    match s.delta_e {
        Some(de) => format!("ΔE {de:.1}  ·  L* {:.0}", mlab[0]),
        None => format!("L* {:.0}", mlab[0]),
    }
}

/// One corner of the colour space's RGB cube, mapped into the trial's Lab
/// frame. Linear RGB → XYZ (white at `Y = 1`) → scaled to the reference white's
/// luminance → L\*a\*b\* against that white — the exact transform an *expected*
/// sample of this code value would take, so the cage is the expected-locus
/// boundary.
fn lab_corner(m: &[[f64; 3]; 3], white_xyz: [f64; 3], rgb: [f64; 3]) -> Vec3 {
    let xyz = mat3_mul_vec(m, &rgb);
    let wy = white_xyz[1];
    let xyz_abs = [xyz[0] * wy, xyz[1] * wy, xyz[2] * wy];
    lab_to_world(metrics::xyz_to_lab(xyz_abs, white_xyz))
}

/// The 12 edges of the colour space's RGB cube, each subdivided and mapped into
/// the Lab frame (so curved edges read as curves, not chords).
fn gamut_wireframe(space: &ColorSpace, white_xyz: [f64; 3]) -> Vec<LineSegment> {
    let m = space.rgb_to_xyz();
    // The 8 cube corners. Two corners share an edge iff they differ in exactly
    // one channel — the nested loop below picks out the 12 such pairs.
    const CORNERS: [[f64; 3]; 8] = [
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [1.0, 1.0, 0.0],
        [1.0, 0.0, 1.0],
        [0.0, 1.0, 1.0],
        [1.0, 1.0, 1.0],
    ];
    let mut out = Vec::new();
    for (i, &a) in CORNERS.iter().enumerate() {
        for &b in CORNERS.iter().skip(i + 1) {
            let differing = (0..3).filter(|&k| (a[k] - b[k]).abs() > 0.5).count();
            if differing != 1 {
                continue;
            }
            let mut prev = lab_corner(&m, white_xyz, a);
            for step in 1..=GAMUT_SEGMENTS {
                let t = step as f64 / GAMUT_SEGMENTS as f64;
                let rgb = [
                    a[0] + (b[0] - a[0]) * t,
                    a[1] + (b[1] - a[1]) * t,
                    a[2] + (b[2] - a[2]) * t,
                ];
                let cur = lab_corner(&m, white_xyz, rgb);
                out.push(LineSegment {
                    start: prev,
                    end: cur,
                    color: GAMUT_COLOR,
                });
                prev = cur;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tristim_analyze::{AnalyzedSample, AnalyzedTrial, GroundTruth, GroundTruthSource};

    fn sample(
        measured_xyz: [f64; 3],
        measured_lab: [f64; 3],
        expected_lab: [f64; 3],
        de: f64,
    ) -> AnalyzedSample {
        AnalyzedSample {
            requested: [0.0; 3],
            measured_xyz,
            measured_xy: None,
            expected_xy: None,
            measured_lab: Some(measured_lab),
            expected_lab: Some(expected_lab),
            delta_uv: None,
            delta_e: Some(de),
            luminance: None,
        }
    }

    /// A scored sRGB trial builds finite, in-range geometry: one dot per
    /// sample (coloured in [0,1] sRGB), one error vector per scorable sample,
    /// and a 12-edge gamut cage. White lands at L*≈100 on the up axis.
    #[test]
    fn builds_finite_geometry() {
        let white_xyz = ColorSpace::SRGB.white_xyz();
        let trial = AnalyzedTrial {
            pixel_format: "xrgb8888".into(),
            ground_truth: GroundTruth::Known {
                space: ColorSpace::SRGB,
                transfer: "srgb".into(),
                absolute: false,
                source: GroundTruthSource::Negotiated,
            },
            samples: vec![
                sample([0.95, 1.0, 1.09], [100.0, 0.0, 0.0], [100.0, 0.0, 0.0], 0.0),
                sample([0.4, 0.2, 0.02], [51.0, 60.0, 40.0], [54.0, 80.0, 67.0], 25.0),
            ],
            summary: None,
            reference_white_xyz: Some(white_xyz),
        };

        let scene = Space3dScene::build(&trial, 3);
        assert_eq!(scene.trial, 3);
        assert!(scene.has_data);

        let (pts, _) = scene.points.snapshot();
        assert_eq!(pts.points.len(), 2);
        for p in &pts.points {
            assert!(p.position.is_finite(), "point position must be finite");
            assert!(
                p.color.iter().all(|c| c.is_finite() && (0.0..=1.0).contains(c)),
                "swatch must be in-gamut sRGB: {:?}",
                p.color
            );
        }
        // White: L* = 100 maps to world Y = 100, a*/b* = 0 → world X/Z = 0.
        assert!((pts.points[0].position.y - 100.0).abs() < 1e-3);
        assert!(pts.points[0].position.x.abs() < 1e-3);
        assert!(pts.points[0].position.z.abs() < 1e-3);

        let (vecs, _) = scene.vectors.snapshot();
        assert_eq!(vecs.segments.len(), 2);

        let (gamut, _) = scene.gamut.snapshot();
        assert_eq!(gamut.segments.len(), 12 * GAMUT_SEGMENTS);
        for s in &gamut.segments {
            assert!(s.start.is_finite() && s.end.is_finite());
        }
    }

    /// An unscored trial (no reference white / no Lab) yields an empty scene
    /// that the chart renders as a placeholder rather than panicking.
    #[test]
    fn unscored_trial_is_empty() {
        let trial = AnalyzedTrial {
            pixel_format: "xrgb8888".into(),
            ground_truth: GroundTruth::Unscored {
                reason: "rejected".into(),
            },
            samples: vec![AnalyzedSample {
                requested: [0.5; 3],
                measured_xyz: [0.2, 0.3, 0.1],
                measured_xy: None,
                expected_xy: None,
                measured_lab: None,
                expected_lab: None,
                delta_uv: None,
                delta_e: None,
                luminance: None,
            }],
            summary: None,
            reference_white_xyz: None,
        };
        let scene = Space3dScene::build(&trial, 0);
        assert!(!scene.has_data);
        assert_eq!(scene.points.snapshot().0.points.len(), 0);
        assert_eq!(scene.gamut.snapshot().0.segments.len(), 0);
    }
}
