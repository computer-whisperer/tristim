//! Wire-protocol constants for the SpyderX2 / Spyder 2024 family.
//!
//! The original SpyderX (PID `0x0A00`) has its own opcode set documented
//! separately in ArgyllCMS `spectro/spydX.c`. **Our device — the SpyderExpress
//! 2024, PID `0x0A0B` — speaks the *X2/2024* protocol** documented in
//! `spectro/spydX2.c` (added in ArgyllCMS V3.4.0, fixes in V3.5.0).
//!
//! The wire framing is identical between SpyderX and SpyderX2/2024; only the
//! opcode set and reply structures differ.
//!
//! ## Packet layout (both directions)
//!
//! ```text
//! Send (to bulk OUT endpoint 0x01):
//!   [0]    opcode
//!   [1..3] nonce              (u16 BE, host-generated, random)
//!   [3..5] payload length     (u16 BE)
//!   [5..]  payload bytes
//!
//! Receive (from bulk IN endpoint 0x81):
//!   [0..2] echoed nonce       (u16 BE, must match what we sent)
//!   [2]    instrument error   (0 = OK, non-zero = device-reported failure)
//!   [3..5] payload length     (u16 BE, must match expected r_size)
//!   [5..]  payload bytes
//!
//! When checksum-protected, the final payload byte is
//! `(sum of preceding payload bytes) & 0xFF`.
//! ```
//!
//! ## Vendor-class reset (mandatory before first bulk command)
//!
//! `bmRequestType=0x41` (Host→Device, vendor, recipient=interface),
//! `bRequest=0x02`, `wValue=2`, `wIndex=0`, no data, then **500 ms sleep**.

/// Datacolor / ColorVision USB vendor ID.
pub const DATACOLOR_VID: u16 = 0x085c;

/// Known SpyderX-family product IDs.
pub mod pid {
    /// Original SpyderX (2019). Uses the `spydX.c` protocol — older opcodes.
    pub const SPYDERX: u16 = 0x0a00;
    /// SpyderX2 (2023). Uses the new X2/2024 protocol.
    pub const SPYDERX2: u16 = 0x0a0a;
    /// Spyder 2024 lineup (SpyderExpress / SpyderPro / Spyder). Same protocol
    /// as X2 with an `is2024` flag enabling extended high-level commands.
    pub const SPYDER_2024: u16 = 0x0a0b;
}

/// USB endpoint addresses (same for SpyderX, X2, and 2024).
pub const EP_OUT: u8 = 0x01;
pub const EP_IN:  u8 = 0x81;

/// Header bytes on every packet (both directions).
pub const HEADER_LEN: usize = 5;

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
