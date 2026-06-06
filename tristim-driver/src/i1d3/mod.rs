//! X-Rite i1Display Pro / ColorMunki Display family driver (USB `0765:5020`,
//! the `i1d3.c` protocol).
//!
//! **Untested port.** This implements the wire format reverse-engineered by
//! Graeme Gill for ArgyllCMS (`spectro/i1d3.c`); we have not run it against
//! real hardware. Validation reports welcome.
//!
//! Covers the i1Display Pro, ColorMunki Display, and the OEM rebadges that
//! share the protocol (NEC SpectraSensor Pro, HP DreamColor, Calibrite-era
//! units, …) — the unlock key table carries one entry per variant and the
//! device picks its own.
//!
//! ## How a measurement works
//!
//! The sensor is a light-to-frequency converter per RGB channel. The driver
//! either counts edges over a fixed integration time (*frequency* mode, good
//! when bright) or measures the time taken to see a target number of edges
//! (*period* mode, good when dim), adaptively: a 0.2 s frequency measurement
//! first, then period re-measurement of any channel that yielded fewer than
//! 200 edges, with edge targets scaled from a pre-measurement. Frequencies
//! are black-subtracted and mapped to XYZ by a 3×3 matrix computed from the
//! per-unit sensor spectral sensitivities stored in the instrument's EEPROM
//! (see [`calmat`]).
//!
//! ## Minimal scope
//!
//! Deliberately not ported (accuracy/latency polish, not correctness):
//! AIO measurement mode (Rev B), refresh-rate detection and
//! refresh-synchronized integration, ambient mode, display-specific spectral
//! calibrations (CCSS), LED control, and the status query `0x0001` (whose
//! effect ArgyllCMS itself doesn't fully understand and which its
//! measurement path never consults). The generic EEPROM-derived matrix is
//! ArgyllCMS's own default.

pub mod calmat;
pub mod eeprom;
pub mod observer;
pub mod unlock;

use crate::colorimeter::{CalibrationId, Colorimeter, DeviceInfo, Error, Result};
use crate::sample::{Sample, Xyz};
use rusb::{Context, DeviceHandle, UsbContext};
use std::time::Duration;

/// X-Rite (now Calibrite) USB vendor ID.
pub const XRITE_VID: u16 = 0x0765;
/// The i1d3 family product ID (all variants share it).
pub const I1D3_PID: u16 = 0x5020;

const EP_OUT: u8 = 0x01;
const EP_IN: u8 = 0x81;
const INTERFACE: u8 = 0;

const CMD_TIMEOUT: Duration = Duration::from_secs(1);
/// Longest reading timeout — period measurements can take up to ~10 s of
/// integration plus margin (Argyll uses 40 s).
const MEAS_TIMEOUT: Duration = Duration::from_secs(40);

/// L2F sensor saturation frequency (Hz). Readings above this are unreliable.
const SAT_FREQ: f64 = 250e3;
/// Sensor clock frequency (Hz).
const CLK_FREQ: f64 = 12e6;
/// Default integration time for the initial frequency measurement (seconds;
/// Argyll's non-refresh default).
const DEFAULT_INTTIME: f64 = 0.2;

/// Command codes: high byte is the HID report-style major command (send
/// byte 0), low byte the minor command (send byte 1, only when major is 0).
#[repr(u16)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CommandCode {
    /// Product name string (reply bytes 2.. ASCII).
    ProdName = 0x0010,
    /// Product type number (u16 LE at reply byte 3; `0x0002` = ColorMunki
    /// Display).
    ProdType = 0x0011,
    /// Firmware version string.
    FirmVer = 0x0012,
    /// Lock status (locked unless reply byte 2 != 0 or byte 3 == 0).
    Locked = 0x0020,
    /// Frequency measurement over a given integration time.
    FreqMeas = 0x0100,
    /// Period measurement of a given edge count per channel.
    PeriodMeas = 0x0200,
    /// Read internal EEPROM (256 bytes; ≤60-byte chunks).
    ReadIntEe = 0x0800,
    /// Read external EEPROM (8192 bytes; ≤59-byte chunks).
    ReadExtEe = 0x1200,
    /// Get ambient-diffuser arm position (reply byte 1: 0 = display,
    /// 1 = ambient). Reply does not echo the command.
    GetDiffPos = 0x9400,
    /// Request an unlock challenge.
    LockChallenge = 0x9900,
    /// Send the unlock response (success: reply byte 2 == `0x77`).
    LockResponse = 0x9a00,
}

