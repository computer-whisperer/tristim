//! Original Datacolor SpyderX driver (USB `085c:0a00`, the `spydX.c` protocol).
//!
//! **Untested port.** This implements the wire format reverse-engineered by
//! Graeme Gill for ArgyllCMS (`spectro/spydX.c`); we have not run it against
//! real SpyderX hardware. The framing and reset are byte-identical to the
//! hardware-validated SpyderX2/2024 driver (shared via [`transport`]), so the
//! risk is concentrated in the opcode payloads and conversion. Validation
//! reports welcome.
//!
//! ## Protocol differences from SpyderX2 / Spyder 2024
//!
//! | | SpyderX | SpyderX2 / 2024 |
//! |---|---|---|
//! | HW version | `0xD9` (23-byte reply) | folded into `0xC2` |
//! | Serial | `0xC2` (37-byte reply, bytes 4..12) | same |
//! | Calibration | `0xCB` → 42 bytes: 3×3 f32-LE matrix | `0xF6` → 108 bytes: 3×6 matrix + gain/offset |
//! | Setup | `0xC3` → 10 bytes: 4+4 u8s | `0xF7` → 22 bytes: 3×6 u8s |
//! | Measure | `0xD2` → 4 × u16 BE (X, Y, Z, IR) | `0xF2` → 6 × u16 BE |
//! | Raw channels | 4 (XYZ + infrared) | 6 |
//!
//! ## Dark calibration
//!
//! Like the X2, the firmware auto-zeros on the vendor reset issued before each
//! measurement, and the setup reply carries device-side black offsets (`s3`)
//! that are always subtracted. *Unlike* the X2, ArgyllCMS additionally
//! maintains a user-side dark offset (`bcal`) measured with the lens cap on
//! and persisted to disk with a 30-minute validity window. This port keeps
//! that offset **session-only**: it starts at zero and is set by
//! [`SpyderX::dark_calibrate`]. Without it, readings very close to black carry
//! a small uncorrected residual; bright-patch accuracy is unaffected.

use super::transport::{self, pid};
use crate::colorimeter::{CalibrationId, Colorimeter, DeviceInfo, Error, Result};
use crate::sample::{RawRepeats, Sample, Xyz};
use rusb::{Context, DeviceHandle};
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Command opcodes for the original SpyderX protocol
/// (`ArgyllCMS spectro/spydX.c`).
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Opcode {
    /// `0xD9` — hardware version. Reply: 23 bytes (`0x17`).
    ///   `[0]`     ASCII digit, major version
    ///   `[2..=3]` ASCII digits, minor version
    GetHwVersion = 0xD9,

    /// `0xC2` — serial number. Reply: 37 bytes (`0x25`), serial at `[4..12]`.
    GetSerial = 0xC2,

    /// `0xCB` — per-unit calibration for cal index `N` (0..=3).
    /// Send: 1 byte. Reply: 42 bytes (`0x2A`, checksummed).
    GetCalibration = 0xCB,

    /// `0xC3` — measurement setup for gain selector `v1`.
    /// Send: 1 byte. Reply: 10 bytes (`0x0A`, checksummed).
    ///
    /// Firmware quirk (Argyll): `v1` 0..=2 return all-`0xFF` payloads that
    /// fail the checksum; `v1 > 3` returns instrument error 1. Only the
    /// calibrated value (3) is known to work.
    MeasSetup = 0xC3,

    /// `0xD2` — take an emissive measurement.
    /// Send: 7 bytes (integration time u16 BE, gain u8, 4 trim u8s).
    /// Reply: 8 bytes — 4 × u16 BE raw counts (X, Y, Z, IR). Not checksummed.
    Measure = 0xD2,

    /// `0xD4` — ambient-light measurement (TSL25721 sensor).
    /// Send: 2 bytes (integration time, gain bits). Reply: 6 bytes.
    AmbientMeasure = 0xD4,
}

/// Number of on-board calibration slots (display-type presets).
/// Index 0 = "General" (the default), 1 = "Standard LED",
/// 2 = "Wide Gamut LED", 3 = "GB LED".
pub const NUM_CALIBRATIONS: u8 = 4;

