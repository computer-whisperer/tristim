//! Device-generic colorimeter abstraction.
//!
//! [`Colorimeter`] is the capability surface the rest of tristim talks to —
//! capture orchestration never names a concrete device. A driver (currently
//! only [`Spyder`](crate::spyder::Spyder)) implements it; [`open_any`] probes
//! the bus and hands back a `Box<dyn Colorimeter>`.
//!
//! The currency is [`Sample`] (absolute XYZ + optional raw counts) — see
//! [`crate::sample`]. Device-specific calibration mechanics (matrices, channel
//! maps, integration knobs) stay behind the trait.

use crate::sample::Sample;
use crate::spyder::measurement::ParseError;
use thiserror::Error;

/// Errors any driver in this crate can surface. Several variants are specific to
/// the Spyder wire protocol; they live here so the trait can expose a single
/// `Result` type rather than an associated error per device.
#[derive(Debug, Error)]
pub enum Error {
    #[error("USB I/O: {0}")]
    Usb(#[from] rusb::Error),

    #[error("no colorimeter found (looked for VID 0x{0:04x})")]
    NotFound(u16),

    #[error("short write: sent {sent}, expected {expected}")]
    ShortWrite { sent: usize, expected: usize },

    #[error("short read: got {got}, expected {expected}")]
    ShortRead { got: usize, expected: usize },

    #[error("nonce mismatch: sent 0x{sent:04x}, got 0x{got:04x}")]
    NonceMismatch { sent: u16, got: u16 },

    #[error("instrument-reported error code 0x{0:02x}")]
    InstrumentError(u8),

    #[error("payload length mismatch: device reported {reported}, expected {expected}")]
    PayloadLenMismatch { reported: usize, expected: usize },

    #[error("checksum mismatch: computed 0x{computed:02x}, device sent 0x{advertised:02x}")]
    ChecksumMismatch { computed: u8, advertised: u8 },

    #[error("device sent unparseable hardware-version string: {0:?}")]
    BadVersionString(Vec<u8>),

    #[error("measurement reply parse: {0}")]
    Parse(#[from] ParseError),

    #[error("integration time {got} ms out of range [{min}, {max}]")]
    IntegrationOutOfRange { got: u16, min: u16, max: u16 },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Vendor- and model-neutral device identity. Carries the provenance facts the
/// capture file records (`usb_pid`, `firmware`) so the capture contract is
/// device-agnostic at the type level; a non-USB instrument simply reports
/// `usb_pid = 0`. Device-specific capability detail stays on the concrete driver.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Manufacturer, e.g. `"Datacolor"`.
    pub vendor: String,
    /// Human-facing model, e.g. `"Spyder 2024"`.
    pub model: String,
    /// Serial number, trimmed.
    pub serial: String,
    /// Firmware/hardware version as `(major, minor)`.
    pub firmware: (u32, u32),
    /// USB product ID, or `0` for non-USB devices.
    pub usb_pid: u16,
}

/// Opaque handle to one of a device's on-board calibration modes (display-type
/// presets / cal indices). Interpreted by the driver that issued it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalibrationId(pub u8);

/// Which adaptive tier produced an [`AdaptiveMeasurement`] — for telemetry and
/// event reporting.
///
/// Non-exhaustive: future drivers may add tiers (treat unknown tiers as
/// "measurement is valid, provenance unfamiliar").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdaptiveTier {
    /// Adaptive disabled (or the requested fast exposure was unavailable): a
    /// single default-exposure burst was taken.
    SingleFull,
    /// The fast measurement passed the trust check and is what's returned.
    Fast,
    /// The fast measurement was untrustworthy; the returned data is the
    /// default-exposure re-measurement that followed.
    EscalatedFull,
}

/// Result of [`Colorimeter::measure_adaptive`]: the repeats that were kept,
/// already converted to absolute XYZ, plus which tier produced them.
#[derive(Debug, Clone)]
pub struct AdaptiveMeasurement {
    pub sample: Sample,
    pub tier: AdaptiveTier,
}

/// Reset discipline for a burst of raw readings (the `auto_zero` choice,
/// generalized). [`AutoZeroEach`](ResetDiscipline::AutoZeroEach) re-zeros the
/// dark baseline before every reading; [`BurstOnce`](ResetDiscipline::BurstOnce)
/// zeros once and reads back-to-back.
///
/// Non-exhaustive: future devices may expose other zeroing disciplines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResetDiscipline {
    AutoZeroEach,
    BurstOnce,
}

