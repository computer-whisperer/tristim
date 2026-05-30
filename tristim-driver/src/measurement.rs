//! Calibration data, measurement setup, and raw-to-XYZ conversion for the
//! SpyderX2 / Spyder 2024 low-level measurement flow.
//!
//! The flow (mirrors `spydX2_GetReading()` in ArgyllCMS):
//!
//! 1. `get_calibration(cal_index)` (opcode `0xF6`) — per-unit calibration
//!    matrix and gain/offset, downloaded once per cal index.
//! 2. `get_setup(v1)` (opcode `0xF7`) — per-calibration measurement-time
//!    parameters (integration time, channel maps, black-cal).
//! 3. (reset) + `measure(setup)` (opcode `0xF2`) — 6 raw u16 sensor counts.
//! 4. Subtract black-cal (`raw[i] -= s5[i]`), clamp to 0.
//! 5. Multiply by the 3×6 calibration matrix to get XYZ.
//! 6. Apply per-channel gain and offset.

/// Per-unit calibration data downloaded from the device with opcode `0xF6`.
///
/// The device carries up to 7 calibration sets (one per display-technology
/// preset). For now we only read index 0 ("General" / fallback).
#[derive(Debug, Clone)]
pub struct Calibration {
    /// Echoed calibration index (we sent it as the send byte; device echoes
    /// it as reply byte 0 — must match).
    pub index: u8,

    /// "v1" — 8-bit value fed to the setup command (acts as gain selector).
    pub v1: u8,

    /// "v2" — 16-bit integration time, units of msec. Capped at 719.
    /// Sent verbatim as part of the measure command.
    pub v2: u16,

    /// "v4" — 6 channel-index bytes. Sent in the measure setup; meaning
    /// is per-channel routing inside the sensor ASIC.
    pub v4: [u8; 6],

    /// 3×6 calibration matrix mapping (6 raw sensor counts) → (XYZ).
    /// Stored on the device as 18 little-endian IEEE-754 f32s.
    /// `matrix[i][j]` is the entry that multiplies raw channel `j` into XYZ
    /// component `i` (0=X, 1=Y, 2=Z).
    pub matrix: [[f64; 6]; 3],

    /// Per-channel post-matrix gain (multiplicative).
    pub gain: [f64; 3],

    /// Per-channel post-matrix offset (additive after gain).
    pub offset: [f64; 3],

    /// "v3" — unused magic byte (we store it for diagnostics).
    pub v3: u8,
}

/// Per-calibration measurement-time setup, downloaded with opcode `0xF7`.
#[derive(Debug, Clone)]
pub struct Setup {
    /// Echoed v1 (must match the v1 from [`Calibration`]).
    pub s1: u8,

    /// Integration time in milliseconds (16-bit BE). Empirically linear with
    /// wall time and raw count magnitude across `[10, cal.v2]` on Spyder 2024;
    /// above `cal.v2` (the device's calibrated default) the firmware silently
    /// misbehaves — both wall time and raw counts go nonsensical. So `cal.v2`
    /// is the practical ceiling, even though the field type is `u16`. Argyll
    /// keeps this distinct from `cal.v2` and treats it as opaque; for the
    /// SpyderX2 / Spyder 2024 lineup the device returns it equal to `cal.v2`
    /// and we now use it as the integration knob (see
    /// [`override_integration`]).
    pub s2: u16,

    /// 6 channel-index bytes (typically pass-through to measure cmd).
    pub s3: [u8; 6],

    /// 6 per-channel values (Argyll comment: "typically 0xbf, 0x9f or similar").
    pub s4: [u8; 6],

    /// 6 per-channel black-calibration / sensor-zero values, subtracted from
    /// raw counts before the matrix multiply.
    pub s5: [u8; 6],
}

/// Raw 6-channel sensor counts returned by opcode `0xF2`.
#[derive(Debug, Clone, Copy)]
pub struct RawMeasurement(pub [u16; 6]);

// `Xyz` is device-agnostic and lives in [`crate::sample`]; re-exported here so
// the historical `measurement::Xyz` path keeps resolving.
pub use crate::sample::Xyz;

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