/// Per-unit calibration downloaded with opcode `0xCB`.
#[derive(Debug, Clone)]
pub struct Calibration {
    /// Echoed calibration index (must match what we sent).
    pub index: u8,
    /// "v1" — gain selector fed to the setup command (3 = 64× on known units).
    pub v1: u8,
    /// "v2" — integration time in msec, sent verbatim in the measure command.
    /// Max 719; hardware quantizes to `2.8 * floor(v2 / 2.8)`. Calibrated
    /// value 714.
    pub v2: u16,
    /// 3×3 matrix mapping black-subtracted raw (X, Y, Z) counts → CIE XYZ.
    /// Stored row-major on the wire as 9 little-endian IEEE-754 f32s:
    /// `matrix[i][j]` multiplies raw channel `j` into XYZ component `i`.
    pub matrix: [[f64; 3]; 3],
    /// "v3" — unused magic byte (kept for diagnostics).
    pub v3: u8,
}

/// Per-measurement setup downloaded with opcode `0xC3`.
#[derive(Debug, Clone)]
pub struct Setup {
    /// Echoed gain selector (must match the calibration's `v1`).
    pub s1: u8,
    /// 4 per-channel trim values, passed through to the measure command
    /// (Argyll: "signed gain trim to an offset value?").
    pub s2: [u8; 4],
    /// 4 per-channel device-side black offsets, subtracted from raw counts
    /// before the matrix (first 3 channels; the 4th is the IR channel).
    pub s3: [u8; 4],
}

/// Raw 4-channel sensor counts from opcode `0xD2`: X, Y, Z, IR.
/// The IR channel does not participate in XYZ conversion.
#[derive(Debug, Clone, Copy)]
pub struct RawMeasurement(pub [u16; 4]);

use super::measurement::ParseError;

/// Parse a 42-byte `0xCB` calibration reply (checksum already verified by the
/// transport).
pub fn parse_calibration(payload: &[u8], expected_index: u8) -> Result<Calibration> {
    if payload.len() != 0x2A {
        return Err(ParseError::BadLength {
            expected: 0x2A,
            got: payload.len(),
        }
        .into());
    }
    let index = payload[0];
    if index != expected_index {
        return Err(ParseError::CalIndexMismatch {
            expected: expected_index,
            got: index,
        }
        .into());
    }
    let v1 = payload[1];
    let v2 = u16::from_be_bytes([payload[2], payload[3]]);

    // 3×3 matrix, row-major (i fastest in memory order: the k-th float is
    // matrix[k / 3][k % 3]), little-endian f32s at bytes 4..40.
    let mut matrix = [[0.0f64; 3]; 3];
    for k in 0..9 {
        let off = 4 + k * 4;
        let bytes: [u8; 4] = payload[off..off + 4].try_into().unwrap();
        matrix[k / 3][k % 3] = f32::from_le_bytes(bytes) as f64;
    }

    let v3 = payload[0x28];

    Ok(Calibration {
        index,
        v1,
        v2,
        matrix,
        v3,
    })
}

/// Parse a 10-byte `0xC3` setup reply (checksum already verified).
pub fn parse_setup(payload: &[u8], expected_v1: u8) -> Result<Setup> {
    if payload.len() != 0x0A {
        return Err(ParseError::BadLength {
            expected: 0x0A,
            got: payload.len(),
        }
        .into());
    }
    let s1 = payload[0];
    if s1 != expected_v1 {
        return Err(ParseError::SetupV1Mismatch {
            expected: expected_v1,
            got: s1,
        }
        .into());
    }
    let s2: [u8; 4] = payload[1..5].try_into().unwrap();
    let s3: [u8; 4] = payload[5..9].try_into().unwrap();
    Ok(Setup { s1, s2, s3 })
}

/// Parse an 8-byte `0xD2` measurement reply: 4 × u16 BE (X, Y, Z, IR).
pub fn parse_raw_measurement(payload: &[u8]) -> Result<RawMeasurement> {
    if payload.len() != 8 {
        return Err(ParseError::BadLength {
            expected: 8,
            got: payload.len(),
        }
        .into());
    }
    let mut raw = [0u16; 4];
    for (i, ch) in raw.iter_mut().enumerate() {
        *ch = u16::from_be_bytes([payload[2 * i], payload[2 * i + 1]]);
    }
    Ok(RawMeasurement(raw))
}

