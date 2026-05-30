//! Device-agnostic measurement types.
//!
//! These are the currency between a [`Colorimeter`](crate::device::Colorimeter)
//! and everything above it (capture orchestration, confidence, analysis). They
//! carry no device-specific calibration form — a [`Sample`] is *already* in
//! absolute CIE XYZ, optionally accompanied by the raw sensor counts that
//! produced it when the device exposes them.

/// CIE XYZ tristimulus values. Units depend on calibration choice — for an
/// emissive display cal, Y is approximately luminance in cd/m² when the device
/// is held against an active display.
#[derive(Debug, Clone, Copy)]
pub struct Xyz {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Xyz {
    /// CIE 1931 xy chromaticity coordinates from this XYZ.
    /// Returns `None` if `X + Y + Z == 0` (pure black).
    pub fn chromaticity(&self) -> Option<(f64, f64)> {
        let sum = self.x + self.y + self.z;
        if sum <= 0.0 {
            return None;
        }
        Some((self.x / sum, self.y / sum))
    }

    /// CIE 1976 u'v' chromaticity coordinates from this XYZ. These are the
    /// (more perceptually uniform) coordinates Δu'v' is measured in.
    /// Returns `None` if `X + 15Y + 3Z <= 0` (pure black).
    pub fn uv_prime(&self) -> Option<(f64, f64)> {
        let d = self.x + 15.0 * self.y + 3.0 * self.z;
        if d <= 0.0 {
            return None;
        }
        Some((4.0 * self.x / d, 9.0 * self.y / d))
    }
}

/// Raw per-channel sensor data behind a [`Sample`], present only for devices
/// that expose integer counts (e.g. the Spyder family). Devices that return
/// XYZ directly leave [`Sample::raw`] as `None`.
///
/// The channel count is variable — different instruments have different filter
/// sets — so every field is indexed `[channel]` over the same `channels` width.
/// Counts being integers is what makes quantization analysis meaningful; a
/// device that exposed pre-scaled floats would not populate this.
#[derive(Debug, Clone)]
pub struct RawRepeats {
    /// Integer sensor counts, `counts[repeat][channel]`.
    pub counts: Vec<Vec<u32>>,
    /// Per-channel black-cal floor (count units), subtracted before conversion.
    pub floor: Vec<f64>,
    /// Per-channel XYZ gradient `∂XYZ/∂count`: how one count on this channel
    /// moves the output. Lets the confidence layer estimate the quantization
    /// floor without knowing the device's calibration form. `grad[channel]`.
    pub grad: Vec<[f64; 3]>,
}

impl RawRepeats {
    /// Number of channels (the common width of every per-channel field).
    pub fn channels(&self) -> usize {
        self.floor.len()
    }

    /// Number of repeats.
    pub fn repeats(&self) -> usize {
        self.counts.len()
    }
}

/// A set of repeated measurements taken at one fixed operating point. `xyz` is
/// the device-computed absolute XYZ, one entry per repeat; `raw`, when present,
/// is the integer counts that produced them (for floor / quantization analysis).
#[derive(Debug, Clone)]
pub struct Sample {
    /// Absolute XYZ, one per repeat.
    pub xyz: Vec<Xyz>,
    /// Raw sensor counts, when the device exposes them.
    pub raw: Option<RawRepeats>,
}

impl Sample {
    /// Number of repeats in this sample.
    pub fn repeats(&self) -> usize {
        self.xyz.len()
    }
}
