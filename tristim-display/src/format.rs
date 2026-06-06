//! Buffer pixel formats: every RGB-family `wl_shm` format, with one
//! table-driven encoder.
//!
//! Each format is described by its channel fields read MSB→LSB of a
//! little-endian word (exactly how `wayland.xml` / `drm_fourcc.h` document
//! them), plus whether the channels are unsigned-normalized integers or
//! IEEE floats. [`BufferFormat::encode`] packs one RGB code-value triple
//! into that word and emits its first `bytes_per_pixel()` little-endian
//! bytes.
//!
//! Code values are written **verbatim**: unorm channels quantize
//! `0..=1` to the channel's full range (out-of-range values clamp —
//! an integer channel can't represent them); float channels carry the
//! value bit-exactly, including negatives and >1.0 (required for scRGB
//! and other extended-range encodings). Alpha and padding channels are
//! always written fully opaque (`max` / `1.0`) so a compositor that
//! samples them sees a sane value.
//!
//! Deliberately excluded:
//! - YUV / planar formats: writing them would mean an RGB→YUV
//!   conversion, i.e. *interpretation* of the code values, which this
//!   crate refuses to do.
//! - Single- and dual-channel formats (`R8`, `GR88`, …): not RGB color.
//! - `abgr32323232f`: `wayland.xml`'s layout summary (`[127:0] R:G:B:A`)
//!   contradicts the format name's own A:B:G:R order, so the layout is
//!   ambiguous; excluded rather than risk silently wrong test data.
//! - The `*_a8` two-plane formats: `wl_shm` buffers are single-plane.

use wayland_client::protocol::wl_shm;

/// One channel of a pixel word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chan {
    R,
    G,
    B,
    /// Alpha — written fully opaque.
    A,
    /// Padding — written like alpha (opaque) so stray samplers see 1.0.
    X,
}

/// One encoded pixel: up to 16 little-endian bytes, `len` of them valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodedPixel {
    bytes: [u8; 16],
    len: usize,
}

impl EncodedPixel {
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl std::ops::Deref for EncodedPixel {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

macro_rules! buffer_formats {
    ($( $variant:ident, $name:literal, $float:literal, [ $( ($ch:ident, $w:literal) ),+ ] );+ $(;)?) => {
        /// Buffer pixel format for the patch surface — every RGB-family
        /// `wl_shm` format (see the module docs for what's excluded and
        /// why). Naming follows `wl_shm`/`drm_fourcc`: the name lists
        /// channels MSB→LSB of a little-endian word, so e.g.
        /// [`Xrgb8888`](Self::Xrgb8888) has B in the lowest byte.
        ///
        /// Only [`Xrgb8888`](Self::Xrgb8888) and [`Argb8888`](Self::Argb8888)
        /// are mandatory per the `wl_shm` spec; everything else must be
        /// advertised by the compositor (see
        /// [`DisplayCapabilities::supports_buffer_format`](crate::DisplayCapabilities::supports_buffer_format)).
        ///
        /// Non-exhaustive: new `wl_shm` formats appear over time. Use
        /// [`name`](Self::name) for display/recording rather than
        /// matching exhaustively.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum BufferFormat {
            $( $variant, )+
        }

        impl BufferFormat {
            /// Every format this build supports, in table order.
            pub const ALL: &'static [BufferFormat] = &[ $( BufferFormat::$variant, )+ ];

            /// The matching `wl_shm` format.
            pub fn wl_format(self) -> wl_shm::Format {
                match self {
                    $( BufferFormat::$variant => wl_shm::Format::$variant, )+
                }
            }

            /// Map back from a `wl_shm` format. `None` for formats this
            /// crate doesn't write (YUV, single-channel, …).
            pub fn from_wl(f: wl_shm::Format) -> Option<Self> {
                match f {
                    $( wl_shm::Format::$variant => Some(BufferFormat::$variant), )+
                    _ => None,
                }
            }

            /// The format's `wl_shm`/fourcc-style lowercase name, e.g.
            /// `"xrgb8888"` — stable, suitable for recording in captures.
            pub fn name(self) -> &'static str {
                match self {
                    $( BufferFormat::$variant => $name, )+
                }
            }

            /// Inverse of [`name`](Self::name).
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $( $name => Some(BufferFormat::$variant), )+
                    _ => None,
                }
            }

            /// Channel fields MSB→LSB of the little-endian pixel word,
            /// plus whether channels are floats.
            fn spec(self) -> (&'static [(Chan, u8)], bool) {
                #[allow(unused_imports)]
                use Chan::*;
                match self {
                    $( BufferFormat::$variant => (&[ $( ($ch, $w) ),+ ], $float), )+
                }
            }
        }
    };
}