/// Encode the 7-byte payload for the `0xD2` measure command.
pub fn encode_measure_request(cal: &Calibration, setup: &Setup) -> [u8; 7] {
    let mut buf = [0u8; 7];
    buf[0..2].copy_from_slice(&cal.v2.to_be_bytes());
    buf[2] = setup.s1;
    buf[3..7].copy_from_slice(&setup.s2);
    buf
}

/// Convert raw counts to XYZ: subtract the device black offsets (`s3`) and the
/// session dark offsets, clamp at zero, then apply the 3×3 matrix. The IR
/// channel (`raw[3]`) is not used.
///
/// Follows `spydX_GetReading()` with one deliberate divergence: Argyll feeds
/// possibly-negative black-subtracted counts straight into the matrix, while
/// this port clamps each channel at zero first (the same convention as the
/// hardware-validated X2 path, and what the confidence layer's floor analysis
/// assumes). The difference only shows within noise of true black.
pub fn raw_to_xyz(raw: &RawMeasurement, setup: &Setup, cal: &Calibration, dark: &[f64; 3]) -> Xyz {
    let mut corrected = [0.0f64; 3];
    for i in 0..3 {
        corrected[i] = (raw.0[i] as f64 - setup.s3[i] as f64 - dark[i]).max(0.0);
    }
    let mut xyz = [0.0f64; 3];
    // Fixed-size 3×3 matrix math; explicit index loops read clearer than
    // iterator chains here (matches the X2 conversion).
    #[allow(clippy::needless_range_loop)]
    for i in 0..3 {
        xyz[i] = cal.matrix[i][0] * corrected[0]
            + cal.matrix[i][1] * corrected[1]
            + cal.matrix[i][2] * corrected[2];
    }
    Xyz {
        x: xyz[0],
        y: xyz[1],
        z: xyz[2],
    }
}

/// An opened original SpyderX. **Untested port** — see the module docs.
///
/// Holds the active calibration + setup, so trait measurements need no
/// per-call calibration argument. The setup block is re-fetched before every
/// measurement burst, mirroring ArgyllCMS (`spydX_GetReading` re-runs setup
/// per reading).
pub struct SpyderX {
    handle: DeviceHandle<Context>,
    info: DeviceInfo,
    cal: Calibration,
    setup: Setup,
    /// Session dark offsets (Argyll's `bcal`), in count units, for the three
    /// XYZ channels. Zero until [`dark_calibrate`](Self::dark_calibrate).
    dark: [f64; 3],
}

impl SpyderX {
    /// Find and open the first original SpyderX on the bus, selecting cal
    /// index 0 ("General") as the active calibration.
    pub fn open_any() -> Result<Self> {
        let (handle, usb_pid) = transport::open_first(&[pid::SPYDERX])?;

        let mut dev = Self {
            handle,
            info: DeviceInfo {
                vendor: "Datacolor".into(),
                model: "SpyderX".into(),
                serial: String::new(),
                firmware: (0, 0),
                usb_pid,
            },
            cal: Calibration {
                index: 0,
                v1: 0,
                v2: 1,
                matrix: [[0.0; 3]; 3],
                v3: 0,
            },
            setup: Setup {
                s1: 0,
                s2: [0; 4],
                s3: [0; 4],
            },
            dark: [0.0; 3],
        };

        dev.send_reset()?;
        dev.info.firmware = dev.read_hw_version()?;
        dev.info.serial = dev.read_serial()?;
        dev.select_calibration(CalibrationId(0))?;

        // ArgyllCMS does one ambient measurement at init "to initialize it";
        // mirror that, ignoring the result (and any error — it's a warm-up).
        let _ = dev.measure_ambient_raw(101, 0x10);

        Ok(dev)
    }

    /// Vendor-class reset; doubles as the auto-zero trigger before
    /// measurements. Identical request to the X2/2024.
    pub fn send_reset(&self) -> Result<()> {
        transport::send_reset(&self.handle)
    }

