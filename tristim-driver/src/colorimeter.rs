//! Device-generic colorimeter abstraction.
//!
//! [`Colorimeter`] is the capability surface the rest of tristim talks to —
//! capture orchestration never names a concrete device. The drivers
//! ([`Spyder`](crate::spyder::Spyder), [`SpyderX`](crate::spyder::spyderx::SpyderX),
//! [`I1d3`](crate::i1d3::I1d3)) implement it; [`open_any`] probes the bus
//! and hands back a `Box<dyn Colorimeter>`.
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

    #[error(
        "no colorimeter found on the USB bus (supported vendors: Datacolor 085c, X-Rite 0765) — is the instrument plugged in?"
    )]
    NoDevice,

    #[error(
        "found a {vendor} device ({vid:04x}:{pid:04x}), but this model is not supported by this crate"
    )]
    UnsupportedModel {
        vendor: &'static str,
        vid: u16,
        pid: u16,
    },

    #[error(
        "USB permission denied opening colorimeter {vid:04x}:{pid:04x} — grant access with a udev rule (see the tristim-driver README) and replug the instrument"
    )]
    AccessDenied { vid: u16, pid: u16 },

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

    #[error("calibration index {got} out of range (max {max})")]
    CalIndexOutOfRange { got: u8, max: u8 },

    #[error("reply does not echo the command: sent 0x{sent:02x}, got 0x{got:02x}")]
    CommandEchoMismatch { sent: u8, got: u8 },

    #[error("instrument refused every known unlock key")]
    UnlockFailed,

    #[error("sensor saturated — light source too bright for this instrument")]
    Saturated,

    #[error("ambient diffuser is over the sensor — swing it aside for display measurement")]
    DiffuserInPath,

    #[error("per-unit calibration data is unusable (degenerate sensor spectra)")]
    BadCalibration,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Map a USB error raised while opening/claiming a specific device:
    /// permission failures become [`Error::AccessDenied`] so front ends can
    /// point the user at udev setup instead of a bare "Access denied".
    pub(crate) fn at_open(e: rusb::Error, vid: u16, pid: u16) -> Self {
        match e {
            rusb::Error::Access => Error::AccessDenied { vid, pid },
            other => Error::Usb(other),
        }
    }
}

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

/// One on-board calibration slot, as enumerated by
/// [`Colorimeter::calibrations`]: the id to pass to
/// [`select_calibration`](Colorimeter::select_calibration) plus the vendor's
/// display-type name for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationDesc {
    pub id: CalibrationId,
    /// Vendor's name for the preset, e.g. `"Wide Gamut LED"`.
    pub name: String,
}

/// How a device maps raw channel readings to CIE XYZ at its active
/// calibration: `xyz = matrix · max(raw − black_floor, 0)`, then
/// `xyz[i] = xyz[i] · gain[i] + offset[i]`.
///
/// The channel count `N` (= `black_floor.len()` = each matrix row's length)
/// and the channel units are device-specific: 6 sensor counts on the Spyder
/// X2/2024, 3 on the original SpyderX (IR excluded, matching
/// [`RawRepeats`](crate::sample::RawRepeats)), 3 internal frequencies in Hz
/// on the i1d3 family (which exposes no raw counts — its conversion is
/// reported for provenance, not recomputation). Capture tooling records this
/// so stored raw counts can be re-converted and audited offline.
#[derive(Debug, Clone, PartialEq)]
pub struct RawConversion {
    /// Per-channel floor subtracted from raw readings before the matrix.
    pub black_floor: Vec<f64>,
    /// 3×N matrix taking floor-subtracted channels to (pre-gain) XYZ.
    pub matrix: [Vec<f64>; 3],
    /// Per-row gain applied after the matrix (`[1, 1, 1]` when none).
    pub gain: [f64; 3],
    /// Per-row offset added last (`[0, 0, 0]` when none).
    pub offset: [f64; 3],
}

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

    /// The device's on-board calibration slots (display-type presets), in
    /// index order — what [`select_calibration`](Self::select_calibration)
    /// accepts. The default implementation reports a single slot named
    /// `"Native"`, correct for devices with one fixed calibration.
    fn calibrations(&self) -> Vec<CalibrationDesc> {
        vec![CalibrationDesc {
            id: CalibrationId(0),
            name: "Native".to_string(),
        }]
    }

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

    /// The raw→XYZ conversion behind the active calibration, when the device
    /// exposes it. See [`RawConversion`]; the default reports nothing.
    fn raw_conversion(&self) -> Option<RawConversion> {
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
/// default calibration already selected. Hardware-validated devices are tried
/// before untested ports.
///
/// When nothing opens, the error distinguishes the three first-run failure
/// modes: a device present but lacking permissions ([`Error::AccessDenied`] —
/// udev rule missing), a known vendor's device of an unsupported model
/// ([`Error::UnsupportedModel`]), and an empty bus ([`Error::NoDevice`]).
pub fn open_any() -> Result<Box<dyn Colorimeter>> {
    match crate::spyder::Spyder::open_any() {
        Ok(spyder) => return Ok(Box::new(spyder)),
        Err(Error::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    match crate::spyder::spyderx::SpyderX::open_any() {
        Ok(spyderx) => return Ok(Box::new(spyderx)),
        Err(Error::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    match crate::i1d3::I1d3::open_any() {
        Ok(i1d3) => return Ok(Box::new(i1d3)),
        Err(Error::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    Err(diagnose_bus())
}

/// No family's scan matched: one more enumeration pass to tell "a known
/// vendor's device is plugged in, but it's a model we don't drive" apart from
/// "no instrument on the bus at all". A supported model can't reach here —
/// its family scan would have tried to open it and propagated that result.
fn diagnose_bus() -> Error {
    use rusb::UsbContext;
    let Ok(ctx) = rusb::Context::new() else {
        return Error::NoDevice;
    };
    let Ok(devices) = ctx.devices() else {
        return Error::NoDevice;
    };
    for device in devices.iter() {
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };
        let (vid, pid) = (desc.vendor_id(), desc.product_id());
        let vendor = match vid {
            crate::spyder::transport::DATACOLOR_VID => "Datacolor",
            crate::i1d3::XRITE_VID => "X-Rite",
            _ => continue,
        };
        return Error::UnsupportedModel { vendor, vid, pid };
    }
    Error::NoDevice
}
