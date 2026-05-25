//! Pure geometry for the chromaticity plot: the chromaticity-space choice
//! (CIE 1931 xy or CIE 1976 u'v'), the data→screen projection, the embedded
//! spectral locus, and color-space gamut triangles.
//!
//! Deliberately free of any aetna types so it can be unit tested in isolation;
//! the [`crate::chart`] module turns these coordinates into aetna vector paths.

use tristim_color::ColorSpace;
use tristim_color::metrics::{uv_prime_to_xy, xy_to_uv_prime};

/// CIE 1931 2° standard-observer chromaticity coordinates `(x, y)` of the
/// spectral locus, 380–700 nm at 5 nm steps. Reference data for drawing the
/// horseshoe outline; the open ends are joined by the line of purples.
///
/// These are the standard published chromaticities of the CIE 1931 2° observer.
/// The locus is a visual reference, not a measured quantity, so the sampling is
/// coarse enough to keep the curve smooth without embedding the full table.
#[rustfmt::skip]
pub const LOCUS_XY: &[[f64; 2]] = &[
    [0.1741, 0.0050], // 380
    [0.1740, 0.0050], // 385
    [0.1738, 0.0049], // 390
    [0.1736, 0.0049], // 395
    [0.1733, 0.0048], // 400
    [0.1730, 0.0048], // 405
    [0.1726, 0.0048], // 410
    [0.1721, 0.0048], // 415
    [0.1714, 0.0051], // 420
    [0.1703, 0.0058], // 425
    [0.1689, 0.0069], // 430
    [0.1669, 0.0086], // 435
    [0.1644, 0.0109], // 440
    [0.1611, 0.0138], // 445
    [0.1566, 0.0177], // 450
    [0.1510, 0.0227], // 455
    [0.1440, 0.0297], // 460
    [0.1355, 0.0399], // 465
    [0.1241, 0.0578], // 470
    [0.1096, 0.0868], // 475
    [0.0913, 0.1327], // 480
    [0.0687, 0.2007], // 485
    [0.0454, 0.2950], // 490
    [0.0235, 0.4127], // 495
    [0.0082, 0.5384], // 500
    [0.0039, 0.6548], // 505
    [0.0139, 0.7502], // 510
    [0.0389, 0.8120], // 515
    [0.0743, 0.8338], // 520
    [0.1142, 0.8262], // 525
    [0.1547, 0.8059], // 530
    [0.1929, 0.7816], // 535
    [0.2296, 0.7543], // 540
    [0.2658, 0.7243], // 545
    [0.3016, 0.6923], // 550
    [0.3373, 0.6589], // 555
    [0.3731, 0.6245], // 560
    [0.4087, 0.5896], // 565
    [0.4441, 0.5547], // 570
    [0.4788, 0.5202], // 575
    [0.5125, 0.4866], // 580
    [0.5448, 0.4544], // 585
    [0.5752, 0.4242], // 590
    [0.6029, 0.3965], // 595
    [0.6270, 0.3725], // 600
    [0.6482, 0.3514], // 605
    [0.6658, 0.3340], // 610
    [0.6801, 0.3197], // 615
    [0.6915, 0.3083], // 620
    [0.7006, 0.2993], // 625
    [0.7079, 0.2920], // 630
    [0.7140, 0.2859], // 635
    [0.7190, 0.2809], // 640
    [0.7230, 0.2770], // 645
    [0.7260, 0.2740], // 650
    [0.7283, 0.2717], // 655
    [0.7300, 0.2700], // 660
    [0.7311, 0.2689], // 665
    [0.7320, 0.2680], // 670
    [0.7327, 0.2673], // 675
    [0.7334, 0.2666], // 680
    [0.7340, 0.2660], // 685
    [0.7344, 0.2656], // 690
    [0.7346, 0.2654], // 695
    [0.7347, 0.2653], // 700
];

/// Which chromaticity projection the plot uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    /// CIE 1931 xy — the familiar diagram.
    Xy,
    /// CIE 1976 u'v' UCS — perceptually uniform, so a Δu'v' error reads as a
    /// proportional on-screen distance.
    UvPrime,
}