    /// Execute one command against the device. See
    /// [`transport`](super::transport) for the wire format.
    pub fn command(
        &mut self,
        opcode: Opcode,
        send_payload: &[u8],
        reply_size: usize,
        verify_checksum: bool,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        transport::command(
            &self.handle,
            opcode as u8,
            send_payload,
            reply_size,
            verify_checksum,
            timeout,
        )
    }

    /// Hardware version from opcode `0xD9` as `(major, minor)`.
    fn read_hw_version(&mut self) -> Result<(u32, u32)> {
        let reply = self.command(Opcode::GetHwVersion, &[], 0x17, false, DEFAULT_TIMEOUT)?;
        let major =
            ascii_digits(&reply[0..1]).ok_or_else(|| Error::BadVersionString(reply.clone()))?;
        let minor =
            ascii_digits(&reply[2..4]).ok_or_else(|| Error::BadVersionString(reply.clone()))?;
        Ok((major, minor))
    }

    /// Serial number from opcode `0xC2` (bytes 4..12 of the reply).
    fn read_serial(&mut self) -> Result<String> {
        let reply = self.command(Opcode::GetSerial, &[], 0x25, false, DEFAULT_TIMEOUT)?;
        Ok(std::str::from_utf8(&reply[4..12])
            .unwrap_or("")
            .trim_end_matches('\0')
            .trim()
            .to_string())
    }

    /// Download the per-unit calibration for `cal_index` (0..=3). Out-of-range
    /// indices are refused host-side, mirroring ArgyllCMS — what the firmware
    /// does with them is unknown.
    pub fn get_calibration(&mut self, cal_index: u8) -> Result<Calibration> {
        if cal_index >= NUM_CALIBRATIONS {
            return Err(Error::CalIndexOutOfRange {
                got: cal_index,
                max: NUM_CALIBRATIONS - 1,
            });
        }
        let reply = self.command(
            Opcode::GetCalibration,
            &[cal_index],
            0x2A,
            true, // checksummed
            DEFAULT_TIMEOUT,
        )?;
        parse_calibration(&reply, cal_index)
    }

    /// Fetch the measurement setup for the given calibration's gain selector.
    pub fn get_setup(&mut self, cal: &Calibration) -> Result<Setup> {
        let reply = self.command(
            Opcode::MeasSetup,
            &[cal.v1],
            0x0A,
            true, // checksummed
            DEFAULT_TIMEOUT,
        )?;
        parse_setup(&reply, cal.v1)
    }

    /// One raw 4-channel reading, auto-zeroing first. ArgyllCMS resets before
    /// every measurement ("to trigger an auto-zero?"), and this port keeps
    /// that discipline unconditionally — burst-without-reset behavior is
    /// unverified on this hardware.
    pub fn measure_raw_once(&mut self) -> Result<RawMeasurement> {
        self.send_reset()?;
        let send = encode_measure_request(&self.cal, &self.setup);
        // Integration time alone can be ~714 ms; Argyll allows 5 s and on
        // untested hardware we mirror it exactly.
        let reply = self.command(Opcode::Measure, &send, 8, false, DEFAULT_TIMEOUT)?;
        parse_raw_measurement(&reply)
    }

    /// Raw ambient reading (opcode `0xD4`): wideband and IR counts from the
    /// TSL25721 ambient sensor, plus the echoed parameters.
    pub fn measure_ambient_raw(&mut self, integration: u8, gain_bits: u8) -> Result<[u16; 2]> {
        let reply = self.command(
            Opcode::AmbientMeasure,
            &[integration, gain_bits],
            6,
            false,
            DEFAULT_TIMEOUT,
        )?;
        Ok([
            u16::from_be_bytes([reply[0], reply[1]]),
            u16::from_be_bytes([reply[2], reply[3]]),
        ])
    }