/// Parse a 108-byte `0xF6` calibration reply. Caller is responsible for
/// verifying the checksum byte (last byte of the payload) before calling.
pub fn parse_calibration(payload: &[u8], expected_index: u8) -> Result<Calibration, ParseError> {
    if payload.len() != 0x6C {
        return Err(ParseError::BadLength {
            expected: 0x6C,
            got: payload.len(),
        });
    }

    let index = payload[0];
    if index != expected_index {
        return Err(ParseError::CalIndexMismatch {
            expected: expected_index,
            got: index,
        });
    }

    let v1 = payload[1];
    let v2 = u16::from_be_bytes([payload[2], payload[3]]);
    let v4: [u8; 6] = payload[4..10].try_into().unwrap();

    // Matrix: 3 rows × 6 cols of f32 LE.
    // Argyll's offset formula: `10 + (j * 3 + i) * 4` for matrix[i][j].
    // Indices drive a transposed byte-offset computation, so explicit
    // `for i/j` reads clearer here than an enumerated iterator chain.
    let mut matrix = [[0.0f64; 6]; 3];
    #[allow(clippy::needless_range_loop)]
    for i in 0..3 {
        for j in 0..6 {
            let off = 10 + (j * 3 + i) * 4;
            let bytes: [u8; 4] = payload[off..off + 4].try_into().unwrap();
            matrix[i][j] = f32::from_le_bytes(bytes) as f64;
        }
    }

    // Gain + offset: 3 pairs of f32 LE at bytes 82..106.
    let mut gain = [0.0f64; 3];
    let mut offset = [0.0f64; 3];
    for j in 0..3 {
        let gain_off = 82 + (j * 2) * 4;
        let off_off = 82 + (j * 2 + 1) * 4;
        gain[j] = f32::from_le_bytes(payload[gain_off..gain_off + 4].try_into().unwrap()) as f64;
        offset[j] = f32::from_le_bytes(payload[off_off..off_off + 4].try_into().unwrap()) as f64;
    }

    let v3 = payload[106];

    Ok(Calibration {
        index,
        v1,
        v2,
        v4,
        matrix,
        gain,
        offset,
        v3,
    })
}

/// Parse a 22-byte `0xF7` setup reply. Caller verifies the checksum.
pub fn parse_setup(payload: &[u8], expected_v1: u8) -> Result<Setup, ParseError> {
    if payload.len() != 0x16 {
        return Err(ParseError::BadLength {
            expected: 0x16,
            got: payload.len(),
        });
    }

    let s2 = u16::from_be_bytes([payload[0], payload[1]]);
    let s1 = payload[2];
    if s1 != expected_v1 {
        return Err(ParseError::SetupV1Mismatch {
            expected: expected_v1,
            got: s1,
        });
    }
    let s3: [u8; 6] = payload[3..9].try_into().unwrap();
    let s4: [u8; 6] = payload[9..15].try_into().unwrap();
    let s5: [u8; 6] = payload[15..21].try_into().unwrap();

    Ok(Setup { s1, s2, s3, s4, s5 })
}

/// Parse a 12-byte `0xF2` measurement reply: 6 × u16 BE.
pub fn parse_raw_measurement(payload: &[u8]) -> Result<RawMeasurement, ParseError> {
    if payload.len() != 0xc {
        return Err(ParseError::BadLength {
            expected: 0xc,
            got: payload.len(),
        });
    }
    let mut raw = [0u16; 6];
    for i in 0..6 {
        raw[i] = u16::from_be_bytes([payload[2 * i], payload[2 * i + 1]]);
    }
    Ok(RawMeasurement(raw))
}

/// Encode the 15-byte payload for the `0xF2` measure command.
pub fn encode_measure_request(setup: &Setup) -> [u8; 15] {
    let mut buf = [0u8; 15];
    buf[0..2].copy_from_slice(&setup.s2.to_be_bytes());
    buf[2] = setup.s1;
    buf[3..9].copy_from_slice(&setup.s3);
    buf[9..15].copy_from_slice(&setup.s4);
    buf
}

// ---------------------------------------------------------------------------
// Raw → XYZ
// ---------------------------------------------------------------------------

