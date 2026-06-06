//! The **3D sample space**: a [`chart3d`] view that embeds a trial's samples in
//! a chosen colour space and shows, in one picture, where the compositor
//! *should* have put each colour versus where it landed.
//!
//! Every mark starts from **absolute CIE XYZ** (cd/m²) — measured samples carry
//! it directly, expected values are derived in the same absolute frame, and the
//! ideal-gamut cages are synthesised from primaries scaled to a white
//! luminance. A [`Space3dView`] then projects that absolute XYZ into world
//! coordinates, so the same geometry can be shown four ways:
//!
//! - **Lab (relative)** — CIE L\*a\*b\* against the trial's *measured* white, the
//!   frame the ΔE\*ab scores live in. Each error vector's *length* is the
//!   reported ΔE\*ab and its *direction* the kind of error. White sits at
//!   L\*=100 regardless of how bright it actually was.
//! - **Lab (absolute)** — the same Lab geometry but against a *fixed* 203 cd/m²
//!   white, so absolute brightness survives: a 1000-nit white rises well above
//!   L\*=100 instead of collapsing onto it.
//! - **xyY (nits)** — CIE xy chromaticity as the floor, absolute luminance
//!   (PQ-encoded so the SDR-to-HDR decade is legible) as height.
//! - **ICtCp** — the BT.2100 absolute-luminance perceptual space: PQ intensity
//!   up, opponent chroma on the floor. Purpose-built for sRGB-vs-BT.2020.
//!
//! The three marks:
//!
//! - **points** — one per measured sample, painted in its own (sRGB-clamped)
//!   colour so the cloud reads like the picture it is.
//! - **error vectors** (lines) — `expected → measured` per scorable sample.
//! - **gamut cages** (lines) — the ideal RGB cube of a colour space, mapped
//!   corner-by-corner and subdivided (cube edges curve). The trial's negotiated
//!   space is always drawn (the *target*, at the measured white). The standard
//!   reference gamuts in [`REF_GAMUTS`] can be overlaid in two flavours
//!   ([`RefCages`]), independent of the projection: *absolute* (each at its spec
//!   reference white) and *relative* (scaled to this trial's white). Enabling
//!   both of a gamut shows the absolute-vs-relative volume difference directly;
//!   each cage's label carries the peak white luminance it was scaled to.
//!
//! Geometry handles are built once per trial + view + reference set and cached
//! on the presenter (see [`Space3dScene`]); `build` clones them into a fresh
//! [`SceneSpec`] each frame, so the backend re-uploads nothing while the camera
//! moves.

use damascene_core::prelude::*;
use damascene_core::scene::glam::Vec3;
use damascene_core::scene::{
    AxisKind, GridPlanes, GridSettings, LabelPlacement, LineData, LineSegment, LineStyle,
    LinesHandle, PointData, PointLabels, PointShape, PointStyle, PointsHandle, ScenePoint,
    SceneSpec, SceneStyle, SizeMode,
};

use tristim_analyze::{AnalyzedSample, AnalyzedTrial, GroundTruth};
use tristim_capture::MeasuredGamut;
use tristim_color::{
    ColorSpace, chromaticity_to_xyz, ictcp, mat3_mul_vec, metrics, transfer, white,
    xyz_to_chromaticity,
};

/// The colour space a [`Space3dScene`] embeds samples in. The first three reuse
/// CIE-derived geometry; only the final projection of an absolute XYZ into world
/// coordinates differs between them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Space3dView {
    /// CIE L\*a\*b\* against the trial's measured white (the ΔE\*ab frame). Default.
    #[default]
    LabRelative,
    /// CIE L\*a\*b\* against a fixed [`ABS_LAB_REF_NITS`] white (absolute lightness).
    LabAbsolute,
    /// CIE xy on the floor, absolute luminance (PQ-encoded) as height.
    XyYNits,
    /// ITU-R BT.2100 ICtCp: PQ intensity up, opponent chroma on the floor.
    ICtCp,
}

/// The reference white for the absolute-Lab view (BT.2408 HDR/SDR diffuse
/// white). Content at this luminance maps to L\*=100; brighter content exceeds it.
pub const ABS_LAB_REF_NITS: f64 = 203.0;

/// CIE xy (offset from D65) → world units, so the chromaticity floor spans a
/// range comparable to the luminance height.
const XYY_CHROMA_SCALE: f64 = 200.0;
/// PQ-encoded luminance (`0..=1`) → world height.
const NITS_PQ_HEIGHT: f64 = 100.0;
/// ICtCp intensity (`0..=1`) → world height.
const ICTCP_I_SCALE: f64 = 100.0;
/// ICtCp opponent chroma (`±0.5`-ish) → world floor units.
const ICTCP_CHROMA_SCALE: f64 = 200.0;