/// Instrument status byte `0x83`: a period measurement saw no edges within
/// the firmware timeout (Rev B behavior). Recoverable — the caller falls
/// back to a long frequency measurement.
const STATUS_PERIOD_FAIL: u8 = 0x83;

/// An opened i1d3-family colorimeter. **Untested port** — see module docs.
pub struct I1d3 {
    handle: DeviceHandle<Context>,
    info: DeviceInfo,
    /// Marketing name of the unlock-key variant that the device accepted
    /// (or "unlocked" if it never needed a key).
    variant: &'static str,
    /// `0x0002` = ColorMunki Display (slower measurement engine; the
    /// adaptive flow skips its second pre-measurement).
    prod_type: u16,
    /// Per-channel dark frequency offsets (Hz) from the internal EEPROM.
    black_hz: [f64; 3],
    /// EEPROM-derived RGB-Hz → XYZ matrix (the MIbLSr default calibration).
    matrix: [[f64; 3]; 3],
    /// Whether the external EEPROM checksum matched (recorded, non-fatal
    /// except on hardware revision A-01 — see [`eeprom::decode_external`]).
    cal_checksum_ok: bool,
}

impl I1d3 {
    /// Find and open the first i1d3-family device on the bus: claim it,
    /// unlock it, read the per-unit calibration EEPROMs, and compute the
    /// default emissive calibration matrix.
    pub fn open_any() -> Result<Self> {
        let ctx = Context::new()?;
        let devices = ctx.devices()?;

        for device in devices.iter() {
            let desc = device.device_descriptor()?;
            if desc.vendor_id() != XRITE_VID || desc.product_id() != I1D3_PID {
                continue;
            }
            let handle = device.open()?;
            // The i1d3 enumerates as a HID device; on Linux usbhid will have
            // claimed it.
            if handle.kernel_driver_active(INTERFACE).unwrap_or(false) {
                handle.detach_kernel_driver(INTERFACE)?;
            }
            let _ = handle.set_active_configuration(1);
            handle.claim_interface(INTERFACE)?;

            let mut dev = Self {
                handle,
                info: DeviceInfo {
                    vendor: "X-Rite".into(),
                    model: String::new(),
                    serial: String::new(),
                    firmware: (0, 0),
                    usb_pid: I1D3_PID,
                },
                variant: "unlocked",
                prod_type: 0,
                black_hz: [0.0; 3],
                matrix: [[0.0; 3]; 3],
                cal_checksum_ok: false,
            };
            dev.init()?;
            return Ok(dev);
        }

        Err(Error::NotFound(XRITE_VID))
    }