/// Convert raw sensor counts to XYZ using the device's per-unit calibration.
///
/// Steps (per `spydX2_GetReading()`):
/// 1. Black-cal subtraction: `corrected[i] = max(0, raw[i] - setup.s5[i])`
///    (the user-side black-cal `bcal[]` from Argyll is omitted here — we
///    haven't performed our own black calibration, so it would be all zeros)
/// 2. Matrix multiply: `XYZ[i] = sum(matrix[i][j] * corrected[j], j in 0..6)`
/// 3. Per-channel gain + offset: `XYZ[i] = XYZ[i] * gain[i] + offset[i]`
// Fixed-size 3×6 matrix math; explicit index loops read clearer than
// iterator chains here.
#[allow(clippy::needless_range_loop)]
pub fn raw_to_xyz(raw: &RawMeasurement, setup: &Setup, cal: &Calibration) -> Xyz {
    // Black-cal subtraction with saturating semantics.
    let mut corrected = [0.0f64; 6];
    for i in 0..6 {
        let r = raw.0[i] as i32 - setup.s5[i] as i32;
        corrected[i] = r.max(0) as f64;
    }

    // Matrix multiply (3×6 * 6×1).
    let mut xyz = [0.0f64; 3];
    for i in 0..3 {
        let mut acc = 0.0;
        for j in 0..6 {
            acc += cal.matrix[i][j] * corrected[j];
        }
        xyz[i] = acc;
    }

    // Per-channel gain + offset.
    for i in 0..3 {
        xyz[i] = xyz[i] * cal.gain[i] + cal.offset[i];
    }

    Xyz {
        x: xyz[0],
        y: xyz[1],
        z: xyz[2],
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("payload length mismatch: expected {expected}, got {got}")]
    BadLength { expected: usize, got: usize },

    #[error("calibration index mismatch: sent {expected}, device echoed {got}")]
    CalIndexMismatch { expected: u8, got: u8 },

    #[error("setup v1 mismatch: expected {expected}, got {got}")]
    SetupV1Mismatch { expected: u8, got: u8 },
}

// ---------------------------------------------------------------------------
// Integration-time override
// ---------------------------------------------------------------------------

/// Below this the protocol overhead dominates wall time and SNR collapses on
/// any non-trivial signal. Above this the firmware silently misbehaves (raw
/// counts and wall time both go nonsensical past ~720 ms, the per-unit
/// calibration ceiling).
pub const MIN_INTEGRATION_MS: u16 = 10;

#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    #[error("integration time {got} ms out of range [{min}, {max}]")]
    OutOfRange { got: u16, min: u16, max: u16 },
}

impl Calibration {
    /// The integration time (ms) this per-unit calibration was characterized
    /// at — and the device's natural firmware ceiling. Same as `v2`; named for
    /// readability at call sites.
    pub fn integration_ms(&self) -> u16 {
        self.v2
    }
}

/// Which tier produced an [`AdaptiveMeasurement`] — for telemetry and event
/// reporting. Callers that want to weight tiers (or count escalations) should
/// branch on this rather than inspecting setup.s2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptiveTier {
    /// Adaptive was disabled (or the requested fast integration was out of
    /// range): a single default-integration measurement was taken.
    SingleFull,
    /// Two-tier: the fast measurement passed `is_trustworthy()` on the first
    /// attempt and is what's returned.
    Fast,
    /// Two-tier: the fast measurement was untrustworthy; the returned data is
    /// the default-integration re-measurement that followed.
    EscalatedFull,
}

/// Result of [`Colorimeter::measure_adaptive`]. The `setup` and `cal` fields
/// are the pair that actually produced `raws` (possibly the override pair, or
/// the originals if no override or after escalation). Pass them to
/// [`MeasurementConfidence::from_repeats`](crate::confidence::MeasurementConfidence::from_repeats)
/// for correct XYZ scaling.
#[derive(Debug, Clone)]
pub struct AdaptiveMeasurement {
    pub raws: Vec<RawMeasurement>,
    pub setup: Setup,
    pub cal: Calibration,
    pub tier: AdaptiveTier,
}