buffer_formats! {
    // ── 8 bpp ──────────────────────────────────────────────────────────
    Rgb332,        "rgb332",        false, [(R, 3), (G, 3), (B, 2)];
    Bgr233,        "bgr233",        false, [(B, 2), (G, 3), (R, 3)];
    // ── 16 bpp, 4:4:4:4 ────────────────────────────────────────────────
    Xrgb4444,      "xrgb4444",      false, [(X, 4), (R, 4), (G, 4), (B, 4)];
    Xbgr4444,      "xbgr4444",      false, [(X, 4), (B, 4), (G, 4), (R, 4)];
    Rgbx4444,      "rgbx4444",      false, [(R, 4), (G, 4), (B, 4), (X, 4)];
    Bgrx4444,      "bgrx4444",      false, [(B, 4), (G, 4), (R, 4), (X, 4)];
    Argb4444,      "argb4444",      false, [(A, 4), (R, 4), (G, 4), (B, 4)];
    Abgr4444,      "abgr4444",      false, [(A, 4), (B, 4), (G, 4), (R, 4)];
    Rgba4444,      "rgba4444",      false, [(R, 4), (G, 4), (B, 4), (A, 4)];
    Bgra4444,      "bgra4444",      false, [(B, 4), (G, 4), (R, 4), (A, 4)];
    // ── 16 bpp, 1:5:5:5 / 5:5:5:1 ──────────────────────────────────────
    Xrgb1555,      "xrgb1555",      false, [(X, 1), (R, 5), (G, 5), (B, 5)];
    Xbgr1555,      "xbgr1555",      false, [(X, 1), (B, 5), (G, 5), (R, 5)];
    Rgbx5551,      "rgbx5551",      false, [(R, 5), (G, 5), (B, 5), (X, 1)];
    Bgrx5551,      "bgrx5551",      false, [(B, 5), (G, 5), (R, 5), (X, 1)];
    Argb1555,      "argb1555",      false, [(A, 1), (R, 5), (G, 5), (B, 5)];
    Abgr1555,      "abgr1555",      false, [(A, 1), (B, 5), (G, 5), (R, 5)];
    Rgba5551,      "rgba5551",      false, [(R, 5), (G, 5), (B, 5), (A, 1)];
    Bgra5551,      "bgra5551",      false, [(B, 5), (G, 5), (R, 5), (A, 1)];
    // ── 16 bpp, 5:6:5 ──────────────────────────────────────────────────
    Rgb565,        "rgb565",        false, [(R, 5), (G, 6), (B, 5)];
    Bgr565,        "bgr565",        false, [(B, 5), (G, 6), (R, 5)];
    // ── 24 bpp ─────────────────────────────────────────────────────────
    Rgb888,        "rgb888",        false, [(R, 8), (G, 8), (B, 8)];
    Bgr888,        "bgr888",        false, [(B, 8), (G, 8), (R, 8)];
    // ── 32 bpp, 8-bit channels ─────────────────────────────────────────
    Xrgb8888,      "xrgb8888",      false, [(X, 8), (R, 8), (G, 8), (B, 8)];
    Xbgr8888,      "xbgr8888",      false, [(X, 8), (B, 8), (G, 8), (R, 8)];
    Rgbx8888,      "rgbx8888",      false, [(R, 8), (G, 8), (B, 8), (X, 8)];
    Bgrx8888,      "bgrx8888",      false, [(B, 8), (G, 8), (R, 8), (X, 8)];
    Argb8888,      "argb8888",      false, [(A, 8), (R, 8), (G, 8), (B, 8)];
    Abgr8888,      "abgr8888",      false, [(A, 8), (B, 8), (G, 8), (R, 8)];
    Rgba8888,      "rgba8888",      false, [(R, 8), (G, 8), (B, 8), (A, 8)];
    Bgra8888,      "bgra8888",      false, [(B, 8), (G, 8), (R, 8), (A, 8)];
    // ── 32 bpp, 10-bit channels ────────────────────────────────────────
    Xrgb2101010,   "xrgb2101010",   false, [(X, 2), (R, 10), (G, 10), (B, 10)];
    Xbgr2101010,   "xbgr2101010",   false, [(X, 2), (B, 10), (G, 10), (R, 10)];
    Rgbx1010102,   "rgbx1010102",   false, [(R, 10), (G, 10), (B, 10), (X, 2)];
    Bgrx1010102,   "bgrx1010102",   false, [(B, 10), (G, 10), (R, 10), (X, 2)];
    Argb2101010,   "argb2101010",   false, [(A, 2), (R, 10), (G, 10), (B, 10)];
    Abgr2101010,   "abgr2101010",   false, [(A, 2), (B, 10), (G, 10), (R, 10)];
    Rgba1010102,   "rgba1010102",   false, [(R, 10), (G, 10), (B, 10), (A, 2)];
    Bgra1010102,   "bgra1010102",   false, [(B, 10), (G, 10), (R, 10), (A, 2)];
    // ── 48 bpp ─────────────────────────────────────────────────────────
    Rgb161616,     "rgb161616",     false, [(R, 16), (G, 16), (B, 16)];
    Bgr161616,     "bgr161616",     false, [(B, 16), (G, 16), (R, 16)];
    // ── 64 bpp, 16-bit unorm channels ──────────────────────────────────
    Xrgb16161616,  "xrgb16161616",  false, [(X, 16), (R, 16), (G, 16), (B, 16)];
    Xbgr16161616,  "xbgr16161616",  false, [(X, 16), (B, 16), (G, 16), (R, 16)];
    Argb16161616,  "argb16161616",  false, [(A, 16), (R, 16), (G, 16), (B, 16)];
    Abgr16161616,  "abgr16161616",  false, [(A, 16), (B, 16), (G, 16), (R, 16)];
    // ── 64 bpp, fp16 channels ──────────────────────────────────────────
    Xrgb16161616f, "xrgb16161616f", true,  [(X, 16), (R, 16), (G, 16), (B, 16)];
    Xbgr16161616f, "xbgr16161616f", true,  [(X, 16), (B, 16), (G, 16), (R, 16)];
    Argb16161616f, "argb16161616f", true,  [(A, 16), (R, 16), (G, 16), (B, 16)];
    Abgr16161616f, "abgr16161616f", true,  [(A, 16), (B, 16), (G, 16), (R, 16)];
}

