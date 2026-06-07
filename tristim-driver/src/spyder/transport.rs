//! USB transport shared by every Spyder-family protocol variant.
//!
//! The bulk framing, nonce/checksum scheme, and vendor-class reset are
//! byte-identical across the original SpyderX (`spydX.c`) and the
//! SpyderX2 / Spyder 2024 (`spydX2.c`) — only the opcode sets and payload
//! layouts differ, and those live with each variant's driver.
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
//! ```
//!
//! When checksum-protected, the final payload byte is
//! `(sum of preceding payload bytes) & 0xFF`.
//!
//! ## Vendor-class reset (mandatory before first bulk command)
//!
//! `bmRequestType=0x41` (Host→Device, vendor, recipient=interface),
//! `bRequest=0x02`, `wValue=2`, `wIndex=0`, no data, then **500 ms sleep**.
//! Without it the device receives bulk writes but never replies. Identical
//! for SpyderX, X2, and 2024.

use crate::colorimeter::{Error, Result};
use rusb::{Context, DeviceHandle, UsbContext};
use std::thread;
use std::time::Duration;

/// Datacolor / ColorVision USB vendor ID.
pub const DATACOLOR_VID: u16 = 0x085c;

/// Known Spyder-family product IDs.
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
pub const EP_IN: u8 = 0x81;

/// Header bytes on every packet (both directions).
pub const HEADER_LEN: usize = 5;

const INTERFACE: u8 = 0;
const RESET_TIMEOUT: Duration = Duration::from_secs(5);

/// Find and open the first Datacolor device whose PID is in `candidates`,
/// detaching any kernel driver and claiming interface 0. Returns the handle
/// and the PID that matched. `Error::NotFound` if nothing on the bus matches.
pub(crate) fn open_first(candidates: &[u16]) -> Result<(DeviceHandle<Context>, u16)> {
    let ctx = Context::new()?;
    let devices = ctx.devices()?;

    for device in devices.iter() {
        // An unreadable descriptor on some unrelated device shouldn't abort
        // the scan — skip it and keep looking.
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };
        if desc.vendor_id() != DATACOLOR_VID {
            continue;
        }
        let pid = desc.product_id();
        if !candidates.contains(&pid) {
            continue;
        }
        // From here on the failure concerns *this* device, so permission
        // errors map to `AccessDenied` (udev rule missing) rather than a
        // bare USB error.
        let at_open = |e| Error::at_open(e, DATACOLOR_VID, pid);
        let handle = device.open().map_err(at_open)?;
        if handle.kernel_driver_active(INTERFACE).unwrap_or(false) {
            handle.detach_kernel_driver(INTERFACE).map_err(at_open)?;
        }
        // Some libusb backends require a configuration be active before
        // claim_interface; idempotent so safe regardless.
        let _ = handle.set_active_configuration(1);
        handle.claim_interface(INTERFACE).map_err(at_open)?;
        let _ = handle.set_alternate_setting(INTERFACE, 0);
        return Ok((handle, pid));
    }

    Err(Error::NotFound(DATACOLOR_VID))
}

/// Vendor-class reset that Spyder-family firmware requires before it will
/// respond to bulk commands; also doubles as the auto-zero trigger before
/// measurements. Mirrors `spydX_reset()` / `spydX2_reset()` in ArgyllCMS.
pub(crate) fn send_reset(handle: &DeviceHandle<Context>) -> Result<()> {
    const BM_REQUEST_TYPE: u8 = 0x41;
    const B_REQUEST: u8 = 0x02;
    const W_VALUE: u16 = 2;
    const W_INDEX: u16 = 0;
    handle.write_control(
        BM_REQUEST_TYPE,
        B_REQUEST,
        W_VALUE,
        W_INDEX,
        &[],
        RESET_TIMEOUT,
    )?;
    // Required — anything less and the device hasn't finished resetting when
    // we hit it with the next command.
    thread::sleep(Duration::from_millis(500));
    Ok(())
}

/// Execute one framed command against the device (see module docs for the
/// wire format). `verify_checksum` enables the additive-u8 check on the reply
/// payload — calibration/setup replies are checksummed, measurement replies
/// are not.
pub(crate) fn command(
    handle: &DeviceHandle<Context>,
    opcode: u8,
    send_payload: &[u8],
    reply_size: usize,
    verify_checksum: bool,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut send_buf = Vec::with_capacity(HEADER_LEN + send_payload.len());
    let nonce: u16 = rand::random();
    send_buf.push(opcode);
    send_buf.extend_from_slice(&nonce.to_be_bytes());
    send_buf.extend_from_slice(&(send_payload.len() as u16).to_be_bytes());
    send_buf.extend_from_slice(send_payload);

    let written = handle.write_bulk(EP_OUT, &send_buf, timeout)?;
    if written != send_buf.len() {
        return Err(Error::ShortWrite {
            sent: written,
            expected: send_buf.len(),
        });
    }

    let mut recv_buf = vec![0u8; HEADER_LEN + reply_size];
    let read = handle.read_bulk(EP_IN, &mut recv_buf, timeout)?;
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
