//! Device-generic colorimeter driver layer for tristim.
//!
//! The public surface is the [`Colorimeter`] trait and the device-agnostic
//! measurement types ([`Sample`], [`Xyz`], [`MeasurementConfidence`]). Open a
//! device with [`open_any`]; everything above this crate talks to the trait and
//! never names a concrete instrument.
//!
//! The only driver implemented today is the Datacolor [`spyder`] family
//! (SpyderX2 / Spyder 2024), reverse-engineered by Graeme Gill for ArgyllCMS
//! (`spectro/spydX2.c`) and re-implemented clean-room here from the documented
//! wire format. Its calibration mechanics live behind the trait in
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