impl Space {
    /// Map a CIE xy chromaticity into this space's plot coordinate.
    pub fn project(self, xy: [f64; 2]) -> [f64; 2] {
        match self {
            Space::Xy => xy,
            Space::UvPrime => xy_to_uv_prime(xy),
        }
    }

    /// Inverse of [`Self::project`]: a plot coordinate back to CIE xy.
    pub fn to_xy(self, p: [f64; 2]) -> [f64; 2] {
        match self {
            Space::Xy => p,
            Space::UvPrime => uv_prime_to_xy(p),
        }
    }

    /// The plot domain (with a little padding past the locus) for this space.
    pub fn view(self) -> View {
        match self {
            Space::Xy => View {
                x_min: -0.02,
                x_max: 0.75,
                y_min: -0.02,
                y_max: 0.87,
            },
            Space::UvPrime => View {
                x_min: -0.02,
                x_max: 0.65,
                y_min: -0.02,
                y_max: 0.62,
            },
        }
    }

    /// Short human label for legends/toggles.
    pub fn label(self) -> &'static str {
        match self {
            Space::Xy => "CIE 1931 xy",
            Space::UvPrime => "CIE 1976 u'v'",
        }
    }

    /// The other projection (for a toggle).
    pub fn toggled(self) -> Space {
        match self {
            Space::Xy => Space::UvPrime,
            Space::UvPrime => Space::Xy,
        }
    }
}

/// The plot's coordinate window: `[min, max]` on each axis, in plot space.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub x_min: f64,
    pub x_max: f64,
    pub y_min: f64,
    pub y_max: f64,
}

/// The spectral locus in `space` (a closed polygon when the ends are joined by
/// the line of purples).
pub fn locus_in(space: Space) -> Vec<[f64; 2]> {
    LOCUS_XY.iter().map(|&xy| space.project(xy)).collect()
}

/// Maps plot-space coordinates into a screen rectangle, flipping y (which
/// increases upward in chromaticity space) onto screen-y (downward).
#[derive(Clone, Copy, Debug)]
pub struct Projector {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    view: View,
}

impl Projector {
    /// `rect` is `[x, y, w, h]` in the target (screen / view_box) space.
    pub fn new(rect: [f32; 4], view: View) -> Self {
        Self {
            x: rect[0],
            y: rect[1],
            w: rect[2],
            h: rect[3],
            view,
        }
    }

    /// Project a plot-space coordinate to a screen-space `[x, y]` in pixels.
    pub fn project(&self, p: [f64; 2]) -> [f32; 2] {
        let fx = ((p[0] - self.view.x_min) / (self.view.x_max - self.view.x_min)) as f32;
        let fy = ((p[1] - self.view.y_min) / (self.view.y_max - self.view.y_min)) as f32;
        [self.x + fx * self.w, self.y + (1.0 - fy) * self.h]
    }
}

/// The three primary vertices (R, G, B) of a color space, in `space`.
pub fn gamut_in(space: Space, cs: &ColorSpace) -> [[f64; 2]; 3] {
    [
        space.project(cs.red),
        space.project(cs.green),
        space.project(cs.blue),
    ]
}

/// A color space's reference white, in `space`.
pub fn white_in(space: Space, cs: &ColorSpace) -> [f64; 2] {
    space.project(cs.white)
}

