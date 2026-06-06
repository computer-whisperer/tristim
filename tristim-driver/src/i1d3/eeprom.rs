//! Decoding of the i1d3's two EEPROMs.
//!
//! * **Internal** (256 bytes, command `0x0800`): serial number, hardware
//!   revision string, and per-channel black-level offsets.
//! * **External** (8192 bytes, command `0x1200`): the per-unit factory
//!   calibration — the spectral sensitivity of each RGB sensor channel,
//!   351 samples at 1 nm from 380 to 730 nm, plus a calibration date and a
//!   checksum.
//!
//! All multi-byte values are little-endian.

/// Wavelength sample count of each sensor sensitivity curve (380–730 nm
/// inclusive at 1 nm).
pub const SPECTRAL_BANDS: usize = 351;
/// First wavelength (nm) of the sensor curves.
pub const SPECTRAL_WL_SHORT: usize = 380;

/// Decoded internal EEPROM.
#[derive(Debug, Clone)]
pub struct InternalEeprom {
    /// Serial number (offset 0x10, up to 20 ASCII bytes).
    pub serial: String,
    /// Hardware revision string, e.g. `"A-01"`, `"B-02"` (offset 0x2C).
    pub version: String,
    /// Per-channel black-level offsets in Hz (u32 LE at 0x04 + 4·ch,
    /// divided by 6e6; `0xFFFFFFFF` means unset → 0.0).
    ///
    /// Note: ArgyllCMS's decoder has an apparent index typo that stores all
    /// three reads into channel 0 (leaving 1 and 2 zero). We decode each
    /// channel as evidently intended; the values are typically tiny either
    /// way.
    pub black_level_hz: [f64; 3],
}

/// Decode the 256-byte internal EEPROM image.
pub fn decode_internal(buf: &[u8; 256]) -> InternalEeprom {
    let mut black = [0.0f64; 3];
    for (ch, b) in black.iter_mut().enumerate() {
        let off = 0x04 + 4 * ch;
        let t = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        if t != 0xffff_ffff {
            *b = f64::from(t) / 6e6;
        }
    }
    InternalEeprom {
        serial: ascii_field(&buf[0x10..0x10 + 20]),
        version: ascii_field(&buf[0x2C..0x2C + 10]),
        black_level_hz: black,
    }
}

/// Decoded external EEPROM (the calibration-relevant fields; the ambient
/// filter spectrum at 0x10BC is not decoded — ambient mode is out of scope).
#[derive(Debug, Clone)]
pub struct ExternalEeprom {
    /// Calibration date as the raw 64-bit LE value at 0x1E (an OEM-format
    /// timestamp; recorded for provenance, not interpreted).
    pub cal_date_raw: u64,
    /// Whether the checksum over the calibration area matched. Only the
    /// `"A-01"` hardware revision has a reliable checksum; on other
    /// revisions a mismatch is recorded but not fatal (mirroring ArgyllCMS).
    pub checksum_ok: bool,
    /// Sensor spectral sensitivities `[channel][band]`, 380–730 nm at 1 nm,
    /// in Hz per mW/nm (stored on the device as f32 LE in Hz per W/nm at
    /// offset 0x26 + ch·351·4; divided by 1000 here).
    pub sensitivity: Box<[[f64; SPECTRAL_BANDS]; 3]>,
}

/// Decode the 8192-byte external EEPROM image.
pub fn decode_external(buf: &[u8; 8192]) -> ExternalEeprom {
    // Checksum: u16 LE at [2..4] over bytes 4..0x179A; some revisions match
    // an alternate range 4..0x178E instead.
    let recorded = u16::from_le_bytes([buf[2], buf[3]]);
    let sum = |end: usize| -> u16 {
        buf[4..end]
            .iter()
            .fold(0u32, |a, &b| a.wrapping_add(u32::from(b))) as u16
    };
    let checksum_ok = recorded == sum(0x179A) || recorded == sum(0x178E);

    let cal_date_raw = u64::from_le_bytes(buf[0x1E..0x26].try_into().unwrap());

    let mut sensitivity = Box::new([[0.0f64; SPECTRAL_BANDS]; 3]);
    for (ch, curve) in sensitivity.iter_mut().enumerate() {
        let base = 0x26 + ch * SPECTRAL_BANDS * 4;
        for (i, s) in curve.iter_mut().enumerate() {
            let off = base + i * 4;
            let v = f32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
            *s = f64::from(v) / 1000.0; // Hz per W/nm → Hz per mW/nm
        }
    }

    ExternalEeprom {
        cal_date_raw,
        checksum_ok,
        sensitivity,
    }
}

fn ascii_field(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_decodes_fields() {
        let mut buf = [0u8; 256];
        buf[0x04..0x08].copy_from_slice(&6_000_000u32.to_le_bytes()); // 1.0 Hz
        buf[0x08..0x0C].copy_from_slice(&0xffff_ffffu32.to_le_bytes()); // unset
        buf[0x0C..0x10].copy_from_slice(&3_000_000u32.to_le_bytes()); // 0.5 Hz
        buf[0x10..0x17].copy_from_slice(b"SN12345");
        buf[0x2C..0x30].copy_from_slice(b"A-01");
        let ee = decode_internal(&buf);
        assert_eq!(ee.serial, "SN12345");
        assert_eq!(ee.version, "A-01");
        assert_eq!(ee.black_level_hz, [1.0, 0.0, 0.5]);
    }

    #[test]
    fn external_decodes_spectra_and_checksum() {
        let mut buf = [0u8; 8192];
        buf[0x1E..0x26].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        // Channel 1, band 10 = 2000 Hz/W/nm → 2.0 Hz/mW/nm.
        let off = 0x26 + 351 * 4 + 10 * 4;
        buf[off..off + 4].copy_from_slice(&2000.0f32.to_le_bytes());
        // Make the primary-range checksum match.
        let sum = buf[4..0x179A]
            .iter()
            .fold(0u32, |a, &b| a.wrapping_add(u32::from(b))) as u16;
        buf[2..4].copy_from_slice(&sum.to_le_bytes());

        let ee = decode_external(&buf);
        assert!(ee.checksum_ok);
        assert_eq!(ee.cal_date_raw, 0x0102_0304_0506_0708);
        assert_eq!(ee.sensitivity[1][10], 2.0);
        assert_eq!(ee.sensitivity[0][10], 0.0);
    }

    #[test]
    fn external_flags_bad_checksum() {
        let mut buf = [0u8; 8192];
        buf[2] = 0xAA;
        buf[3] = 0xBB;
        buf[100] = 7; // ensure neither range sums to 0xBBAA
        let ee = decode_external(&buf);
        assert!(!ee.checksum_ok);
    }
}