impl Space3dView {
    /// Project an absolute CIE XYZ (cd/m²) into scene world coordinates.
    /// `trial_white` is the trial's measured-white XYZ, used only by the
    /// relative-Lab view.
    fn world(self, xyz: [f64; 3], trial_white: [f64; 3]) -> Vec3 {
        match self {
            Space3dView::LabRelative => lab_to_world(metrics::xyz_to_lab(xyz, trial_white)),
            Space3dView::LabAbsolute => lab_to_world(metrics::xyz_to_lab(xyz, abs_lab_white())),
            Space3dView::XyYNits => {
                let c = xyz_to_chromaticity(xyz).unwrap_or(white::D65);
                let h = transfer::pq_oetf(xyz[1].max(0.0)) * NITS_PQ_HEIGHT;
                Vec3::new(
                    ((c[0] - white::D65[0]) * XYY_CHROMA_SCALE) as f32,
                    h as f32,
                    ((c[1] - white::D65[1]) * XYY_CHROMA_SCALE) as f32,
                )
            }
            Space3dView::ICtCp => {
                let [i, ct, cp] = ictcp::xyz_to_ictcp(xyz);
                Vec3::new(
                    (ct * ICTCP_CHROMA_SCALE) as f32,
                    (i * ICTCP_I_SCALE) as f32,
                    (cp * ICTCP_CHROMA_SCALE) as f32,
                )
            }
        }
    }

    /// `(x, y-up, z)` axis titles for this view.
    fn axis_titles(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Space3dView::LabRelative => ("a*", "L*", "b*"),
            Space3dView::LabAbsolute => ("a*", "L* (abs)", "b*"),
            Space3dView::XyYNits => ("x", "Y · cd/m² (PQ)", "y"),
            Space3dView::ICtCp => ("Ct", "I (PQ)", "Cp"),
        }
    }

    /// Fixed bounds for the up axis, or `None` to auto-fit. The absolute-Lab
    /// view auto-fits because a high-nit cage (e.g. BT.2020 at 10000 cd/m²)
    /// drives L\* well past 100; the others are bounded `[0, 100]`.
    fn y_bounds(self) -> Option<(f32, f32)> {
        match self {
            Space3dView::LabAbsolute => None,
            _ => Some((0.0, 100.0)),
        }
    }

    /// Title for the legend card.
    fn legend_title(self) -> &'static str {
        match self {
            Space3dView::LabRelative => "CIELAB space (relative white)",
            Space3dView::LabAbsolute => "CIELAB space (absolute white)",
            Space3dView::XyYNits => "xyY space (absolute nits)",
            Space3dView::ICtCp => "ICtCp space (BT.2100)",
        }
    }

    /// One-line description of what the height axis means, for the legend.
    fn height_hint(self) -> &'static str {
        match self {
            Space3dView::LabRelative => "height = L* (0–100, against the measured white)",
            Space3dView::LabAbsolute => {
                "height = L* against a fixed 203 cd/m² white — brighter content rises past 100"
            }
            Space3dView::XyYNits => {
                "height = luminance, cd/m² (PQ-encoded); floor = CIE xy around D65"
            }
            Space3dView::ICtCp => "height = PQ intensity (absolute); floor = Ct/Cp opponent chroma",
        }
    }

    /// Short, view-appropriate per-sample hover label.
    fn point_label(self, s: &AnalyzedSample) -> String {
        let lead = match s.delta_e {
            Some(de) => format!("ΔE {de:.1}"),
            None => String::new(),
        };
        let tail = match self {
            Space3dView::LabRelative | Space3dView::LabAbsolute => {
                s.measured_lab.map(|l| format!("L* {:.0}", l[0]))
            }
            Space3dView::XyYNits | Space3dView::ICtCp => {
                Some(format!("{:.0} cd/m²", s.measured_xyz[1]))
            }
        };
        match (lead.is_empty(), tail) {
            (false, Some(t)) => format!("{lead}  ·  {t}"),
            (false, None) => lead,
            (true, Some(t)) => t,
            (true, None) => String::new(),
        }
    }
}