/// Subdivide a triangle into `n²` smaller triangles by barycentric subdivision,
/// returning each as three vertices in the input space. Used to tile a gamut
/// triangle for the chromaticity color fill — each cell can then be flat-filled
/// with the true color of its centroid chromaticity.
pub fn subdivide_triangle(verts: [[f64; 2]; 3], n: usize) -> Vec<[[f64; 2]; 3]> {
    let n = n.max(1);
    let nf = n as f64;
    // Barycentric grid point: weights (1-a/n-b/n, a/n, b/n) over the vertices.
    let point = |a: usize, b: usize| -> [f64; 2] {
        let l1 = a as f64 / nf;
        let l2 = b as f64 / nf;
        let l0 = 1.0 - l1 - l2;
        [
            l0 * verts[0][0] + l1 * verts[1][0] + l2 * verts[2][0],
            l0 * verts[0][1] + l1 * verts[1][1] + l2 * verts[2][1],
        ]
    };
    let mut tris = Vec::with_capacity(n * n);
    for a in 0..n {
        for b in 0..(n - a) {
            tris.push([point(a, b), point(a + 1, b), point(a, b + 1)]);
            if a + b + 1 < n {
                tris.push([point(a + 1, b), point(a + 1, b + 1), point(a, b + 1)]);
            }
        }
    }
    tris
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn projects_view_corners_to_rect_corners_with_y_flip() {
        let view = Space::UvPrime.view();
        let p = Projector::new([0.0, 0.0, 100.0, 100.0], view);
        // (x_min, y_min) → bottom-left; (x_max, y_max) → top-right.
        let bl = p.project([view.x_min, view.y_min]);
        let tr = p.project([view.x_max, view.y_max]);
        let tl = p.project([view.x_min, view.y_max]);
        assert!(
            close(bl[0], 0.0, 1e-3) && close(bl[1], 100.0, 1e-3),
            "{bl:?}"
        );
        assert!(
            close(tr[0], 100.0, 1e-3) && close(tr[1], 0.0, 1e-3),
            "{tr:?}"
        );
        assert!(close(tl[0], 0.0, 1e-3) && close(tl[1], 0.0, 1e-3), "{tl:?}");
    }

    #[test]
    fn locus_has_one_point_per_xy_in_both_spaces() {
        assert_eq!(locus_in(Space::Xy).len(), LOCUS_XY.len());
        assert_eq!(locus_in(Space::UvPrime).len(), LOCUS_XY.len());
    }

    #[test]
    fn srgb_gamut_uv_matches_known_values() {
        // sRGB red xy (0.640, 0.330) → u'v' (0.4507, 0.5229) by hand.
        let g = gamut_in(Space::UvPrime, &ColorSpace::SRGB);
        assert!((g[0][0] - 0.4507).abs() < 1e-3, "red u' = {}", g[0][0]);
        assert!((g[0][1] - 0.5229).abs() < 1e-3, "red v' = {}", g[0][1]);
        // D65 white (0.3127, 0.3290) → u'v' (0.1978, 0.4683).
        let w = white_in(Space::UvPrime, &ColorSpace::SRGB);
        assert!((w[0] - 0.1978).abs() < 1e-3, "white u' = {}", w[0]);
        assert!((w[1] - 0.4683).abs() < 1e-3, "white v' = {}", w[1]);
    }

    #[test]
    fn xy_space_is_identity() {
        // In xy the gamut vertices are just the primaries themselves.
        let g = gamut_in(Space::Xy, &ColorSpace::SRGB);
        assert_eq!(g[0], ColorSpace::SRGB.red);
        assert_eq!(g[1], ColorSpace::SRGB.green);
        assert_eq!(g[2], ColorSpace::SRGB.blue);
    }

    #[test]
    fn locus_stays_within_view_in_both_spaces() {
        for space in [Space::Xy, Space::UvPrime] {
            let v = space.view();
            for p in locus_in(space) {
                assert!(
                    p[0] >= v.x_min && p[0] <= v.x_max,
                    "{:?}: x {} out",
                    space,
                    p[0]
                );
                assert!(
                    p[1] >= v.y_min && p[1] <= v.y_max,
                    "{:?}: y {} out",
                    space,
                    p[1]
                );
            }
        }
    }

    #[test]
    fn subdivide_triangle_produces_n_squared_cells() {
        let tri = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        for n in [1, 2, 4, 8, 32] {
            assert_eq!(subdivide_triangle(tri, n).len(), n * n, "n={n}");
        }
    }

    #[test]
    fn subdivide_triangle_keeps_cells_inside() {
        // Every sub-vertex must stay within the unit triangle (x,y ≥ 0, x+y ≤ 1).
        let tri = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        for cell in subdivide_triangle(tri, 8) {
            for [x, y] in cell {
                assert!(x >= -1e-9 && y >= -1e-9 && x + y <= 1.0 + 1e-9, "({x},{y})");
            }
        }
    }
}