    /// The unlock-key variant the device accepted, e.g. `"i1Display Pro"`.
    pub fn variant(&self) -> &'static str {
        self.variant
    }

    /// True for the ColorMunki Display (product type `0x0002`).
    pub fn is_munki_display(&self) -> bool {
        self.prod_type == 0x0002
    }

    /// The EEPROM-derived RGB-Hz → XYZ calibration matrix in use.
    pub fn calibration_matrix(&self) -> [[f64; 3]; 3] {
        self.matrix
    }

    /// Whether the calibration EEPROM checksum matched (informational on
    /// most hardware revisions).
    pub fn cal_checksum_ok(&self) -> bool {
        self.cal_checksum_ok
    }

    /// One command/response exchange: 64-byte interrupt report each way.
    fn command(&mut self, cc: CommandCode, params: &[u8], timeout: Duration) -> Result<[u8; 64]> {
        let cc = cc as u16;
        let major = (cc >> 8) as u8;

        let mut send = [0u8; 64];
        send[0] = major;
        if major == 0x00 {
            send[1] = (cc & 0xff) as u8;
        }
        // Caller parameters start at byte 1 (byte 2 for major-0 commands
        // would collide with the minor code, but no major-0 command takes
        // parameters).
        send[1 + usize::from(major == 0x00)..1 + usize::from(major == 0x00) + params.len()]
            .copy_from_slice(params);

        let written = self.handle.write_interrupt(EP_OUT, &send, timeout)?;
        if written != 64 {
            return Err(Error::ShortWrite {
                sent: written,
                expected: 64,
            });
        }

        let mut recv = [0u8; 64];
        let read = self.handle.read_interrupt(EP_IN, &mut recv, timeout)?;
        if read != 64 {
            self.flush_response();
            return Err(Error::ShortRead {
                got: read,
                expected: 64,
            });
        }

        // Byte 0 is a status code; byte 1 echoes the major command (except
        // GetDiffPos, which returns the position there instead).
        if recv[0] != 0x00 {
            self.flush_response();
            return Err(Error::InstrumentError(recv[0]));
        }
        if cc != CommandCode::GetDiffPos as u16 && recv[1] != major {
            self.flush_response();
            return Err(Error::CommandEchoMismatch {
                sent: major,
                got: recv[1],
            });
        }

        Ok(recv)
    }

    /// Drain a possibly stale response after a failed exchange, so the next
    /// command doesn't read it (mirrors Argyll's 0.2 s flush).
    fn flush_response(&mut self) {
        let mut buf = [0u8; 64];
        let _ = self
            .handle
            .read_interrupt(EP_IN, &mut buf, Duration::from_millis(200));
    }

    fn read_string(&mut self, cc: CommandCode) -> Result<String> {
        let recv = self.command(cc, &[], CMD_TIMEOUT)?;
        let bytes = &recv[2..];
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        Ok(String::from_utf8_lossy(&bytes[..end]).trim().to_string())
    }

    fn lock_status(&mut self) -> Result<bool> {
        let recv = self.command(CommandCode::Locked, &[], CMD_TIMEOUT)?;
        // Unlocked when byte 2 != 0 or byte 3 == 0.
        Ok(!(recv[2] != 0 || recv[3] == 0))
    }

    fn unlock(&mut self) -> Result<()> {
        for key in unlock::UNLOCK_KEYS {
            let challenge = self.command(CommandCode::LockChallenge, &[], CMD_TIMEOUT)?;
            let response = unlock::create_unlock_response(key.key, &challenge);
            // The response occupies the whole payload (bytes 1..64 zero
            // except 24..40); send it verbatim.
            let recv = self.command(CommandCode::LockResponse, &response[1..], CMD_TIMEOUT)?;
            if recv[2] == 0x77 && !self.lock_status()? {
                self.variant = key.name;
                return Ok(());
            }
        }
        Err(Error::UnlockFailed)
    }

    fn read_internal_eeprom(&mut self) -> Result<[u8; 256]> {
        let mut out = [0u8; 256];
        let mut addr = 0usize;
        while addr < 256 {
            let chunk = (256 - addr).min(60);
            let recv = self.command(
                CommandCode::ReadIntEe,
                &[addr as u8, chunk as u8],
                CMD_TIMEOUT,
            )?;
            out[addr..addr + chunk].copy_from_slice(&recv[4..4 + chunk]);
            addr += chunk;
        }
        Ok(out)
    }

    fn read_external_eeprom(&mut self) -> Result<Box<[u8; 8192]>> {
        let mut out = Box::new([0u8; 8192]);
        let mut addr = 0usize;
        while addr < 8192 {
            let chunk = (8192 - addr).min(59);
            // Address is big-endian here — unlike every other multi-byte
            // field in this protocol.
            let params = [(addr >> 8) as u8, (addr & 0xff) as u8, chunk as u8];
            let recv = self.command(CommandCode::ReadExtEe, &params, CMD_TIMEOUT)?;
            out[addr..addr + chunk].copy_from_slice(&recv[5..5 + chunk]);
            addr += chunk;
        }
        Ok(out)
    }

    fn init(&mut self) -> Result<()> {
        self.info.model = self.read_string(CommandCode::ProdName)?;
        let recv = self.command(CommandCode::ProdType, &[], CMD_TIMEOUT)?;
        self.prod_type = u16::from_le_bytes([recv[3], recv[4]]);
        let firmware = self.read_string(CommandCode::FirmVer)?;
        self.info.firmware = parse_version(&firmware);

        if self.lock_status()? {
            self.unlock()?;
        }

        let int_ee = eeprom::decode_internal(&self.read_internal_eeprom()?);
        self.info.serial = int_ee.serial;
        self.black_hz = int_ee.black_level_hz;

        let ext_ee = eeprom::decode_external(&*self.read_external_eeprom()?);
        self.cal_checksum_ok = ext_ee.checksum_ok;
        // Only the A-01 hardware revision has a reliable calibration
        // checksum; there a mismatch means corrupt data (fatal, as in
        // ArgyllCMS). Later revisions changed the covered range and a
        // mismatch is merely recorded.
        if !ext_ee.checksum_ok && int_ee.version == "A-01" {
            return Err(Error::BadCalibration);
        }
        self.matrix = calmat::comp_calmat(&ext_ee.sensitivity).ok_or(Error::BadCalibration)?;

        Ok(())
    }

    /// Frequency measurement: both-edge counts per channel over `inttime`
    /// seconds (clock-rounded). Returns the counts and the rounded time.
    fn freq_measure(&mut self, inttime: f64) -> Result<([f64; 3], f64)> {
        let inttime = inttime.min(20.0);
        let intclks = (inttime * CLK_FREQ + 0.5) as u32;
        let actual = f64::from(intclks) / CLK_FREQ;

        let mut params = [0u8; 23];
        params[..4].copy_from_slice(&intclks.to_le_bytes());
        // params[22] (send byte 23): unknown, always 0.
        let recv = self.command(CommandCode::FreqMeas, &params, MEAS_TIMEOUT)?;

        let mut counts = [0.0f64; 3];
        for (i, c) in counts.iter_mut().enumerate() {
            let off = 2 + 4 * i;
            *c = f64::from(u32::from_le_bytes(recv[off..off + 4].try_into().unwrap()));
        }
        // The hardware synchronizes the L2F to the start of the integration
        // window, rounding the count down (0..-1 bit). Adding 0.5 centers
        // the quantization error — except on true zero, which must stay
        // consistent with period mode reporting zero.
        if counts.iter().all(|&c| c != 0.0) {
            for c in &mut counts {
                *c += 0.5;
            }
        }
        Ok((counts, actual))
    }

    /// Period measurement: clock counts to observe `edgec[ch]` edges, for
    /// the channels in `mask`. A channel that sees no edges within the
    /// firmware's ~10 s window reports 0 (Rev A) or fails the whole command
    /// with status `0x83` (Rev B) — the caller handles both.
    fn period_measure(&mut self, edgec: [u16; 3], mask: u8) -> Result<[f64; 3]> {
        let mut params = [0u8; 8];
        params[0..2].copy_from_slice(&edgec[0].to_le_bytes());
        params[2..4].copy_from_slice(&edgec[1].to_le_bytes());
        params[4..6].copy_from_slice(&edgec[2].to_le_bytes());
        params[6] = mask;
        // params[7] (send byte 8): unknown, always 0.
        let recv = self.command(CommandCode::PeriodMeas, &params, MEAS_TIMEOUT)?;

        let mut clocks = [0.0f64; 3];
        for (i, c) in clocks.iter_mut().enumerate() {
            let off = 2 + 4 * i;
            *c = f64::from(u32::from_le_bytes(recv[off..off + 4].try_into().unwrap()));
        }
        Ok(clocks)
    }

    /// Adaptive emissive measurement → per-channel frequency in Hz, black
    /// level already subtracted. Port of `i1d3_take_emis_measurement()`
    /// (adaptive mode, no refresh synchronization).
    // Per-channel mask/index math throughout; explicit indices mirror the
    // documented algorithm more directly than iterator chains.
    #[allow(clippy::needless_range_loop)]
    fn measure_emissive_hz(&mut self) -> Result<[f64; 3]> {
        let mut edgec = [2u16; 3];
        let mut rgb = [0.0f64; 3]; // result, Hz
        let mut rmeas; // raw measure: freq counts, then period clocks
        let mut rgb_trial = [0.0f64; 3]; // trial Hz from pre-measurement

        // Stage 1: fixed-period frequency measurement.
        let (counts, inttime) = self.freq_measure(DEFAULT_INTTIME)?;
        for i in 0..3 {
            rgb[i] = 0.5 * counts[i] / inttime;
            if rgb[i] > SAT_FREQ {
                return Err(Error::Saturated);
            }
        }
        rmeas = counts;

        // Channels with too few edges for 0.25% quantization need re-reading.
        let mut mask = 0u8;
        for i in 0..3 {
            if rmeas[i] < 200.0 {
                mask |= 1 << i;
            }
        }

        if mask != 0 {
            // Stage 2: pre-measurement to estimate the period per channel.
            // Channels with a workable frequency count skip it (their counts
            // convert directly to a period equivalent).
            let mut premask = 0u8;
            let pre_threshold = if self.is_munki_display() { 10.0 } else { 20.0 };
            for i in 0..3 {
                if mask & (1 << i) == 0 {
                    continue;
                }
                if rmeas[i] < pre_threshold {
                    premask |= 1 << i;
                } else {
                    let freq = rmeas[i] * 0.5 / inttime;
                    rmeas[i] = 0.5 * f64::from(edgec[i]) * CLK_FREQ / freq;
                }
            }

            if premask != 0 {
                let clocks = match self.period_measure(edgec, premask) {
                    Ok(c) => c,
                    Err(Error::InstrumentError(STATUS_PERIOD_FAIL)) => {
                        return self.long_freq_fallback();
                    }
                    Err(e) => return Err(e),
                };
                for i in 0..3 {
                    if premask & (1 << i) != 0 {
                        rmeas[i] = clocks[i];
                        if rmeas[i] >= 0.5 {
                            rgb_trial[i] = CLK_FREQ * 0.5 * f64::from(edgec[i]) / rmeas[i];
                        }
                    }
                }

                // Second pre-measurement at a ~0.1 s edge target, for better
                // period estimates. Skipped on the ColorMunki Display (slow).
                if !self.is_munki_display() {
                    let mut premask2 = 0u8;
                    for i in 0..3 {
                        if premask & (1 << i) == 0 || rmeas[i] <= 0.5 {
                            continue;
                        }
                        let nedgec =
                            (f64::from(edgec[i]) * 0.1 * CLK_FREQ / rmeas[i]).clamp(2.0, 65534.0);
                        let nedgec = 2 * (nedgec / 2.0).floor() as u16;
                        if nedgec > edgec[i] {
                            premask2 |= 1 << i;
                            edgec[i] = nedgec;
                        }
                    }
                    if premask2 != 0 {
                        let clocks = match self.period_measure(edgec, premask2) {
                            Ok(c) => c,
                            Err(Error::InstrumentError(STATUS_PERIOD_FAIL)) => {
                                return self.long_freq_fallback();
                            }
                            Err(e) => return Err(e),
                        };
                        for i in 0..3 {
                            if premask2 & (1 << i) != 0 {
                                rmeas[i] = clocks[i];
                            }
                            if rmeas[i] >= 0.5 {
                                rgb_trial[i] = CLK_FREQ * 0.5 * f64::from(edgec[i]) / rmeas[i];
                            }
                        }
                    }
                }
            }

            // Stage 3: per-channel target integration times. Aim for 200
            // edges; when that's unreachable at the default time, lengthen
            // the integration on a square-root curve, capped at 6 s.
            let mut tintt = [DEFAULT_INTTIME; 3];
            let mut tinttime = DEFAULT_INTTIME;
            for i in 0..3 {
                if mask & (1 << i) == 0 || rmeas[i] <= 0.5 {
                    continue;
                }
                let nedgec = f64::from(edgec[i]) * DEFAULT_INTTIME * CLK_FREQ / rmeas[i];
                if nedgec < 200.0 {
                    let mint = DEFAULT_INTTIME / 6.0;
                    let bl = ((nedgec - mint) / (200.0 - mint)).max(0.0).sqrt();
                    let tedges = bl * (200.0 - mint) + mint;
                    tintt[i] = (tedges / (f64::from(edgec[i]) * CLK_FREQ / rmeas[i])).min(6.0);
                    tinttime = tinttime.max(tintt[i]);
                }
            }

            // Stage 4: final edge counts. Prefer the burst-wide target when
            // it doesn't stretch this channel's time by more than 10%.
            for i in 0..3 {
                if mask & (1 << i) == 0 {
                    continue;
                }
                if rmeas[i] > 0.5 {
                    let even_edges = |t: f64| -> u16 {
                        let n = (f64::from(edgec[i]) * t * CLK_FREQ / rmeas[i]).clamp(2.0, 65534.0);
                        2 * (n / 2.0).floor() as u16
                    };
                    let mut nedgec = even_edges(tintt[i]);
                    let onedgec = even_edges(tinttime);
                    let atintt = f64::from(onedgec) * rmeas[i] / (f64::from(edgec[i]) * CLK_FREQ);
                    if atintt < 1.1 * tinttime {
                        nedgec = onedgec;
                    }
                    if edgec[i] == nedgec {
                        // Same target as the pre-measurement — reuse it.
                        rgb[i] = CLK_FREQ * 0.5 * f64::from(edgec[i]) / rmeas[i];
                        mask &= !(1 << i);
                    } else {
                        edgec[i] = nedgec;
                    }
                } else {
                    // No edges seen at all: the channel is dark.
                    rgb[i] = 0.0;
                    mask &= !(1 << i);
                }
            }

            // Stage 5: the precise period measurement.
            if mask != 0 {
                let clocks = match self.period_measure(edgec, mask) {
                    Ok(c) => c,
                    Err(Error::InstrumentError(STATUS_PERIOD_FAIL)) => {
                        return self.long_freq_fallback();
                    }
                    Err(e) => return Err(e),
                };
                for i in 0..3 {
                    if mask & (1 << i) == 0 {
                        continue;
                    }
                    // A timed-out channel (patch got dimmer than the trial)
                    // reports 0 clocks; the trial value is more realistic.
                    rgb[i] = if clocks[i] < 0.5 {
                        rgb_trial[i]
                    } else {
                        CLK_FREQ * 0.5 * f64::from(edgec[i]) / clocks[i]
                    };
                }
            }
        }

        self.finish_rgb(rgb)
    }

    /// Rev B mitigation: when period measurement reports `0x83` (no edges
    /// within its window), fall back to one long frequency measurement.
    fn long_freq_fallback(&mut self) -> Result<[f64; 3]> {
        let (counts, inttime) = self.freq_measure(10.0)?;
        let rgb = counts.map(|c| 0.5 * c / inttime);
        self.finish_rgb(rgb)
    }

    /// Black-subtract, clamp, and saturation-check a raw Hz triple.
    fn finish_rgb(&mut self, mut rgb: [f64; 3]) -> Result<[f64; 3]> {
        for (v, &black) in rgb.iter_mut().zip(&self.black_hz) {
            *v = (*v - black).max(0.0);
            if *v > SAT_FREQ {
                return Err(Error::Saturated);
            }
        }
        Ok(rgb)
    }

    /// Position of the ambient-diffuser arm: `false` = display (sensor
    /// clear), `true` = ambient (diffuser over the sensor).
    pub fn diffuser_over_sensor(&mut self) -> Result<bool> {
        let recv = self.command(CommandCode::GetDiffPos, &[], CMD_TIMEOUT)?;
        Ok(recv[1] != 0)
    }

    /// One complete display measurement: adaptive measurement through the
    /// calibration matrix. (The diffuser-position check happens once per
    /// burst in [`Colorimeter::measure`].)
    fn take_xyz(&mut self) -> Result<Xyz> {
        let rgb = self.measure_emissive_hz()?;
        let xyz = calmat::mul3x3_vec(&self.matrix, rgb);
        Ok(Xyz {
            x: xyz[0],
            y: xyz[1],
            z: xyz[2],
        })
    }
}