/// The fixed reference-white XYZ for [`Space3dView::LabAbsolute`]: D65 at
/// [`ABS_LAB_REF_NITS`] cd/m².
fn abs_lab_white() -> [f64; 3] {
    let w = chromaticity_to_xyz(white::D65); // Y = 1
    [
        w[0] * ABS_LAB_REF_NITS,
        ABS_LAB_REF_NITS,
        w[2] * ABS_LAB_REF_NITS,
    ]
}

/// A standard colour space the 3D view can outline as a reference overlay,
/// alongside the trial's own (always-drawn) gamut.
#[derive(Clone, Copy)]
pub struct RefGamut {
    /// Toggle route suffix (`ref:<key>`) and stable identity.
    pub key: &'static str,
    /// In-plot label text.
    pub name: &'static str,
    /// Compact toggle-button label — the overlay row packs six of these (plus
    /// captions and the shell toggle), so the full names don't fit a
    /// half-width window.
    pub short: &'static str,
    pub space: ColorSpace,
    /// Reference white luminance (cd/m²) the cage is anchored at in the absolute
    /// views — a gamut alone fixes no brightness, so this stands in for its
    /// typical mastering: SDR for sRGB/P3, the PQ container max for BT.2020.
    pub ref_white_nits: f64,
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
        short: "sRGB",
        space: ColorSpace::SRGB,
        ref_white_nits: 80.0,
        color: [0.42, 0.60, 1.0, 0.5],
    },
    RefGamut {
        key: "p3",
        name: "Display P3",
        short: "P3",
        space: ColorSpace::DISPLAY_P3,
        ref_white_nits: 100.0,
        color: [0.36, 0.85, 0.52, 0.5],
    },
    RefGamut {
        key: "bt2020",
        name: "Rec.2020",
        short: "2020",
        space: ColorSpace::BT2020,
        ref_white_nits: 10_000.0,
        color: [1.0, 0.70, 0.30, 0.5],
    },
];

/// Which reference-gamut cages are enabled, split by anchor kind — independent
/// of the active projection, so a toggle means the same thing in every view.
/// Both arrays are parallel to [`REF_GAMUTS`].
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct RefCages {
    /// Absolute-anchored: each cage at its gamut's spec reference white
    /// ([`RefGamut::ref_white_nits`]).
    pub abs: [bool; N_REF_GAMUTS],
    /// Relative-anchored: each cage scaled to the trial's measured white.
    pub rel: [bool; N_REF_GAMUTS],
}

/// Cached scene geometry for one analyzed trial. Cheap to clone into a
/// [`SceneSpec`] each frame (the handles are `Arc`s); rebuilt only when the
/// selected trial, view, or overlays change.
pub struct Space3dScene {
    /// Trial index these handles were built for — part of the cache key.
    pub trial: usize,
    /// The projection these handles were built for — part of the cache key.
    view: Space3dView,
    /// Reference overlays these handles were built for — part of the cache key.
    refs: RefCages,
    /// Measured samples, each at its projected position, coloured by measured colour.
    points: PointsHandle,
    /// Per-point hover labels (ΔE / lightness or nits), aligned with `points`.
    point_labels: PointLabels,
    /// `expected → measured` displacement per scorable sample.
    vectors: LinesHandle,
    /// All gamut cages (target + enabled references + the measured shell when
    /// enabled), per-segment coloured.
    gamut: LinesHandle,
    /// Whether the measured-gamut shell overlay is included — part of the cache key.
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
    pub fn matches(
        &self,
        trial: usize,
        view: Space3dView,
        refs: RefCages,
        show_measured: bool,
    ) -> bool {
        self.trial == trial
            && self.view == view
            && self.refs == refs
            && self.show_measured == show_measured
    }

