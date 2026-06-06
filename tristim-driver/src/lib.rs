//! Device-generic colorimeter driver layer for tristim.
//!
//! The public surface is the [`Colorimeter`] trait and the device-agnostic
//! measurement types ([`Sample`], [`Xyz`], [`MeasurementConfidence`]). Open a
//! device with [`open_any`]; everything above this crate talks to the trait and
//! never names a concrete instrument.
//!
//! The implemented drivers: the Datacolor [`spyder`] family — SpyderX2 /
//! Spyder 2024 ([`Spyder`], hardware-validated) and the original SpyderX
//! ([`SpyderX`], an untested port) — and the X-Rite [`i1d3`] family —
//! i1Display Pro / ColorMunki Display and OEM rebadges ([`I1d3`], an
//! untested port). All wire protocols were reverse-engineered by Graeme
//! Gill for ArgyllCMS (`spectro/spydX2.c`, `spectro/spydX.c`,
//! `spectro/i1d3.c`) and are re-implemented clean-room here from the
//! documented wire formats. Calibration mechanics live behind the trait in
//! each driver module; device-aware tooling (the examples) can reach them
//! directly.

pub mod colorimeter;
pub mod confidence;
pub mod i1d3;
pub mod sample;
pub mod spyder;

pub use colorimeter::{
    AdaptiveMeasurement, AdaptiveTier, CalibrationId, Colorimeter, DeviceInfo, Error,
    RawDiagnostics, ResetDiscipline, Result, open_any,
};
pub use confidence::{MeasurementConfidence, RawStats, TrustFlag};
pub use i1d3::I1d3;
pub use sample::{RawRepeats, Sample, Xyz};
pub use spyder::Spyder;
pub use spyder::spyderx::SpyderX;
