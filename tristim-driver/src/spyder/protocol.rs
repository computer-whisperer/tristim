//! Wire-protocol opcodes for the SpyderX2 / Spyder 2024 variant.
//!
//! The framing, reset, and endpoint facts shared with the original SpyderX
//! live in [`transport`](super::transport); this module holds what is
//! specific to the *X2/2024* protocol documented in ArgyllCMS
//! `spectro/spydX2.c` (added V3.4.0, fixes in V3.5.0). The original SpyderX
//! opcode set is in [`spyderx`](super::spyderx).

// Re-exported here because this is where they historically lived; the
// canonical home is the family-shared transport module.
pub use super::transport::{DATACOLOR_VID, EP_IN, EP_OUT, HEADER_LEN, pid};

/// Command opcodes for the SpyderX2 / Spyder 2024 protocol.
///
/// Source: `ArgyllCMS spectro/spydX2.c` (added V3.4.0, fixes V3.5.0).
/// Note the differences from the original SpyderX:
///   `0xD9` (HW version) → folded into `0xC2`
///   `0xCB` (cal matrix) → `0xF6` with larger 108-byte reply
///   `0xC3` (setup)      → `0xF7` with 22-byte reply
///   `0xD2` (measure)    → `0xF2` with 15-byte send / 12-byte reply
///   `0xFA` is a new high-level command (Spyder 2024 firmware only)
///   `0xC2` (info) and `0xD4` (ambient measure) opcodes are unchanged
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Opcode {
    /// `0xC2` — get device info. Reply: 37 bytes (`0x25`).
    /// Layout:
    ///   `[0]`       ASCII digit, hardware major version
    ///   `[2..=3]`   ASCII digits, hardware minor version
    ///   `[4..=11]`  8-byte ASCII serial number (zero-terminated separately)
    ///   `[17..=21]` Spyder 2024 extended capabilities (when `[17..=19] == 09 08 01`)
    ///   `[35]`      max display number + 1 (Spyder 2024 only)
    /// SpyderX2 firmware reports `5.50`, Spyder 2024 reports `6.00`.
    GetInfo = 0xC2,

    /// `0xF6` — get per-unit calibration matrix for cal index `N`.
    /// Send: 1 byte (cal index). Reply: 108 bytes (`0x6C`, checksummed).
    GetCalibration = 0xF6,

    /// `0xF7` — get measurement setup parameters for cal index `N`.
    /// Send: 1 byte. Reply: 22 bytes (`0x16`, checksummed).
    GetSetup = 0xF7,

    /// `0xF2` — take an emissive measurement.
    /// Send: 15 bytes (`0xf`, the setup params from `GetSetup`).
    /// Reply: 12 bytes (`0xc`, raw sensor counts).
    Measure = 0xF2,

    /// `0xFA` — high-level Spyder 2024 measurement command (compact, takes
    /// display-type number, returns XYZ directly). Send: 1 byte, reply: 13 bytes.
    /// Only available when `GetInfo` reports `hlavail=1`.
    HighLevelMeasure = 0xFA,

    /// `0xD4` — ambient-light measurement. Send: 2 bytes, reply: 6 bytes.
    AmbientMeasure = 0xD4,
}