    /// Measure the user-side dark offset with the **lens cap on** (or the
    /// puck face-down on a non-emissive surface) and make it the session dark
    /// reference, mirroring ArgyllCMS's black calibration. Returns the new
    /// offsets in count units.
    ///
    /// The offset is session-only — it is not persisted, and it starts at
    /// zero on open. Skipping it leaves a small uncorrected residual on
    /// readings very close to black. Also re-fetches the measurement setup
    /// (so the offset pairs with current device-side black values), updating
    /// the active setup as a side effect.
    pub fn dark_calibrate(&mut self) -> Result<[f64; 3]> {
        self.setup = self.get_setup(&self.cal.clone())?;
        let raw = self.measure_raw_once()?;
        for i in 0..3 {
            self.dark[i] = raw.0[i] as f64 - self.setup.s3[i] as f64;
        }
        Ok(self.dark)
    }

    /// The current session dark offsets (count units; zero until
    /// [`dark_calibrate`](Self::dark_calibrate)).
    pub fn dark_offsets(&self) -> [f64; 3] {
        self.dark
    }
}

/// Build a device-agnostic [`Sample`] from raw readings. Only the three XYZ
/// channels enter [`RawRepeats`] — the IR channel does not contribute to XYZ
/// (zero gradient), and including it would distort the confidence layer's
/// signal-channel floor analysis. Device-aware tooling that wants the IR
/// counts can use [`SpyderX::measure_raw_once`] directly.
fn raws_to_sample(
    raws: &[RawMeasurement],
    setup: &Setup,
    cal: &Calibration,
    dark: &[f64; 3],
) -> Sample {
    let counts: Vec<Vec<u32>> = raws
        .iter()
        .map(|r| r.0[..3].iter().map(|&c| c as u32).collect())
        .collect();
    let floor: Vec<f64> = (0..3).map(|i| setup.s3[i] as f64 + dark[i]).collect();
    // ∂XYZ/∂count for channel j is the j-th matrix column.
    let grad: Vec<[f64; 3]> = (0..3)
        .map(|j| [cal.matrix[0][j], cal.matrix[1][j], cal.matrix[2][j]])
        .collect();
    let xyz: Vec<Xyz> = raws
        .iter()
        .map(|r| raw_to_xyz(r, setup, cal, dark))
        .collect();
    Sample {
        xyz,
        raw: Some(RawRepeats {
            counts,
            floor,
            grad,
        }),
    }
}

impl Colorimeter for SpyderX {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn select_calibration(&mut self, id: CalibrationId) -> Result<()> {
        let cal = self.get_calibration(id.0)?;
        let setup = self.get_setup(&cal)?;
        self.cal = cal;
        self.setup = setup;
        Ok(())
    }

    fn measure(&mut self, n: usize) -> Result<Sample> {
        // Re-fetch the setup per burst, mirroring Argyll's per-reading setup.
        self.setup = self.get_setup(&self.cal.clone())?;
        let mut raws = Vec::with_capacity(n);
        for _ in 0..n {
            raws.push(self.measure_raw_once()?);
        }
        Ok(raws_to_sample(&raws, &self.setup, &self.cal, &self.dark))
    }

    // measure_adaptive: trait default (single full burst). The X2's fast tier
    // rides on a hardware-characterized integration override; the SpyderX
    // equivalent (scaling the matrix by v2 ratio) is plausible but unverified,
    // so this port doesn't offer it. Same reasoning for raw_diagnostics.
}