/// Build a [`Setup`] + [`Calibration`] pair for measuring at a non-default
/// integration time. **Use the returned pair together** — the setup tells the
/// device how long to integrate, the calibration scales raw counts so that
/// [`raw_to_xyz`] (and downstream
/// [`MeasurementConfidence`](crate::confidence::MeasurementConfidence)) return
/// XYZ in the same absolute units as a default-integration measurement.
/// Pairing the returned setup with the original calibration (or vice versa)
/// silently misreports luminance by the integration ratio.
///
/// The matrix is scaled by `cal.integration_ms() / target_ms` so that
/// `XYZ = matrix_scaled · (raw_at_target − s5) · gain + offset` reproduces the
/// XYZ a default-integration measurement of the same scene would have given.
/// Gain and offset are unchanged (they're integration-independent calibration
/// terms applied after the matrix).
///
/// `target_ms` must lie in `[`[`MIN_INTEGRATION_MS`]`, cal.integration_ms()]`
/// — the upper end is the firmware's actual ceiling.
///
/// # Caveats
/// The dark-cal floor `s5` is left unscaled. True dark current is proportional
/// to integration time, so subtracting the calibrated (default-integration) s5
/// at a shorter integration over-corrects by a few counts. The bias is below
/// 1% for any meaningfully bright signal — the regime where short integration
/// is worth using.
pub fn override_integration(
    setup: &Setup,
    cal: &Calibration,
    target_ms: u16,
) -> Result<(Setup, Calibration), IntegrationError> {
    let max_ms = cal.integration_ms();
    if target_ms < MIN_INTEGRATION_MS || target_ms > max_ms {
        return Err(IntegrationError::OutOfRange {
            got: target_ms,
            min: MIN_INTEGRATION_MS,
            max: max_ms,
        });
    }

    let scale = cal.v2 as f64 / target_ms as f64;
    let mut scaled_cal = cal.clone();
    for row in &mut scaled_cal.matrix {
        for entry in row {
            *entry *= scale;
        }
    }
    let new_setup = Setup {
        s2: target_ms,
        ..setup.clone()
    };
    Ok((new_setup, scaled_cal))
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    fn unit_cal_v2(v2: u16) -> Calibration {
        let mut matrix = [[0.0f64; 6]; 3];
        matrix[0][0] = 1.0;
        matrix[1][1] = 1.0;
        matrix[2][2] = 1.0;
        Calibration {
            index: 0,
            v1: 0,
            v2,
            v4: [0; 6],
            matrix,
            gain: [1.0; 3],
            offset: [0.0; 3],
            v3: 0,
        }
    }

    fn zero_floor_setup() -> Setup {
        Setup {
            s1: 0,
            s2: 714,
            s3: [0; 6],
            s4: [0; 6],
            s5: [0; 6],
        }
    }

    /// Half-integration measurement should reconstruct the same XYZ as the
    /// default once we use the returned scaled calibration. This is the
    /// guarantee the API exists to make.
    #[test]
    fn override_preserves_absolute_xyz() {
        let cal = unit_cal_v2(714);
        let setup = zero_floor_setup();
        let raw_full = RawMeasurement([200, 150, 100, 0, 0, 0]);
        let xyz_full = raw_to_xyz(&raw_full, &setup, &cal);

        let (setup_half, cal_half) = override_integration(&setup, &cal, 357).unwrap();
        assert_eq!(setup_half.s2, 357);
        let raw_half = RawMeasurement([100, 75, 50, 0, 0, 0]);
        let xyz_half = raw_to_xyz(&raw_half, &setup_half, &cal_half);

        assert!((xyz_full.x - xyz_half.x).abs() < 1e-9);
        assert!((xyz_full.y - xyz_half.y).abs() < 1e-9);
        assert!((xyz_full.z - xyz_half.z).abs() < 1e-9);
    }

    /// Below MIN or above cal.v2 the firmware misbehaves; the API refuses
    /// rather than handing back garbage.
    #[test]
    fn override_rejects_out_of_range() {
        let cal = unit_cal_v2(714);
        let setup = zero_floor_setup();
        assert!(matches!(
            override_integration(&setup, &cal, 5),
            Err(IntegrationError::OutOfRange { .. })
        ));
        assert!(matches!(
            override_integration(&setup, &cal, 1000),
            Err(IntegrationError::OutOfRange { .. })
        ));
        // Boundaries are accepted.
        assert!(override_integration(&setup, &cal, MIN_INTEGRATION_MS).is_ok());
        assert!(override_integration(&setup, &cal, cal.v2).is_ok());
    }

    /// The override at the device default must be a no-op — same setup, same
    /// matrix entries. This is what lets a caller pass `cal.integration_ms()`
    /// unconditionally without changing behavior.
    #[test]
    fn override_at_default_is_identity() {
        let cal = unit_cal_v2(714);
        let setup = zero_floor_setup();
        let (setup_id, cal_id) = override_integration(&setup, &cal, 714).unwrap();
        assert_eq!(setup_id.s2, setup.s2);
        for i in 0..3 {
            for j in 0..6 {
                assert!((cal_id.matrix[i][j] - cal.matrix[i][j]).abs() < 1e-12);
            }
        }
    }
}
