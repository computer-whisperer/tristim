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

    /// 16-bit integration parameter (BE). Distinct from `Calibration::v2`
    /// despite the similar role — Argyll comments call this a "magic" value.
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

/// CIE XYZ tristimulus values. Units depend on calibration choice — for the
/// 2024 emissive cal indexes, Y is approximately luminance in cd/m² when the
/// device is held against an active display.
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
}

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
    let mut matrix = [[0.0f64; 6]; 3];
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
