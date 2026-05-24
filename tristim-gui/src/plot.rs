//! Pure geometry for the CIE 1976 u'v' chromaticity plot: the data→screen
//! projection, the spectral-locus outline, and color-space gamut triangles.
//!
//! Deliberately free of any aetna types so it can be unit tested in isolation;
//! the [`crate::chart`] module turns these coordinates into aetna vector paths.

use tristim_color::ColorSpace;
use tristim_color::metrics::xy_to_uv_prime;

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

/// The spectral locus in CIE 1976 u'v' (a closed polygon when the ends are
/// joined by the line of purples).
pub fn locus_uv() -> Vec<[f64; 2]> {
    LOCUS_XY.iter().map(|&xy| xy_to_uv_prime(xy)).collect()
}

/// The u'v' window the plot shows, with a little padding past the locus
/// extremes (u' spans ~0–0.62, v' ~0–0.59).
#[derive(Clone, Copy, Debug)]
pub struct UvView {
    pub u_min: f64,
    pub u_max: f64,
    pub v_min: f64,
    pub v_max: f64,
}

impl UvView {
    pub const DEFAULT: UvView = UvView {
        u_min: -0.02,
        u_max: 0.65,
        v_min: -0.02,
        v_max: 0.62,
    };
}

/// Maps u'v' coordinates into a screen rectangle, flipping v' (which increases
/// upward) onto screen-y (which increases downward).
#[derive(Clone, Copy, Debug)]
pub struct Projector {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    view: UvView,
}

impl Projector {
    /// `rect` is `[x, y, w, h]` in the target (screen / view_box) space.
    pub fn new(rect: [f32; 4], view: UvView) -> Self {
        Self {
            x: rect[0],
            y: rect[1],
            w: rect[2],
            h: rect[3],
            view,
        }
    }

    /// Project a u'v' coordinate to a screen-space `[x, y]` in pixels.
    pub fn project(&self, uv: [f64; 2]) -> [f32; 2] {
        let fu = ((uv[0] - self.view.u_min) / (self.view.u_max - self.view.u_min)) as f32;
        let fv = ((uv[1] - self.view.v_min) / (self.view.v_max - self.view.v_min)) as f32;
        [self.x + fu * self.w, self.y + (1.0 - fv) * self.h]
    }
}

/// The three primary vertices (R, G, B) of a color space, in u'v'.
pub fn gamut_uv(space: &ColorSpace) -> [[f64; 2]; 3] {
    [
        xy_to_uv_prime(space.red),
        xy_to_uv_prime(space.green),
        xy_to_uv_prime(space.blue),
    ]
}

/// A color space's reference white, in u'v'.
pub fn white_uv(space: &ColorSpace) -> [f64; 2] {
    xy_to_uv_prime(space.white)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn projects_view_corners_to_rect_corners_with_v_flip() {
        let view = UvView::DEFAULT;
        let p = Projector::new([0.0, 0.0, 100.0, 100.0], view);
        // (u_min, v_min) → bottom-left; (u_max, v_max) → top-right.
        let bl = p.project([view.u_min, view.v_min]);
        let tr = p.project([view.u_max, view.v_max]);
        let tl = p.project([view.u_min, view.v_max]);
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
    fn locus_uv_has_one_point_per_xy() {
        assert_eq!(locus_uv().len(), LOCUS_XY.len());
    }

    #[test]
    fn srgb_gamut_uv_matches_known_values() {
        // sRGB red xy (0.640, 0.330) → u'v' (0.4507, 0.5229) by hand.
        let g = gamut_uv(&ColorSpace::SRGB);
        assert!((g[0][0] - 0.4507).abs() < 1e-3, "red u' = {}", g[0][0]);
        assert!((g[0][1] - 0.5229).abs() < 1e-3, "red v' = {}", g[0][1]);
        // D65 white (0.3127, 0.3290) → u'v' (0.1978, 0.4683).
        let w = white_uv(&ColorSpace::SRGB);
        assert!((w[0] - 0.1978).abs() < 1e-3, "white u' = {}", w[0]);
        assert!((w[1] - 0.4683).abs() < 1e-3, "white v' = {}", w[1]);
    }

    #[test]
    fn locus_stays_within_default_view() {
        let view = UvView::DEFAULT;
        for uv in locus_uv() {
            assert!(
                uv[0] >= view.u_min && uv[0] <= view.u_max,
                "u' {} out of view",
                uv[0]
            );
            assert!(
                uv[1] >= view.v_min && uv[1] <= view.v_max,
                "v' {} out of view",
                uv[1]
            );
        }
    }
}