impl Colorimeter for I1d3 {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn select_calibration(&mut self, id: CalibrationId) -> Result<()> {
        // One calibration: the EEPROM-derived default matrix. (Display-type
        // presets on this instrument are host-side CCSS spectral files —
        // out of scope for now.)
        if id.0 != 0 {
            return Err(Error::CalIndexOutOfRange { got: id.0, max: 0 });
        }
        Ok(())
    }

    fn measure(&mut self, n: usize) -> Result<Sample> {
        // The diffuser arm must be off the sensor for display measurement;
        // checked once per burst.
        if self.diffuser_over_sensor()? {
            return Err(Error::DiffuserInPath);
        }
        let mut xyz = Vec::with_capacity(n);
        for _ in 0..n {
            xyz.push(self.take_xyz()?);
        }
        // raw: None — the instrument's adaptive engine returns derived
        // frequencies, not fixed-exposure integer counts, so the confidence
        // layer uses its XYZ-repeat-scatter path.
        Ok(Sample { xyz, raw: None })
    }

    // measure_adaptive: trait default. Every measurement is already
    // internally adaptive (frequency/period selection per channel); the
    // X2-style fixed-fast-exposure tier doesn't map onto this engine.
    // raw_diagnostics: None — no fixed-exposure integer-count mode.
}

/// Parse `(major, minor)` out of a firmware string like `"v1.3"` — best
/// effort, `(0, 0)` when unrecognizable.
fn parse_version(s: &str) -> (u32, u32) {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = digits.split('.');
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version("v1.3"), (1, 3));
        assert_eq!(parse_version("2.10"), (2, 10));
        assert_eq!(parse_version("garbage"), (0, 0));
    }
}
