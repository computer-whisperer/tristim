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
//! - **gamut cages** (lines) — the ideal RGB cube of a colour space, mapped
//!   corner-by-corner into the same Lab frame and subdivided (cube edges curve
//!   in Lab). It is the surface the measured points *would* sit on if the
//!   display reproduced that space perfectly, so drift off it is visible at a
//!   glance. The trial's negotiated space is always drawn (the *target*); the
//!   standard reference gamuts in [`REF_GAMUTS`] can be overlaid for comparison,
//!   each in its own colour with an in-plot name label.
//!
//! Geometry handles are built once per trial + reference set and cached on the
//! presenter (see [`Space3dScene`]); `build` clones them into a fresh
//! [`SceneSpec`] each frame, so the backend re-uploads nothing while the camera
//! moves.

use aetna_core::prelude::*;
use aetna_core::scene::glam::Vec3;
use aetna_core::scene::{
    AxisKind, GridPlanes, GridSettings, LabelPlacement, LineData, LineSegment, LineStyle,
    LinesHandle, PointData, PointLabels, PointShape, PointStyle, PointsHandle, ScenePoint,
    SceneSpec, SceneStyle, SizeMode,
};

use tristim_analyze::{AnalyzedSample, AnalyzedTrial, GroundTruth};
use tristim_capture::MeasuredGamut;
use tristim_color::{ColorSpace, mat3_mul_vec, metrics, transfer};

/// A standard colour space the 3D view can outline as a reference overlay,
/// alongside the trial's own (always-drawn) gamut.
#[derive(Clone, Copy)]
pub struct RefGamut {
    /// Toggle route suffix (`ref:<key>`) and stable identity.
    pub key: &'static str,
    /// Button / in-plot label text.
    pub name: &'static str,
    pub space: ColorSpace,
    /// Cage + label colour (authoring sRGBA).
    pub color: [f32; 4],
}

/// Number of reference-gamut overlays offered.
pub const N_REF_GAMUTS: usize = 3;

/// The reference gamuts, in nesting order (sRGB ⊂ Display P3 ⊂ Rec.2020) — the
/// same trio the chromaticity colour-field supports.
pub const REF_GAMUTS: [RefGamut; N_REF_GAMUTS] = [
    RefGamut {
        key: "srgb",
        name: "sRGB",
        space: ColorSpace::SRGB,
        color: [0.42, 0.60, 1.0, 0.5],
    },
    RefGamut {
        key: "p3",
        name: "Display P3",
        space: ColorSpace::DISPLAY_P3,
        color: [0.36, 0.85, 0.52, 0.5],
    },
    RefGamut {
        key: "bt2020",
        name: "Rec.2020",
        space: ColorSpace::BT2020,
        color: [1.0, 0.70, 0.30, 0.5],
    },
];

/// Which reference overlays are enabled, parallel to [`REF_GAMUTS`].
pub type RefSet = [bool; N_REF_GAMUTS];

/// Cached scene geometry for one analyzed trial. Cheap to clone into a
/// [`SceneSpec`] each frame (the handles are `Arc`s); rebuilt only when the
/// selected trial changes.
pub struct Space3dScene {
    /// Trial index these handles were built for — half of the cache key.
    pub trial: usize,
    /// Reference overlays these handles were built for — the other half.
    refs: RefSet,
    /// Measured samples, each at its `measured_lab`, coloured by measured colour.
    points: PointsHandle,
    /// Per-point hover labels (ΔE / L\*), aligned with `points`.
    point_labels: PointLabels,
    /// `expected → measured` displacement per scorable sample.
    vectors: LinesHandle,
    /// All gamut cages (target + enabled references + the measured shell when
    /// enabled), per-segment coloured.
    gamut: LinesHandle,
    /// Whether the measured-gamut shell overlay is included — half the cache key.
    show_measured: bool,
    /// One label-anchor point per cage, at its green primary.
    gamut_label_geo: PointsHandle,
    /// Persistent cage name labels, aligned with `gamut_label_geo`.
    gamut_labels: PointLabels,
    /// Whether any sample could be embedded (false ⇒ show a placeholder note).
    has_data: bool,
}