    /// Build the cached geometry for `trial` (index `idx`) in `view` with the
    /// given reference-gamut overlays enabled. When `show_measured` and
    /// `measured` is present, the probed gamut shell is overlaid.
    pub fn build(
        trial: &AnalyzedTrial,
        idx: usize,
        view: Space3dView,
        refs: RefCages,
        measured: Option<&MeasuredGamut>,
        show_measured: bool,
    ) -> Self {
        let mut points = Vec::new();
        let mut labels = Vec::new();
        let mut vectors = Vec::new();
        let mut cage = Vec::new();
        let mut anchor_pts = Vec::new();
        let mut anchor_txt = Vec::new();

        // A reference white (the brightest measured patch) is what makes a trial
        // scorable; without it there is no expected locus to draw. Every mark is
        // built from absolute XYZ and projected by `view`.
        if let Some(white_xyz) = trial.reference_white_xyz {
            let white_y = white_xyz[1];

            for s in &trial.samples {
                points.push(ScenePoint {
                    position: view.world(s.measured_xyz, white_xyz),
                    color: display_color(s.measured_xyz, white_y),
                });
                labels.push(view.point_label(s));
                if let Some(exyz) = s.expected_xyz {
                    vectors.push(LineSegment {
                        start: view.world(exyz, white_xyz),
                        end: view.world(s.measured_xyz, white_xyz),
                        color: VECTOR_COLOR,
                    });
                }
            }

            // Cages: the trial's negotiated space (always, as the target, at the
            // measured white) plus each enabled reference overlay (at its own
            // reference white in the absolute views).
            let target = match &trial.ground_truth {
                GroundTruth::Known { space, .. } => Some(*space),
                _ => None,
            };
            if let Some(space) = &target {
                add_cage(
                    &mut cage,
                    &mut anchor_pts,
                    &mut anchor_txt,
                    view,
                    space,
                    white_y,
                    white_xyz,
                    TARGET_CAGE_COLOR,
                    format!("{} · target", space_name(space)),
                );
            }
            // Absolute-anchored references: each at its gamut's spec reference
            // white. Drawn even when the gamut matches the target — at a
            // different white it's a distinct (spec vs measured) volume, which
            // is exactly the absolute-vs-relative comparison.
            for (on, g) in refs.abs.iter().zip(REF_GAMUTS.iter()) {
                if *on {
                    add_cage(
                        &mut cage,
                        &mut anchor_pts,
                        &mut anchor_txt,
                        view,
                        &g.space,
                        g.ref_white_nits,
                        white_xyz,
                        g.color,
                        g.name.to_string(),
                    );
                }
            }
            // Relative-anchored references: scaled to the trial's measured white.
            // The target *is* its gamut at that white, so a relative cage of the
            // target's gamut would duplicate it — skip. Dimmed so it reads as
            // secondary to the absolute cage of the same hue.
            for (on, g) in refs.rel.iter().zip(REF_GAMUTS.iter()) {
                let dup = target.is_some_and(|t| t == g.space);
                if *on && !dup {
                    add_cage(
                        &mut cage,
                        &mut anchor_pts,
                        &mut anchor_txt,
                        view,
                        &g.space,
                        white_y,
                        white_xyz,
                        rel_color(g.color),
                        g.name.to_string(),
                    );
                }
            }

            // The measured gamut shell: the *actual* probed boundary, projected
            // the same way so drift from the ideal cages reads directly.
            if let Some(g) = measured.filter(|_| show_measured) {
                cage.extend(measured_cage(view, g, white_xyz));
                if let Some(green) = g.vertices.iter().find(|v| v.code_value == [0.0, 1.0, 0.0]) {
                    anchor_pts.push(ScenePoint {
                        position: view.world(green.xyz, white_xyz),
                        color: MEASURED_CAGE_COLOR,
                    });
                    // The shell is the *actual* reproduction, not scaled — its
                    // peak is the brightest probed vertex (what the display hit).
                    let peak = g.vertices.iter().map(|v| v.xyz[1]).fold(0.0, f64::max);
                    anchor_txt.push(format!("measured · {}", nits_label(peak)));
                }
            }
        }

        let has_data = !points.is_empty();
        Self {
            trial: idx,
            view,
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

/// Build the 3D-space El for a cached scene. Fills whatever rect its parent
/// resolves — the plot card's overlay stack sizes it (square beside the stat
/// column, full content width when stacked; see
/// `PresenterApp::content_panel`).
pub fn space_chart(scene: &Space3dScene) -> El {
    if !scene.has_data {
        return column([
            text("No scored samples to embed.").muted(),
            text("This trial has no ground truth (rejected, or a description this build can't map), so there is no colour frame to place samples in.")
                .muted()
                .font_size(12.0)
                .wrap_text(),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0));
    }

    // A neutral reference floor/grid, sized to the world scale the projections
    // share (lightness/intensity run ~0..100, the floor axes span roughly
    // ±100). No scene background — the scene composites over the enclosing
    // card's panel surface, so the swatches read against the same surface as
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

    let (ax, ay, az) = scene.view.axis_titles();
    let mut spec = SceneSpec::new()
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
        .axis_titles(ax, ay, az);

    // Most views pin the up axis to a one-sided [0, 100] so it doesn't dive
    // into meaningless negative space; the absolute-Lab view auto-fits because
    // bright cages push L* past 100.
    if let Some((lo, hi)) = scene.view.y_bounds() {
        spec = spec.axis_bounds(AxisKind::Y, lo, hi);
    }

    // No `.key(...)`: the scene sits at a stable spot in the tree, so its
    // structural node id keys the camera state (and the hover pick) across
    // frames and trial/view switches — an explicit key buys nothing here.
    // `chart3d` is fill-sized by default.
    chart3d(spec)
}

/// The legend/detail card for the 3D view.
pub fn space_legend(view: Space3dView) -> El {
    titled_card(
        view.legend_title(),
        [
            legend_row("dots", "measured samples, in their own colour"),
            legend_row("lines", "expected → measured (length = ΔE*ab in Lab)"),
            legend_row(
                "cages",
                "gamut bounds — the trial's space (· target) plus reference gamuts (abs = spec white, rel = scaled to this trial), each labelled with its peak white luminance (cd/m²)",
            ),
            legend_row(
                "measured",
                "the probed gamut shell, when enabled (hot edges = clamped/folded regions)",
            ),
            text(view.height_hint()).muted().font_size(12.0).wrap_text(),
            text("Drag to orbit · shift-drag to pan · wheel to zoom · hover a dot for detail.")
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
/// Subdivisions per gamut-cube edge (edges curve in most projections).
const GAMUT_SEGMENTS: usize = 16;

/// Lab `[L*, a*, b*]` → scene world: **X = a\*, Y = L\* (up), Z = b\***. The
/// a\*/b\* plane is the floor; lightness rises. Distances are preserved, so in
/// the Lab views world distance ≈ ΔE\*ab.
fn lab_to_world(lab: [f64; 3]) -> Vec3 {
    Vec3::new(lab[1] as f32, lab[0] as f32, lab[2] as f32)
}

/// A measured XYZ as an authoring-space sRGBA swatch: normalise to the
/// reference white, matrix into linear sRGB, clamp to gamut, sRGB-encode.
/// Out-of-sRGB content clips but keeps its hue — enough to read the dot. The
/// swatch is view-independent (it's the colour the patch *is*, not where it sits).
fn display_color(xyz: [f64; 3], white_y: f64) -> [f32; 4] {
    let s = if white_y > 0.0 { 1.0 / white_y } else { 1.0 };
    let xyz_n = [xyz[0] * s, xyz[1] * s, xyz[2] * s];
    let lin = mat3_mul_vec(&ColorSpace::SRGB.xyz_to_rgb(), &xyz_n);
    let enc = |c: f64| transfer::srgb_oetf(c.clamp(0.0, 1.0)) as f32;
    [enc(lin[0]), enc(lin[1]), enc(lin[2]), 1.0]
}

/// One corner of the colour space's RGB cube, as absolute XYZ then projected.
/// Linear RGB → XYZ (white at `Y = 1`) → scaled to the cage's white luminance →
/// `view.world` — the exact transform an *expected* sample of this code value
/// would take, so the cage is the expected-locus boundary.
fn cage_corner(
    view: Space3dView,
    m: &[[f64; 3]; 3],
    cage_white_y: f64,
    trial_white: [f64; 3],
    rgb: [f64; 3],
) -> Vec3 {
    let xyz = mat3_mul_vec(m, &rgb);
    let xyz_abs = [
        xyz[0] * cage_white_y,
        xyz[1] * cage_white_y,
        xyz[2] * cage_white_y,
    ];
    view.world(xyz_abs, trial_white)
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
#[allow(clippy::too_many_arguments)]
fn add_cage(
    segs: &mut Vec<LineSegment>,
    anchor_pts: &mut Vec<ScenePoint>,
    anchor_txt: &mut Vec<String>,
    view: Space3dView,
    space: &ColorSpace,
    cage_white_y: f64,
    trial_white: [f64; 3],
    color: [f32; 4],
    name: String,
) {
    let m = space.rgb_to_xyz();
    segs.extend(gamut_wireframe(view, &m, cage_white_y, trial_white, color));
    anchor_pts.push(ScenePoint {
        position: cage_corner(view, &m, cage_white_y, trial_white, [0.0, 1.0, 0.0]),
        color,
    });
    // Tag the cage with its peak (white-corner) luminance — the value it was
    // scaled to. In the relative-Lab view every cage carries the same trial
    // white, signalling the normalisation; in the absolute views each reference
    // shows its own spec white, so a brighter gamut reads as brighter.
    anchor_txt.push(format!("{name} · {}", nits_label(cage_white_y)));
}

/// A cage's peak luminance as a compact label suffix (e.g. `203 cd/m²`).
fn nits_label(nits: f64) -> String {
    format!("{} cd/m²", nits.round() as i64)
}

/// Dim a reference cage's colour for its *relative*-anchored variant, so the two
/// same-hue cages (spec vs trial-white) read apart. The nits in each label is
/// the definitive distinguisher; the alpha is just a visual hint.
fn rel_color(c: [f32; 4]) -> [f32; 4] {
    [c[0], c[1], c[2], c[3] * 0.6]
}

/// The measured gamut shell: each refined leaf patch drawn as a quad outline,
/// projected by `view`, with folded (clamped) patches flagged hotter. This is
/// the *actual* probed boundary surface, to read against the ideal cages.
fn measured_cage(
    view: Space3dView,
    gamut: &MeasuredGamut,
    trial_white: [f64; 3],
) -> Vec<LineSegment> {
    let world: Vec<Vec3> = gamut
        .vertices
        .iter()
        .map(|v| view.world(v.xyz, trial_white))
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

/// The 12 edges of the colour space's RGB cube, each subdivided and projected
/// by `view` (so curved edges read as curves, not chords), in `color`.
fn gamut_wireframe(
    view: Space3dView,
    m: &[[f64; 3]; 3],
    cage_white_y: f64,
    trial_white: [f64; 3],
    color: [f32; 4],
) -> Vec<LineSegment> {
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
            let mut prev = cage_corner(view, m, cage_white_y, trial_white, a);
            for step in 1..=GAMUT_SEGMENTS {
                let t = step as f64 / GAMUT_SEGMENTS as f64;
                let rgb = [
                    a[0] + (b[0] - a[0]) * t,
                    a[1] + (b[1] - a[1]) * t,
                    a[2] + (b[2] - a[2]) * t,
                ];
                let cur = cage_corner(view, m, cage_white_y, trial_white, rgb);
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
        expected_xyz: [f64; 3],
        expected_lab: [f64; 3],
        de: f64,
    ) -> AnalyzedSample {
        AnalyzedSample {
            requested: [0.0; 3],
            measured_xyz,
            measured_xy: None,
            expected_xy: None,
            expected_xyz: Some(expected_xyz),
            measured_lab: Some(measured_lab),
            expected_lab: Some(expected_lab),
            delta_uv: None,
            delta_e: Some(de),
            luminance: None,
        }
    }

    /// A scored sRGB trial builds finite, in-range geometry: one dot per
    /// sample (coloured in [0,1] sRGB), one error vector per scorable sample,
    /// and a 12-edge gamut cage. In the relative-Lab view white lands at
    /// L*≈100 on the up axis.
    #[test]
    fn builds_finite_geometry() {
        let white_xyz = scaled_white(200.0);
        let trial = AnalyzedTrial {
            pixel_format: "xrgb8888".into(),
            ground_truth: GroundTruth::Known {
                space: ColorSpace::SRGB,
                transfer: "srgb".into(),
                absolute: false,
                source: GroundTruthSource::Negotiated,
            },
            samples: vec![
                sample(
                    white_xyz,
                    [100.0, 0.0, 0.0],
                    white_xyz,
                    [100.0, 0.0, 0.0],
                    0.0,
                ),
                sample(
                    [80.0, 40.0, 4.0],
                    [51.0, 60.0, 40.0],
                    [88.0, 52.0, 6.0],
                    [54.0, 80.0, 67.0],
                    25.0,
                ),
            ],
            summary: None,
            reference_white_xyz: Some(white_xyz),
        };

        let scene = Space3dScene::build(
            &trial,
            3,
            Space3dView::LabRelative,
            RefCages::default(),
            None,
            false,
        );
        assert_eq!(scene.trial, 3);
        let one_abs = RefCages {
            abs: [true, false, false],
            rel: [false; N_REF_GAMUTS],
        };
        assert!(scene.matches(3, Space3dView::LabRelative, RefCages::default(), false));
        assert!(!scene.matches(3, Space3dView::XyYNits, RefCages::default(), false));
        assert!(!scene.matches(3, Space3dView::LabRelative, one_abs, false));
        assert!(!scene.matches(3, Space3dView::LabRelative, RefCages::default(), true));
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
        // The target cage is labelled with its peak (measured-white) luminance.
        assert_eq!(scene.gamut_labels.get(0), Some("sRGB · target · 200 cd/m²"));
    }

    /// Every projection produces finite geometry, and the key contrast holds:
    /// the relative-Lab view pins the trial's reference white at L*=100 whatever
    /// its absolute brightness, while the absolute views move it with luminance.
    /// That is exactly what "absolute nits" buys for an sRGB-vs-BT.2020 read.
    #[test]
    fn projections_finite_and_absolute_views_track_luminance() {
        // World height of a trial whose (single) sample *is* the reference white.
        let ref_white_height = |view: Space3dView, nits: f64| -> f32 {
            let w = scaled_white(nits);
            let trial = AnalyzedTrial {
                pixel_format: "x".into(),
                ground_truth: GroundTruth::Known {
                    space: ColorSpace::BT2020,
                    transfer: "st2084_pq".into(),
                    absolute: true,
                    source: GroundTruthSource::Negotiated,
                },
                samples: vec![sample(w, [100.0, 0.0, 0.0], w, [100.0, 0.0, 0.0], 0.0)],
                summary: None,
                reference_white_xyz: Some(w),
            };
            let scene = Space3dScene::build(&trial, 0, view, RefCages::default(), None, false);
            let (pts, _) = scene.points.snapshot();
            assert_eq!(pts.points.len(), 1);
            let y = pts.points[0].position.y;
            assert!(y.is_finite(), "{view:?} produced a non-finite height");
            y
        };

        for view in [
            Space3dView::LabRelative,
            Space3dView::LabAbsolute,
            Space3dView::XyYNits,
            Space3dView::ICtCp,
        ] {
            let dim = ref_white_height(view, 200.0);
            let bright = ref_white_height(view, 1000.0);
            match view {
                // Relative: the reference white is L*=100 (world y=100) either way.
                Space3dView::LabRelative => {
                    assert!((dim - 100.0).abs() < 1e-3, "relative white ≠ 100: {dim}");
                    assert!(
                        (bright - 100.0).abs() < 1e-3,
                        "relative white ≠ 100: {bright}"
                    );
                }
                // Absolute: a brighter white sits visibly higher.
                _ => assert!(
                    bright > dim + 1.0,
                    "{view:?} should lift a brighter white, dim={dim} bright={bright}"
                ),
            }
        }
    }

    /// An sRGB-target trial used to exercise the abs/rel cage sets.
    fn srgb_target_trial(white_nits: f64) -> AnalyzedTrial {
        let white_xyz = scaled_white(white_nits);
        AnalyzedTrial {
            pixel_format: "x".into(),
            ground_truth: GroundTruth::Known {
                space: ColorSpace::SRGB, // target = sRGB (REF_GAMUTS[0])
                transfer: "srgb".into(),
                absolute: false,
                source: GroundTruthSource::Negotiated,
            },
            samples: vec![sample(
                white_xyz,
                [100.0, 0.0, 0.0],
                white_xyz,
                [100.0, 0.0, 0.0],
                0.0,
            )],
            summary: None,
            reference_white_xyz: Some(white_xyz),
        }
    }

    /// An enabled reference adds a cage + label. The *relative* cage of the
    /// target's own gamut is deduplicated (it would coincide with the target);
    /// the *absolute* cage of that gamut is not — at its spec white it's a
    /// distinct volume, which is the whole point.
    #[test]
    fn reference_overlays_add_and_dedup_cages() {
        let trial = srgb_target_trial(200.0);
        let segs = |s: &Space3dScene| s.gamut.snapshot().0.segments.len();
        let labels = |s: &Space3dScene| s.gamut_label_geo.snapshot().0.points.len();

        // Display P3 absolute (index 1): target + P3 = two cages, two labels.
        let p3 = Space3dScene::build(
            &trial,
            0,
            Space3dView::LabRelative,
            RefCages {
                abs: [false, true, false],
                rel: [false; N_REF_GAMUTS],
            },
            None,
            false,
        );
        assert_eq!(segs(&p3), 2 * 12 * GAMUT_SEGMENTS);
        assert_eq!(labels(&p3), 2);

        // sRGB *relative* == the target: deduped to one cage.
        let srgb_rel = Space3dScene::build(
            &trial,
            0,
            Space3dView::LabRelative,
            RefCages {
                abs: [false; N_REF_GAMUTS],
                rel: [true, false, false],
            },
            None,
            false,
        );
        assert_eq!(segs(&srgb_rel), 12 * GAMUT_SEGMENTS);
        assert_eq!(labels(&srgb_rel), 1);

        // sRGB *absolute* (spec 80 vs the trial's 200) is a distinct volume —
        // not deduped, so target + abs sRGB = two cages.
        let srgb_abs = Space3dScene::build(
            &trial,
            0,
            Space3dView::LabRelative,
            RefCages {
                abs: [true, false, false],
                rel: [false; N_REF_GAMUTS],
            },
            None,
            false,
        );
        assert_eq!(segs(&srgb_abs), 2 * 12 * GAMUT_SEGMENTS);
        assert_eq!(labels(&srgb_abs), 2);
    }

    /// Cage labels carry the peak white luminance they were scaled to, and the
    /// anchoring is now decoupled from the projection: with both an absolute and
    /// a relative Rec.2020 enabled, the abs cage shows its spec white (10000) and
    /// the rel cage shows the trial white (250) — identically in *every* view.
    #[test]
    fn cage_labels_show_peak_nits_independent_of_view() {
        let trial = srgb_target_trial(250.0);
        // Rec.2020 (index 2) in both anchor sets.
        let refs = RefCages {
            abs: [false, false, true],
            rel: [false, false, true],
        };
        for view in [
            Space3dView::LabRelative,
            Space3dView::LabAbsolute,
            Space3dView::XyYNits,
            Space3dView::ICtCp,
        ] {
            let scene = Space3dScene::build(&trial, 0, view, refs, None, false);
            // Order: target, then the abs set, then the rel set.
            assert_eq!(
                scene.gamut_labels.get(0),
                Some("sRGB · target · 250 cd/m²"),
                "{view:?}"
            );
            assert_eq!(
                scene.gamut_labels.get(1),
                Some("Rec.2020 · 10000 cd/m²"),
                "{view:?} abs"
            );
            assert_eq!(
                scene.gamut_labels.get(2),
                Some("Rec.2020 · 250 cd/m²"),
                "{view:?} rel"
            );
        }
    }

    /// The measured-shell overlay adds patch-outline segments + a "measured"
    /// label only when enabled, and flags folded patches in the hot colour.
    #[test]
    fn measured_shell_overlay_adds_when_enabled() {
        use tristim_capture::{GamutPatch, GamutVertex, MeasuredGamut};
        let white_xyz = scaled_white(200.0);
        let trial = AnalyzedTrial {
            pixel_format: "xrgb8888".into(),
            ground_truth: GroundTruth::Known {
                space: ColorSpace::SRGB,
                transfer: "srgb".into(),
                absolute: false,
                source: GroundTruthSource::Negotiated,
            },
            samples: vec![sample(
                white_xyz,
                [100.0, 0.0, 0.0],
                white_xyz,
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
        let off = Space3dScene::build(
            &trial,
            0,
            Space3dView::LabRelative,
            RefCages::default(),
            Some(&gamut),
            false,
        );
        let off_segs = off.gamut.snapshot().0.segments.len();
        assert_eq!(off.gamut_label_geo.snapshot().0.points.len(), 1);

        // On: +4 patch-edge segments and a "measured" label anchor.
        let on = Space3dScene::build(
            &trial,
            0,
            Space3dView::LabRelative,
            RefCages::default(),
            Some(&gamut),
            true,
        );
        let (segs, _) = on.gamut.snapshot();
        assert_eq!(segs.segments.len(), off_segs + 4);
        assert_eq!(on.gamut_label_geo.snapshot().0.points.len(), 2);
        // The shell is labelled with its brightest probed vertex (green, Y=70).
        assert_eq!(on.gamut_labels.get(1), Some("measured · 70 cd/m²"));
        // The folded patch is drawn in the hot fold colour.
        assert!(segs.segments.iter().any(|s| s.color == MEASURED_FOLD_COLOR));
    }

    /// An unscored trial (no reference white) yields an empty scene that the
    /// chart renders as a placeholder rather than panicking.
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
        let scene = Space3dScene::build(
            &trial,
            0,
            Space3dView::LabRelative,
            RefCages {
                abs: [true; N_REF_GAMUTS],
                rel: [true; N_REF_GAMUTS],
            },
            None,
            false,
        );
        assert!(!scene.has_data);
        assert_eq!(scene.points.snapshot().0.points.len(), 0);
        // No reference white ⇒ no frame ⇒ no cages even with overlays on.
        assert_eq!(scene.gamut.snapshot().0.segments.len(), 0);
        assert_eq!(scene.gamut_label_geo.snapshot().0.points.len(), 0);
    }

    /// D65 white at an absolute luminance, as the analyzer's measured-white XYZ.
    fn scaled_white(nits: f64) -> [f64; 3] {
        let w = chromaticity_to_xyz(white::D65);
        [w[0] * nits, nits, w[2] * nits]
    }
}