impl BufferFormat {
    /// Bytes per pixel in the buffer.
    pub fn bytes_per_pixel(self) -> usize {
        let (fields, _) = self.spec();
        fields.iter().map(|&(_, w)| w as usize).sum::<usize>() / 8
    }

    /// Whether channels are IEEE floats (fp16) rather than
    /// unsigned-normalized integers.
    pub fn is_float(self) -> bool {
        self.spec().1
    }

    /// Whether the format carries an alpha channel (always written
    /// fully opaque by this crate).
    pub fn has_alpha(self) -> bool {
        self.spec().0.iter().any(|&(ch, _)| ch == Chan::A)
    }

    /// The narrowest color-channel width in bits — the format's
    /// effective depth for a grey ramp (e.g. 5 for `rgb565`, 10 for
    /// `xrgb2101010`, 16 for fp16).
    pub fn color_depth(self) -> u8 {
        let (fields, _) = self.spec();
        fields
            .iter()
            .filter(|&&(ch, _)| matches!(ch, Chan::R | Chan::G | Chan::B))
            .map(|&(_, w)| w)
            .min()
            .unwrap_or(0)
    }

    /// Pack one RGB code-value triple into a pixel.
    ///
    /// Unorm channels quantize `v.clamp(0,1) * (2^w - 1)` (round to
    /// nearest); float channels write the IEEE binary16 bits of `v`
    /// verbatim — including values outside `0..=1`, as extended-range
    /// encodings (scRGB) require. Alpha/padding channels are written
    /// fully opaque.
    pub fn encode(self, rgb: [f64; 3]) -> EncodedPixel {
        let (fields, float) = self.spec();
        let total_bits: u32 = fields.iter().map(|&(_, w)| w as u32).sum();
        let mut word: u128 = 0;
        let mut shift = total_bits;
        for &(ch, w) in fields {
            shift -= w as u32;
            let v: f64 = match ch {
                Chan::R => rgb[0],
                Chan::G => rgb[1],
                Chan::B => rgb[2],
                Chan::A | Chan::X => 1.0,
            };
            let bits: u128 = if float {
                debug_assert_eq!(w, 16, "only fp16 float channels exist in the table");
                half::f16::from_f64(v).to_bits() as u128
            } else {
                let max = (1u128 << w) - 1;
                let q = (v.clamp(0.0, 1.0) * max as f64).round() as u128;
                q.min(max)
            };
            word |= bits << shift;
        }
        EncodedPixel {
            bytes: word.to_le_bytes(),
            len: (total_bits / 8) as usize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xrgb8888_layout_matches_legacy_packing() {
        // The old code packed 0xFF_RR_GG_BB as a LE u32.
        let px = BufferFormat::Xrgb8888.encode([1.0, 0.5, 0.0]);
        let legacy = 0xFF_FF_80_00u32.to_le_bytes();
        assert_eq!(px.as_slice(), &legacy);
    }

    #[test]
    fn xbgr16161616f_layout_matches_legacy_fp16_pixel() {
        // The old fp16_pixel wrote [R, G, B, X=1.0] as LE f16 pairs.
        let rgb = [0.5081, 1.25, -0.25]; // incl. extended-range values
        let px = BufferFormat::Xbgr16161616f.encode(rgb);
        let mut legacy = Vec::new();
        for v in [rgb[0], rgb[1], rgb[2], 1.0] {
            legacy.extend_from_slice(&half::f16::from_f64(v).to_le_bytes());
        }
        assert_eq!(px.as_slice(), &legacy[..]);
    }

    #[test]
    fn float_channels_do_not_clamp() {
        let px = BufferFormat::Xbgr16161616f.encode([2.0, -1.0, 0.0]);
        let r = half::f16::from_le_bytes([px[0], px[1]]);
        let g = half::f16::from_le_bytes([px[2], px[3]]);
        assert_eq!(r.to_f64(), 2.0);
        assert_eq!(g.to_f64(), -1.0);
    }

    #[test]
    fn unorm_channels_clamp_and_quantize() {
        let px = BufferFormat::Xrgb2101010.encode([1.5, 0.5, -0.2]);
        let word = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
        assert_eq!((word >> 20) & 0x3FF, 1023); // R clamped to max
        assert_eq!((word >> 10) & 0x3FF, 512); // G = round(0.5 * 1023)
        assert_eq!(word & 0x3FF, 0); // B clamped to 0
        assert_eq!(word >> 30, 0b11); // X written opaque
    }

    #[test]
    #[allow(clippy::unusual_byte_groupings)] // grouped 5:6:5 to mirror the fields
    fn rgb565_packs_to_spec() {
        // White = all channels at max: 0xFFFF.
        assert_eq!(
            BufferFormat::Rgb565.encode([1.0; 3]).as_slice(),
            &[0xFF, 0xFF]
        );
        // Pure green fills only the middle 6 bits.
        let px = BufferFormat::Rgb565.encode([0.0, 1.0, 0.0]);
        let word = u16::from_le_bytes([px[0], px[1]]);
        assert_eq!(word, 0b00000_111111_00000);
    }

    #[test]
    fn rgb888_is_bgr_in_memory() {
        // [23:0] R:G:B little endian ⇒ memory bytes B, G, R.
        let px = BufferFormat::Rgb888.encode([1.0, 0.0, 0.0]);
        assert_eq!(px.as_slice(), &[0x00, 0x00, 0xFF]);
    }

    #[test]
    fn alpha_is_always_opaque() {
        let px = BufferFormat::Argb8888.encode([0.0, 0.0, 0.0]);
        assert_eq!(px.as_slice(), &0xFF_00_00_00u32.to_le_bytes());
        let px = BufferFormat::Rgba8888.encode([0.0, 0.0, 0.0]);
        assert_eq!(px.as_slice(), &0x00_00_00_FFu32.to_le_bytes());
    }

    #[test]
    fn every_format_roundtrips_names_and_wl() {
        for &f in BufferFormat::ALL {
            assert_eq!(BufferFormat::from_name(f.name()), Some(f), "{}", f.name());
            assert_eq!(
                BufferFormat::from_wl(f.wl_format()),
                Some(f),
                "{}",
                f.name()
            );
            // bpp is consistent with the encoded pixel length.
            assert_eq!(
                f.encode([0.5; 3]).len(),
                f.bytes_per_pixel(),
                "{}",
                f.name()
            );
            // every format names a sane depth
            assert!(f.color_depth() >= 2, "{}", f.name());
        }
    }

    #[test]
    fn depth_reports_narrowest_color_channel() {
        assert_eq!(BufferFormat::Rgb565.color_depth(), 5);
        assert_eq!(BufferFormat::Rgb332.color_depth(), 2);
        assert_eq!(BufferFormat::Xrgb8888.color_depth(), 8);
        assert_eq!(BufferFormat::Xrgb2101010.color_depth(), 10);
        assert_eq!(BufferFormat::Xbgr16161616f.color_depth(), 16);
    }
}