impl Space3dScene {
    /// Whether these cached handles still match the requested view.
    pub fn matches(&self, trial: usize, refs: RefSet, show_measured: bool) -> bool {
        self.trial == trial && self.refs == refs && self.show_measured == show_measured
    }

    /// Build the cached geometry for `trial` (index `idx`) with the given
    /// reference-gamut overlays enabled. When `show_measured` and `measured` is
    /// present, the probed gamut shell is overlaid in the same Lab frame.
    pub fn build(
        trial: &AnalyzedTrial,
        idx: usize,
        refs: RefSet,
        measured: Option<&MeasuredGamut>,
        show_measured: bool,
    ) -> Self {
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

        // Cages: the trial's negotiated space (always, as the target) plus each
        // enabled reference overlay — all in the shared measured-white Lab
        // frame, so they're directly comparable to the sample cloud.
        let mut cage = Vec::new();
        let mut anchor_pts = Vec::new();
        let mut anchor_txt = Vec::new();
        if let Some(white_xyz) = white {
            let target = match &trial.ground_truth {
                GroundTruth::Known { space, .. } => Some(*space),
                _ => None,
            };
            if let Some(space) = &target {
                add_cage(
                    &mut cage,
                    &mut anchor_pts,
                    &mut anchor_txt,
                    space,
                    white_xyz,
                    TARGET_CAGE_COLOR,
                    format!("{} · target", space_name(space)),
                );
            }
            for (on, g) in refs.iter().zip(REF_GAMUTS.iter()) {
                // Skip a reference that coincides with the target (already drawn).
                let dup = target.is_some_and(|t| t == g.space);
                if *on && !dup {
                    add_cage(
                        &mut cage,
                        &mut anchor_pts,
                        &mut anchor_txt,
                        &g.space,
                        white_xyz,
                        g.color,
                        g.name.to_string(),
                    );
                }
            }

            // The measured gamut shell: the *actual* probed boundary, drawn in
            // the same Lab frame so drift from the ideal cages reads directly.
            if let Some(g) = measured.filter(|_| show_measured) {
                cage.extend(measured_cage(g, white_xyz));
                if let Some(green) = g.vertices.iter().find(|v| v.code_value == [0.0, 1.0, 0.0]) {
                    anchor_pts.push(ScenePoint {
                        position: lab_to_world(metrics::xyz_to_lab(green.xyz, white_xyz)),
                        color: MEASURED_CAGE_COLOR,
                    });
                    anchor_txt.push("measured".to_string());
                }
            }
        }

        let has_data = !points.is_empty();
        Self {
            trial: idx,
            refs,
            points: PointsHandle::new(PointData { points }),
            point_labels: PointLabels::new(labels).on_hover(),
            vectors: LinesHandle::new(LineData { segments: vectors }),
            gamut: LinesHandle::new(LineData { segments: cage }),
            show_measured,
            gamut_label_geo: PointsHandle::new(PointData { points: anchor_pts }),
            gamut_labels: PointLabels::new(anchor_txt)
                .always()
                .placement(LabelPlacement::Above),
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
    // span roughly ±100 for wide content. No scene background — the scene
    // composites over the enclosing card's panel surface (a dark surface in
    // the default theme), so the swatches read against the same surface as
    // the rest of the UI.
    let style = SceneStyle {
        grid: GridSettings {
            planes: GridPlanes::XZ,
            spacing: 20.0,
            extent: 100.0,
            subdivisions: 1,
            ..Default::default()
        },
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
        .lines_styled(
            scene.vectors.clone(),
            LineStyle {
                width: 2.0,
                ..Default::default()
            },
        )
        // A small square marker + persistent name at each cage's green primary,
        // so every outlined gamut is labelled where it's most distinctive.
        .points_labeled(
            scene.gamut_label_geo.clone(),
            PointStyle {
                size: 5.0,
                shape: PointShape::Square,
                size_mode: SizeMode::ScreenSpace,
            },
            scene.gamut_labels.clone(),
        )
        .style(style)
        // X = a* (green→red), Y = L* (dark→light, up), Z = b* (blue→yellow).
        .axis_titles("a*", "L*", "b*")
        // Lightness is one-sided: clip the L* axis to [0, 100] so it doesn't
        // dive into meaningless negative space below the a*/b* floor. a*/b*
        // stay bipolar (the symmetric default).
        .axis_bounds(AxisKind::Y, 0.0, 100.0);

    // No `.key(...)`: the scene sits at a stable spot in the tree, so its
    // structural node id keys the camera state (and the hover pick) across
    // frames and trial switches — an explicit key buys nothing here.
    chart3d(spec).width(Size::Fixed(px)).height(Size::Fixed(px))
}

/// The legend/detail card for the 3D view.
pub fn space_legend() -> El {
    titled_card(
        "CIELAB sample space",
        [
            legend_row("dots", "measured samples, in their own colour"),
            legend_row("lines", "expected → measured (length = ΔE*ab)"),
            legend_row(
                "cages",
                "gamut bounds — the trial's space (· target) plus any reference gamuts enabled above",
            ),
            legend_row(
                "measured",
                "the probed gamut shell, when enabled (hot edges = clamped/folded regions)",
            ),
            text("Drag to orbit · shift-drag to pan · wheel to zoom · hover a dot for ΔE.")
                .muted()
                .font_size(12.0)
                .wrap_text(),
        ],
    )
}

fn legend_row(head: &str, body: &str) -> El {
    row([
        // Wide enough for the longest head ("measured").
        text(head).font_size(12.0).width(Size::Fixed(68.0)),
        // Fill the remaining row width so a long description wraps within the
        // card instead of overflowing it.
        text(body)
            .muted()
            .font_size(12.0)
            .wrap_text()
            .width(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Start)
    .width(Size::Fill(1.0))
}

// ── geometry helpers ─────────────────────────────────────────────────────────

/// Faint, semi-transparent neutral for the error vectors.
const VECTOR_COLOR: [f32; 4] = [0.92, 0.93, 0.97, 0.55];
/// The trial's own (target) gamut cage: a bright neutral, brighter than the
/// coloured reference overlays so the target reads as primary.
const TARGET_CAGE_COLOR: [f32; 4] = [0.86, 0.89, 0.96, 0.55];
/// The measured gamut shell (probed cube-surface wireframe): a distinct magenta
/// so it reads against both the neutral target cage and the coloured references.
const MEASURED_CAGE_COLOR: [f32; 4] = [0.93, 0.45, 0.85, 0.7];
/// Clamped (folded) patches of the measured shell — where pushing the code value
/// stopped moving the measurement; drawn hotter to flag the boundary it hit.
const MEASURED_FOLD_COLOR: [f32; 4] = [1.0, 0.4, 0.25, 0.9];
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

/// Short display name for a colour space (for the in-plot cage label).
fn space_name(s: &ColorSpace) -> &'static str {
    if *s == ColorSpace::SRGB {
        "sRGB"
    } else if *s == ColorSpace::DISPLAY_P3 {
        "Display P3"
    } else if *s == ColorSpace::DCI_P3 {
        "DCI-P3"
    } else if *s == ColorSpace::BT2020 {
        "Rec.2020"
    } else if *s == ColorSpace::ADOBE_RGB {
        "Adobe RGB"
    } else {
        "trial gamut"
    }
}

/// Append one gamut cage (subdivided RGB-cube wireframe) plus a labelled anchor
/// point at its green primary — the vertex that differs most between gamuts, so
/// labels don't pile up at the shared white.
fn add_cage(
    segs: &mut Vec<LineSegment>,
    anchor_pts: &mut Vec<ScenePoint>,
    anchor_txt: &mut Vec<String>,
    space: &ColorSpace,
    white_xyz: [f64; 3],
    color: [f32; 4],
    name: String,
) {
    segs.extend(gamut_wireframe(space, white_xyz, color));
    let m = space.rgb_to_xyz();
    anchor_pts.push(ScenePoint {
        position: lab_corner(&m, white_xyz, [0.0, 1.0, 0.0]),
        color,
    });
    anchor_txt.push(name);
}

/// The measured gamut shell: each refined leaf patch drawn as a quad outline in
/// the trial's Lab frame, with folded (clamped) patches flagged hotter. This is
/// the *actual* probed boundary surface, to read against the ideal cages.
fn measured_cage(gamut: &MeasuredGamut, white_xyz: [f64; 3]) -> Vec<LineSegment> {
    let world: Vec<Vec3> = gamut
        .vertices
        .iter()
        .map(|v| lab_to_world(metrics::xyz_to_lab(v.xyz, white_xyz)))
        .collect();
    let mut out = Vec::new();
    for p in &gamut.patches {
        let color = if p.status == "folded" {
            MEASURED_FOLD_COLOR
        } else {
            MEASURED_CAGE_COLOR
        };
        // The quad's 4 edges (corners are stored CCW).
        for k in 0..4 {
            let (Some(&start), Some(&end)) =
                (world.get(p.corners[k]), world.get(p.corners[(k + 1) % 4]))
            else {
                continue;
            };
            out.push(LineSegment { start, end, color });
        }
    }
    out
}

/// The 12 edges of the colour space's RGB cube, each subdivided and mapped into
/// the Lab frame (so curved edges read as curves, not chords), in `color`.
fn gamut_wireframe(space: &ColorSpace, white_xyz: [f64; 3], color: [f32; 4]) -> Vec<LineSegment> {
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
                    color,
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
            expected_xyz: None,
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
                sample(
                    [0.4, 0.2, 0.02],
                    [51.0, 60.0, 40.0],
                    [54.0, 80.0, 67.0],
                    25.0,
                ),
            ],
            summary: None,
            reference_white_xyz: Some(white_xyz),
        };

        let scene = Space3dScene::build(&trial, 3, [false; N_REF_GAMUTS], None, false);
        assert_eq!(scene.trial, 3);
        assert!(scene.matches(3, [false; N_REF_GAMUTS], false));
        assert!(!scene.matches(3, [true, false, false], false));
        assert!(!scene.matches(3, [false; N_REF_GAMUTS], true));
        assert!(scene.has_data);

        let (pts, _) = scene.points.snapshot();
        assert_eq!(pts.points.len(), 2);
        for p in &pts.points {
            assert!(p.position.is_finite(), "point position must be finite");
            assert!(
                p.color
                    .iter()
                    .all(|c| c.is_finite() && (0.0..=1.0).contains(c)),
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

        // One cage (the trial's sRGB target), with a single labelled anchor.
        let (gamut, _) = scene.gamut.snapshot();
        assert_eq!(gamut.segments.len(), 12 * GAMUT_SEGMENTS);
        for s in &gamut.segments {
            assert!(s.start.is_finite() && s.end.is_finite());
        }
        assert_eq!(scene.gamut_label_geo.snapshot().0.points.len(), 1);
        assert_eq!(scene.gamut_labels.get(0), Some("sRGB · target"));
    }

    /// Enabling a non-target reference adds a second cage + label; enabling the
    /// reference that *is* the target is deduplicated (no second cage).
    #[test]
    fn reference_overlays_add_and_dedup_cages() {
        let white_xyz = ColorSpace::SRGB.white_xyz();
        let trial = AnalyzedTrial {
            pixel_format: "xrgb8888".into(),
            ground_truth: GroundTruth::Known {
                space: ColorSpace::SRGB, // target = sRGB (REF_GAMUTS[0])
                transfer: "srgb".into(),
                absolute: false,
                source: GroundTruthSource::Negotiated,
            },
            samples: vec![sample(
                [0.4, 0.2, 0.02],
                [51.0, 60.0, 40.0],
                [54.0, 80.0, 67.0],
                25.0,
            )],
            summary: None,
            reference_white_xyz: Some(white_xyz),
        };

        // Display P3 overlay on (index 1): target + P3 = two cages, two labels.
        let p3 = Space3dScene::build(&trial, 0, [false, true, false], None, false);
        assert_eq!(
            p3.gamut.snapshot().0.segments.len(),
            2 * 12 * GAMUT_SEGMENTS
        );
        assert_eq!(p3.gamut_label_geo.snapshot().0.points.len(), 2);

        // sRGB overlay on (index 0) == target: deduped to one cage.
        let srgb = Space3dScene::build(&trial, 0, [true, false, false], None, false);
        assert_eq!(srgb.gamut.snapshot().0.segments.len(), 12 * GAMUT_SEGMENTS);
        assert_eq!(srgb.gamut_label_geo.snapshot().0.points.len(), 1);
    }

    /// The measured-shell overlay adds patch-outline segments + a "measured"
    /// label only when enabled, and flags folded patches in the hot colour.
    #[test]
    fn measured_shell_overlay_adds_when_enabled() {
        use tristim_capture::{GamutPatch, GamutVertex, MeasuredGamut};
        let white_xyz = ColorSpace::SRGB.white_xyz();
        let trial = AnalyzedTrial {
            pixel_format: "xrgb8888".into(),
            ground_truth: GroundTruth::Known {
                space: ColorSpace::SRGB,
                transfer: "srgb".into(),
                absolute: false,
                source: GroundTruthSource::Negotiated,
            },
            samples: vec![sample(
                [0.95, 1.0, 1.09],
                [100.0, 0.0, 0.0],
                [100.0, 0.0, 0.0],
                0.0,
            )],
            summary: None,
            reference_white_xyz: Some(white_xyz),
        };
        let v = |cv: [f64; 3], xyz: [f64; 3]| GamutVertex {
            code_value: cv,
            xyz,
            lab: [0.0; 3],
            trustworthy: true,
        };
        let gamut = MeasuredGamut {
            white_xyz,
            vertices: vec![
                v([0.0, 0.0, 0.0], [0.1, 0.1, 0.1]),
                v([1.0, 0.0, 0.0], [40.0, 20.0, 2.0]),
                v([0.0, 1.0, 0.0], [35.0, 70.0, 12.0]),
                v([0.0, 0.0, 1.0], [18.0, 7.0, 95.0]),
            ],
            patches: vec![GamutPatch {
                face: "R=1".into(),
                corners: [0, 1, 2, 3],
                status: "folded".into(),
            }],
        };

        // Off: the gamut is present but not drawn — target cage + its one label.
        let off = Space3dScene::build(&trial, 0, [false; N_REF_GAMUTS], Some(&gamut), false);
        let off_segs = off.gamut.snapshot().0.segments.len();
        assert_eq!(off.gamut_label_geo.snapshot().0.points.len(), 1);

        // On: +4 patch-edge segments and a "measured" label anchor.
        let on = Space3dScene::build(&trial, 0, [false; N_REF_GAMUTS], Some(&gamut), true);
        let (segs, _) = on.gamut.snapshot();
        assert_eq!(segs.segments.len(), off_segs + 4);
        assert_eq!(on.gamut_label_geo.snapshot().0.points.len(), 2);
        assert_eq!(on.gamut_labels.get(1), Some("measured"));
        // The folded patch is drawn in the hot fold colour.
        assert!(segs.segments.iter().any(|s| s.color == MEASURED_FOLD_COLOR));
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
                expected_xyz: None,
                measured_lab: None,
                expected_lab: None,
                delta_uv: None,
                delta_e: None,
                luminance: None,
            }],
            summary: None,
            reference_white_xyz: None,
        };
        let scene = Space3dScene::build(&trial, 0, [true, true, true], None, false);
        assert!(!scene.has_data);
        assert_eq!(scene.points.snapshot().0.points.len(), 0);
        // No reference white ⇒ no Lab frame ⇒ no cages even with overlays on.
        assert_eq!(scene.gamut.snapshot().0.segments.len(), 0);
        assert_eq!(scene.gamut_label_geo.snapshot().0.points.len(), 0);
    }
}
