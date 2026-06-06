//! Device-generic colorimeter driver layer for tristim.
//!
//! The public surface is the [`Colorimeter`] trait and the device-agnostic
//! measurement types ([`Sample`], [`Xyz`], [`MeasurementConfidence`]). Open a
//! device with [`open_any`]; everything above this crate talks to the trait and
//! never names a concrete instrument.
//!
//! The implemented drivers are the Datacolor [`spyder`] family: SpyderX2 /
//! Spyder 2024 ([`Spyder`], hardware-validated) and the original SpyderX
//! ([`SpyderX`], an untested port). Both wire protocols were
//! reverse-engineered by Graeme Gill for ArgyllCMS (`spectro/spydX2.c` /
//! `spectro/spydX.c`) and are re-implemented clean-room here from the
//! documented wire format. Calibration mechanics live behind the trait in
//! [`spyder`]; device-aware tooling (the examples) can reach them directly.

pub mod colorimeter;
pub mod confidence;
pub mod sample;
pub mod spyder;

pub use colorimeter::{
    AdaptiveMeasurement, AdaptiveTier, CalibrationId, Colorimeter, DeviceInfo, Error,
    RawDiagnostics, ResetDiscipline, Result, open_any,
};
pub use confidence::{MeasurementConfidence, RawStats, TrustFlag};
pub use sample::{RawRepeats, Sample, Xyz};
pub use spyder::Spyder;
pub use spyder::spyderx::SpyderX;
