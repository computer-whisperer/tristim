//! High-level handle for an open Datacolor Spyder colorimeter.

use crate::measurement::{
    self, Calibration, RawMeasurement, Setup, Xyz, encode_measure_request,
    parse_calibration, parse_raw_measurement, parse_setup,
};
use crate::protocol::{DATACOLOR_VID, EP_IN, EP_OUT, HEADER_LEN, Opcode, pid};
use rusb::{Context, DeviceHandle, UsbContext};
use std::thread;
use std::time::Duration;
use thiserror::Error;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const INTERFACE: u8 = 0;

#[derive(Debug, Error)]
pub enum Error {
    #[error("USB I/O: {0}")]
    Usb(#[from] rusb::Error),

    #[error("no Datacolor colorimeter found (looked for VID 0x{0:04x})")]
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
    Parse(#[from] measurement::ParseError),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Decoded device info as returned by opcode `0xC2`.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Hardware version as `(major, minor)`. SpyderX2 firmware = `(5, 50)`;
    /// Spyder 2024 firmware = `(6, 0)` typically.
    pub hw_version: (u32, u32),
    /// 8-character ASCII serial number, trimmed.
    pub serial: String,
    /// For Spyder 2024 firmware: high-level command set is available
    /// (detected by the `09 08 01` signature at reply bytes `[17..=19]`).
    pub high_level_commands: bool,
    /// Spyder 2024 only: max display-type number that the high-level commands accept.
    pub max_display_type: Option<u8>,
    /// Spyder 2024 only: bitmask of valid display-type numbers.
    pub display_type_mask: Option<u16>,
}

/// An opened SpyderX2-family colorimeter (covers SpyderX2 and Spyder 2024).
pub struct Colorimeter {
    handle: DeviceHandle<Context>,
    pid: u16,
}

impl Colorimeter {
    /// Find and open the first Spyder-family device on the bus.
    ///
    /// Tries PIDs `SPYDER_2024` (0x0A0B) and `SPYDERX2` (0x0A0A) — both use
    /// the spydX2 protocol implemented here. The original `SPYDERX` (0x0A00)
    /// is not handled by this driver (different opcode set).
    pub fn open_any() -> Result<Self> {
        let ctx = Context::new()?;
        let devices = ctx.devices()?;

        let candidates = [pid::SPYDER_2024, pid::SPYDERX2];

        for device in devices.iter() {
            let desc = device.device_descriptor()?;
            if desc.vendor_id() != DATACOLOR_VID {
                continue;
            }
            if !candidates.contains(&desc.product_id()) {
                continue;
            }
            let handle = device.open()?;
            if handle.kernel_driver_active(INTERFACE).unwrap_or(false) {
                handle.detach_kernel_driver(INTERFACE)?;
            }
            // Some libusb backends require a configuration be active before
            // claim_interface; idempotent so safe regardless.
            let _ = handle.set_active_configuration(1);
            handle.claim_interface(INTERFACE)?;
            let _ = handle.set_alternate_setting(INTERFACE, 0);
            let device = Self {
                handle,
                pid: desc.product_id(),
            };
            // Vendor-class reset; without it the device receives bulk writes
            // but never replies. See `send_reset()` for details.
            device.send_reset()?;
            return Ok(device);
        }

        Err(Error::NotFound(DATACOLOR_VID))
    }

    /// USB product ID of the device we opened.
    pub fn pid(&self) -> u16 {
        self.pid
    }

    /// True if this is the Spyder 2024 lineup (vs. SpyderX2).
    pub fn is_spyder_2024(&self) -> bool {
        self.pid == pid::SPYDER_2024
    }

    /// Vendor-class reset that the SpyderX2/2024 firmware requires before it
    /// will respond to bulk commands. Mirrors `spydX2_reset()` in ArgyllCMS.
    /// Identical request for SpyderX, X2, and 2024.
    fn send_reset(&self) -> Result<()> {
        const BM_REQUEST_TYPE: u8 = 0x41;
        const B_REQUEST: u8 = 0x02;
        const W_VALUE: u16 = 2;
        const W_INDEX: u16 = 0;
        self.handle.write_control(
            BM_REQUEST_TYPE,
            B_REQUEST,
            W_VALUE,
            W_INDEX,
            &[],
            DEFAULT_TIMEOUT,
        )?;
        // Required — anything less and the device hasn't finished resetting
        // when we hit it with the next command.
        thread::sleep(Duration::from_millis(500));
        Ok(())
    }

    /// Execute one command against the device. See [`protocol`](crate::protocol)
    /// module docs for the wire format.
    pub fn command(
        &mut self,
        opcode: Opcode,
        send_payload: &[u8],
        reply_size: usize,
        verify_checksum: bool,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let mut send_buf = Vec::with_capacity(HEADER_LEN + send_payload.len());
        let nonce: u16 = rand::random();
        send_buf.push(opcode as u8);
        send_buf.extend_from_slice(&nonce.to_be_bytes());
        send_buf.extend_from_slice(&(send_payload.len() as u16).to_be_bytes());
        send_buf.extend_from_slice(send_payload);

        let written = self.handle.write_bulk(EP_OUT, &send_buf, timeout)?;
        if written != send_buf.len() {
            return Err(Error::ShortWrite {
                sent: written,
                expected: send_buf.len(),
            });
        }

        let mut recv_buf = vec![0u8; HEADER_LEN + reply_size];
        let read = self.handle.read_bulk(EP_IN, &mut recv_buf, timeout)?;
        if read != recv_buf.len() {
            return Err(Error::ShortRead {
                got: read,
                expected: recv_buf.len(),
            });
        }

        let echoed_nonce = u16::from_be_bytes([recv_buf[0], recv_buf[1]]);
        if echoed_nonce != nonce {
            return Err(Error::NonceMismatch {
                sent: nonce,
                got: echoed_nonce,
            });
        }

        let iec = recv_buf[2];
        if iec != 0 {
            return Err(Error::InstrumentError(iec));
        }

        let reported_len = u16::from_be_bytes([recv_buf[3], recv_buf[4]]) as usize;
        if reported_len != reply_size {
            return Err(Error::PayloadLenMismatch {
                reported: reported_len,
                expected: reply_size,
            });
        }

        let payload = recv_buf[HEADER_LEN..].to_vec();

        if verify_checksum && !payload.is_empty() {
            let n = payload.len();
            let computed: u8 = payload[..n - 1]
                .iter()
                .copied()
                .fold(0u8, |a, b| a.wrapping_add(b));
            let advertised = payload[n - 1];
            if computed != advertised {
                return Err(Error::ChecksumMismatch {
                    computed,
                    advertised,
                });
            }
        }

        Ok(payload)
    }

    /// Read hardware version + serial + extended capabilities in one call.
    /// Issues opcode `0xC2`. See [`Opcode::GetInfo`] for the reply layout.
    pub fn get_info(&mut self) -> Result<DeviceInfo> {
        let reply = self.command(Opcode::GetInfo, &[], 0x25, false, DEFAULT_TIMEOUT)?;

        let major = parse_ascii_int(&reply[0..1])
            .ok_or_else(|| Error::BadVersionString(reply.clone()))?;
        let minor = parse_ascii_int(&reply[2..4])
            .ok_or_else(|| Error::BadVersionString(reply.clone()))?;

        let serial_bytes = &reply[4..12];
        let serial = std::str::from_utf8(serial_bytes)
            .unwrap_or("")
            .trim_end_matches('\0')
            .trim()
            .to_string();

        // Spyder 2024 high-level signature at bytes [17..=19] = 09 08 01.
        let (high_level, max_display_type, display_type_mask) =
            if self.is_spyder_2024() && reply[17] == 0x09 && reply[18] == 0x08 && reply[19] == 0x01
            {
                let mxdnp1 = reply[35];
                let dnomask = u16::from_be_bytes([reply[20], reply[21]]);
                (true, Some(mxdnp1), Some(dnomask))
            } else {
                (false, None, None)
            };

        Ok(DeviceInfo {
            hw_version: (major, minor),
            serial,
            high_level_commands: high_level,
            max_display_type,
            display_type_mask,
        })
    }

    /// Download the per-unit calibration matrix for the given calibration
    /// index (0..7 on Spyder 2024; index 0 = "General"). Issues opcode `0xF6`.
    pub fn get_calibration(&mut self, cal_index: u8) -> Result<Calibration> {
        let reply = self.command(
            Opcode::GetCalibration,
            &[cal_index],
            0x6C,
            true, // checksummed
            DEFAULT_TIMEOUT,
        )?;
        Ok(parse_calibration(&reply, cal_index)?)
    }

    /// Fetch measurement-time setup for the given calibration (using its `v1`).
    /// Issues opcode `0xF7`.
    pub fn get_setup(&mut self, cal: &Calibration) -> Result<Setup> {
        let reply = self.command(
            Opcode::GetSetup,
            &[cal.v1],
            0x16,
            true, // checksummed
            DEFAULT_TIMEOUT,
        )?;
        Ok(parse_setup(&reply, cal.v1)?)
    }

    /// Take one raw 6-channel measurement using the given setup.
    /// Issues `send_reset()` (auto-zero) then opcode `0xF2`. The integration
    /// time embedded in `setup.s2` determines how long the device takes to
    /// reply — for our default (~714 msec), expect ~1 second wall time.
    pub fn measure_raw(&mut self, setup: &Setup) -> Result<RawMeasurement> {
        // Argyll resets before every measurement (auto-zero behavior).
        self.send_reset()?;
        let send = encode_measure_request(setup);
        // No checksum on the measurement reply per spydX2_Measure (last arg is 0).
        // Bump timeout — integration time alone can be ~720 msec.
        let reply = self.command(
            Opcode::Measure,
            &send,
            0xc,
            false,
            Duration::from_secs(3),
        )?;
        Ok(parse_raw_measurement(&reply)?)
    }

    /// End-to-end XYZ measurement using calibration index `cal_index`.
    /// Convenience wrapper: downloads calibration if not cached, fetches
    /// setup, takes a measurement, converts raw counts to XYZ.
    pub fn measure_xyz(&mut self, cal_index: u8) -> Result<(Xyz, RawMeasurement, Calibration, Setup)> {
        let cal = self.get_calibration(cal_index)?;
        let setup = self.get_setup(&cal)?;
        let raw = self.measure_raw(&setup)?;
        let xyz = measurement::raw_to_xyz(&raw, &setup, &cal);
        Ok((xyz, raw, cal, setup))
    }
}

fn parse_ascii_int(bytes: &[u8]) -> Option<u32> {
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