/// The capability surface every colorimeter driver exposes.
///
/// A driver holds its own active-calibration state: [`open_any`] selects a
/// sensible default, [`select_calibration`](Colorimeter::select_calibration)
/// changes it, and [`measure`](Colorimeter::measure) uses whatever is current.
pub trait Colorimeter {
    /// Vendor/model/serial/firmware identity.
    fn info(&self) -> &DeviceInfo;

    /// Make `id` the active calibration for subsequent measurements.
    fn select_calibration(&mut self, id: CalibrationId) -> Result<()>;

    /// Take `n` repeats at the active calibration, returning absolute XYZ (and
    /// raw counts when the device exposes them). Uses the device's conservative
    /// default discipline (re-zero per reading where applicable).
    fn measure(&mut self, n: usize) -> Result<Sample>;

    /// Adaptive measurement: when `fast_ms` names a shorter in-range exposure,
    /// a fast burst is taken first and kept if trustworthy, otherwise a
    /// default-exposure burst is taken and returned. The default implementation
    /// ignores `fast_ms` and takes a single default-exposure burst — correct for
    /// devices with no exposure knob. Drivers with one override this with their
    /// own timing behavior.
    fn measure_adaptive(
        &mut self,
        repeats: usize,
        fast_ms: Option<u16>,
    ) -> Result<AdaptiveMeasurement> {
        let _ = fast_ms;
        let sample = self.measure(repeats)?;
        Ok(AdaptiveMeasurement {
            sample,
            tier: AdaptiveTier::SingleFull,
        })
    }

    /// Raw-counts diagnostics, when supported. `None` for devices that only
    /// expose XYZ. See [`RawDiagnostics`].
    fn raw_diagnostics(&mut self) -> Option<&mut dyn RawDiagnostics> {
        None
    }
}

/// Low-level raw-counts access, for the CLI's characterization/speed/exposure
/// diagnostics. Optional per device — fetched via
/// [`Colorimeter::raw_diagnostics`]. Every method works at the device's active
/// calibration; returned [`Sample`]s carry [`RawRepeats`](crate::sample::RawRepeats).
pub trait RawDiagnostics {
    /// Re-zero the dark baseline (auto-zero).
    fn reset(&mut self) -> Result<()>;

    /// Take `n` raw readings under the given reset `discipline`, returning the
    /// sample and the per-reading wall times in milliseconds.
    fn measure_raw(&mut self, n: usize, discipline: ResetDiscipline) -> Result<(Sample, Vec<f64>)>;

    /// Same as [`measure_raw`](Self::measure_raw) but at an overridden exposure.
    /// Errors with [`Error::IntegrationOutOfRange`] if `integration_ms` is
    /// outside [`integration_range`](Self::integration_range).
    fn measure_raw_at(
        &mut self,
        n: usize,
        integration_ms: u16,
        discipline: ResetDiscipline,
    ) -> Result<(Sample, Vec<f64>)>;

    /// Inclusive `(min_ms, max_ms)` exposure range, or `None` if fixed.
    fn integration_range(&self) -> Option<(u16, u16)>;
}

/// Probe the bus and open the first supported colorimeter, with a sensible
/// default calibration already selected.
pub fn open_any() -> Result<Box<dyn Colorimeter>> {
    let spyder = crate::spyder::Spyder::open_any()?;
    Ok(Box::new(spyder))
}
