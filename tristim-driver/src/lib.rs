//! Driver for Datacolor Spyder-family colorimeters.
//!
//! Currently scoped to the SpyderX-family wire protocol (PIDs in `0x0A0X` range).
//! Hardware tested against SpyderExpress 2024 (PID `0x0A0B`).
//!
//! Protocol reverse-engineered by Graeme Gill for ArgyllCMS (`spectro/spydX.c`);
//! this is a clean-room Rust re-implementation working from the documented
//! wire format, not a code translation.

pub mod confidence;
pub mod device;
pub mod measurement;
pub mod protocol;

pub use confidence::{MeasurementConfidence, TrustFlag};
pub use device::{Colorimeter, DeviceInfo};
pub use measurement::{
    AdaptiveMeasurement, AdaptiveTier, Calibration, IntegrationError, MIN_INTEGRATION_MS,
    RawMeasurement, Setup, Xyz, override_integration,
};
pub use protocol::{DATACOLOR_VID, Opcode};
