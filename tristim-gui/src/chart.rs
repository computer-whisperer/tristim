//! Render an analyzed trial as an aetna chart: a chromaticity diagram (CIE 1931
//! xy or 1976 u'v') with the spectral locus, the target gamut triangle and
//! white point, and each measured sample joined by an error vector to where it
//! should have landed — colored by ΔE\*ab.
//!
//! The vector overlays are built as one [`VectorAsset`]; the optional color
//! field behind them is a custom-shader element (see `chroma_field.wgsl`). Both
//! are a fixed-size square (the vector view_box / the element size), so the plot
//! stays aspect-correct regardless of where layout places it.

use aetna_core::prelude::*;
use tristim_analyze::{AnalyzedTrial, GroundTruth};
use tristim_color::ColorSpace;

use crate::plot::{Projector, Space, gamut_in, locus_in, white_in};

/// Side length (px) of the square plot; also the vector view_box extent.
const SIZE: f32 = 440.0;

/// Name shared by the registered shader and its binding (see [`field_shader`]).
const FIELD_SHADER: &str = "chroma_field";

/// The presenter window's own negotiated gamut — what it can actually display.
/// The color fill is bounded to this so every painted color is in-gamut (no
/// clipping, no lying).
#[derive(Clone, Copy, Debug)]
pub enum PresenterGamut {
    Srgb,
    DisplayP3,
    Bt2020,
}

impl PresenterGamut {
    fn space(self) -> ColorSpace {
        match self {
            PresenterGamut::Srgb => ColorSpace::SRGB,
            PresenterGamut::DisplayP3 => ColorSpace::DISPLAY_P3,
            PresenterGamut::Bt2020 => ColorSpace::BT2020,
        }
    }
}

/// The color-field shader to register from `App::shaders`.
pub fn field_shader() -> AppShader {
    AppShader {
        name: FIELD_SHADER,
        wgsl: include_str!("chroma_field.wgsl"),
        samples_backdrop: false,
        samples_time: false,
    }
}

/// Build the chromaticity chart El for `t` in the chosen `space`. When `field`
/// is `Some`, a shader-painted color backdrop bounded to that (the presenter's)
/// gamut sits behind the vector overlays.
pub fn chromaticity_chart(t: &AnalyzedTrial, space: Space, field: Option<PresenterGamut>) -> El {
    let overlays = vector_chart(t, space);
    match field {
        Some(gamut) => stack([field_el(gamut, space), overlays])
            .width(Size::Fixed(SIZE))
            .height(Size::Fixed(SIZE)),
        None => overlays,
    }
}

/// The per-pixel color field: a custom-shader square. The shader maps each
/// pixel to its chromaticity's color and clips to the gamut (see the WGSL). All
/// it needs is the gamut's XYZ→RGB matrix, the plot view window, and which
/// projection is in use — packed into the generic vec slots.
fn field_el(gamut: PresenterGamut, space: Space) -> El {
    let mat = gamut.space().xyz_to_rgb();
    let e = |r: usize, c: usize| mat[r][c] as f32;
    let v = space.view();
    let flag = match space {
        Space::Xy => 0.0,
        Space::UvPrime => 1.0,
    };
    El::new(Kind::Custom(FIELD_SHADER))
        .width(Size::Fixed(SIZE))
        .height(Size::Fixed(SIZE))
        .shader(
            ShaderBinding::custom(FIELD_SHADER)
                .vec4("vec_a", [e(0, 0), e(0, 1), e(0, 2), e(1, 0)])
                .vec4("vec_b", [e(1, 1), e(1, 2), e(2, 0), e(2, 1)])
                .vec4("vec_c", [e(2, 2), flag, 0.0, 0.0])
                .vec4(
                    "vec_d",
                    [
                        v.x_min as f32,
                        v.x_max as f32,
                        v.y_min as f32,
                        v.y_max as f32,
                    ],
                ),
        )
}