fn ascii_digits(bytes: &[u8]) -> Option<u32> {
    let trimmed: Vec<u8> = bytes
        .iter()
        .copied()
        .take_while(|b| b.is_ascii_digit())
        .collect();
    if trimmed.is_empty() {
        return None;
    }
    std::str::from_utf8(&trimmed).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a calibration reply payload: index, v1, v2, 9 f32-LE matrix
    /// entries (row-major), v3, checksum.
    fn cal_payload(index: u8, v1: u8, v2: u16, matrix: [[f32; 3]; 3], v3: u8) -> Vec<u8> {
        let mut p = vec![0u8; 0x2A];
        p[0] = index;
        p[1] = v1;
        p[2..4].copy_from_slice(&v2.to_be_bytes());
        for k in 0..9 {
            p[4 + k * 4..8 + k * 4].copy_from_slice(&matrix[k / 3][k % 3].to_le_bytes());
        }
        p[0x28] = v3;
        let sum: u8 = p[..0x29].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        p[0x29] = sum;
        p
    }

    #[test]
    fn calibration_parses_row_major() {
        let m = [[1.0f32, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]];
        let p = cal_payload(2, 3, 714, m, 0xAB);
        let cal = parse_calibration(&p, 2).unwrap();
        assert_eq!(cal.v1, 3);
        assert_eq!(cal.v2, 714);
        assert_eq!(cal.v3, 0xAB);
        // Row-major: the second wire float is matrix[0][1], not matrix[1][0].
        assert_eq!(cal.matrix[0][1], 2.0);
        assert_eq!(cal.matrix[1][0], 4.0);
        assert_eq!(cal.matrix[2][2], 9.0);
    }

    #[test]
    fn calibration_rejects_index_mismatch() {
        let p = cal_payload(1, 3, 714, [[0.0; 3]; 3], 0);
        assert!(parse_calibration(&p, 0).is_err());
    }

    #[test]
    fn setup_parses_and_checks_v1() {
        let p = [3u8, 10, 11, 12, 13, 1, 2, 3, 4, 0];
        let s = parse_setup(&p, 3).unwrap();
        assert_eq!(s.s2, [10, 11, 12, 13]);
        assert_eq!(s.s3, [1, 2, 3, 4]);
        assert!(parse_setup(&p, 2).is_err());
    }

    #[test]
    fn measure_request_layout() {
        let cal = Calibration {
            index: 0,
            v1: 3,
            v2: 714,
            matrix: [[0.0; 3]; 3],
            v3: 0,
        };
        let setup = Setup {
            s1: 3,
            s2: [9, 8, 7, 6],
            s3: [0; 4],
        };
        let req = encode_measure_request(&cal, &setup);
        assert_eq!(req, [0x02, 0xCA, 3, 9, 8, 7, 6]); // 714 = 0x02CA BE
    }

    #[test]
    fn raw_measurement_is_u16_be() {
        let p = [0x01, 0x00, 0x00, 0x02, 0x12, 0x34, 0xFF, 0xFF];
        let r = parse_raw_measurement(&p).unwrap();
        assert_eq!(r.0, [256, 2, 0x1234, 65535]);
    }

    #[test]
    fn conversion_subtracts_black_and_ignores_ir() {
        let cal = Calibration {
            index: 0,
            v1: 3,
            v2: 714,
            matrix: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            v3: 0,
        };
        let setup = Setup {
            s1: 3,
            s2: [0; 4],
            s3: [2, 2, 2, 2],
        };
        let dark = [1.0, 1.0, 1.0];
        // IR channel huge: must not affect XYZ.
        let raw = RawMeasurement([103, 53, 3, 60000]);
        let xyz = raw_to_xyz(&raw, &setup, &cal, &dark);
        assert_eq!(xyz.x, 100.0);
        assert_eq!(xyz.y, 50.0);
        assert_eq!(xyz.z, 0.0); // 3 - 2 - 1 = 0, clamped at 0
    }

    #[test]
    fn sample_carries_three_channels_with_matrix_column_gradients() {
        let cal = Calibration {
            index: 0,
            v1: 3,
            v2: 714,
            matrix: [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]],
            v3: 0,
        };
        let setup = Setup {
            s1: 3,
            s2: [0; 4],
            s3: [1, 1, 1, 1],
        };
        let dark = [0.5, 0.5, 0.5];
        let raws = [RawMeasurement([10, 20, 30, 40])];
        let s = raws_to_sample(&raws, &setup, &cal, &dark);
        let rr = s.raw.unwrap();
        assert_eq!(rr.channels(), 3);
        assert_eq!(rr.counts[0], vec![10, 20, 30]); // IR dropped
        assert_eq!(rr.floor, vec![1.5, 1.5, 1.5]);
        // Gradient of channel 0 is the first matrix *column*.
        assert_eq!(rr.grad[0], [1.0, 4.0, 7.0]);
        assert_eq!(rr.grad[2], [3.0, 6.0, 9.0]);
    }
}