/// The vector overlays: frame, spectral locus, target gamut triangle + white
/// point (when the trial has a basis), and per-sample error vectors + dots.
fn vector_chart(t: &AnalyzedTrial, space: Space) -> El {
    let proj = Projector::new([0.0, 0.0, SIZE, SIZE], space.view());
    let mut paths: Vec<VectorPath> = vec![frame_path(), locus_path(&proj, space)];

    if let GroundTruth::Known { space: target, .. } = &t.ground_truth {
        paths.push(triangle_path(&proj, &gamut_in(space, target)));
        white_marker(&proj, white_in(space, target), &mut paths);
    }

    for s in &t.samples {
        let Some(m_xy) = s.measured_xy else { continue };
        let m = proj.project(space.project(m_xy));
        let color = s.delta_e.map_or(UNSCORED, heat);
        if let Some(e_xy) = s.expected_xy {
            let e = proj.project(space.project(e_xy));
            paths.push(line_path(e, m, color, 1.5));
        }
        paths.push(circle(m, 4.0).fill_solid(color).build());
    }

    vector(VectorAsset::from_paths([0.0, 0.0, SIZE, SIZE], paths))
}

const LOCUS: Color = Color::srgb_u8(150, 150, 160);
const TRIANGLE: Color = Color::srgb_u8(150, 190, 255);
const WHITE: Color = Color::srgb_u8(245, 245, 245);
const UNSCORED: Color = Color::srgb_u8(140, 140, 150);

/// Faint border just inside the view_box so the stroke isn't clipped.
fn frame_path() -> VectorPath {
    let m = 0.5;
    PathBuilder::new()
        .move_to(m, m)
        .line_to(SIZE - m, m)
        .line_to(SIZE - m, SIZE - m)
        .line_to(m, SIZE - m)
        .close()
        .stroke_solid(tokens::BORDER, 1.0)
        .build()
}

fn locus_path(proj: &Projector, space: Space) -> VectorPath {
    let pts = locus_in(space);
    let p0 = proj.project(pts[0]);
    let mut pb = PathBuilder::new().move_to(p0[0], p0[1]);
    for &c in &pts[1..] {
        let p = proj.project(c);
        pb = pb.line_to(p[0], p[1]);
    }
    pb.close().stroke_solid(LOCUS, 1.5).build()
}

fn triangle_path(proj: &Projector, g: &[[f64; 2]; 3]) -> VectorPath {
    let a = proj.project(g[0]);
    let b = proj.project(g[1]);
    let c = proj.project(g[2]);
    PathBuilder::new()
        .move_to(a[0], a[1])
        .line_to(b[0], b[1])
        .line_to(c[0], c[1])
        .close()
        .stroke_solid(TRIANGLE, 1.5)
        .build()
}

/// A hollow ring + center dot at the target white point.
fn white_marker(proj: &Projector, p: [f64; 2], out: &mut Vec<VectorPath>) {
    let c = proj.project(p);
    out.push(circle(c, 5.0).stroke_solid(WHITE, 1.5).build());
    out.push(circle(c, 1.5).fill_solid(WHITE).build());
}

fn line_path(a: [f32; 2], b: [f32; 2], color: Color, width: f32) -> VectorPath {
    PathBuilder::new()
        .move_to(a[0], a[1])
        .line_to(b[0], b[1])
        .stroke_solid(color, width)
        .build()
}

/// An unbuilt circular path centered at `c` with radius `r`, approximated by
/// four cubic Béziers. The caller adds the fill or stroke.
fn circle(c: [f32; 2], r: f32) -> PathBuilder {
    const K: f32 = 0.552_285; // 4/3 * (sqrt(2) - 1)
    let (x, y) = (c[0], c[1]);
    let k = K * r;
    PathBuilder::new()
        .move_to(x, y - r)
        .cubic_to(x + k, y - r, x + r, y - k, x + r, y)
        .cubic_to(x + r, y + k, x + k, y + r, x, y + r)
        .cubic_to(x - k, y + r, x - r, y + k, x - r, y)
        .cubic_to(x - r, y - k, x - k, y - r, x, y - r)
        .close()
}

/// ΔE\*ab → heat color: green (faithful) through amber to red (severe). The
/// scale saturates near ΔE 15, well above the ~2.3 just-noticeable threshold.
fn heat(de: f64) -> Color {
    let good = Color::srgb_u8(90, 200, 130);
    let amber = Color::srgb_u8(240, 200, 70);
    let bad = Color::srgb_u8(235, 80, 80);
    let t = (de / 15.0).clamp(0.0, 1.0) as f32;
    if t < 0.5 {
        good.mix(amber, t * 2.0)
    } else {
        amber.mix(bad, (t - 0.5) * 2.0)
    }
}
