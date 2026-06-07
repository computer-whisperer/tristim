//! Datacolor Spyder-family drivers.
//!
//! Two protocol variants share one USB [`transport`]:
//!
//! * [`Spyder`] (this module) — SpyderX2 / Spyder 2024, the `spydX2.c`
//!   protocol. Hardware-validated.
//! * [`SpyderX`](spyderx::SpyderX) — the original SpyderX, the `spydX.c`
//!   protocol. **Untested port** — see its module docs.
//!
//! Both implement the device-generic [`Colorimeter`] trait. The wire protocol
//! and per-unit calibration mechanics live in the [`protocol`] and
//! [`measurement`] submodules; they're `pub` for device-aware tooling (the
//! crate examples) but are not part of the generic driver surface — capture
//! orchestration sees only the trait and [`Sample`].

pub mod measurement;
pub mod protocol;
pub mod spyderx;
pub mod transport;

use crate::colorimeter::{
    AdaptiveMeasurement, AdaptiveTier, CalibrationId, Colorimeter, DeviceInfo, Error,
    RawConversion, RawDiagnostics, ResetDiscipline, Result,
};
use crate::confidence::MeasurementConfidence;
use crate::sample::{RawRepeats, Sample};
use measurement::{
    Calibration, IntegrationError, RawMeasurement, Setup, encode_measure_request,
    override_integration, parse_calibration, parse_raw_measurement, parse_setup, raw_to_xyz,
};
use protocol::Opcode;
use rusb::{Context, DeviceHandle};
use std::time::{Duration, Instant};
use transport::pid;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Spyder-specific capability detail, not part of the generic [`DeviceInfo`].
/// Reachable on the concrete [`Spyder`] via [`Spyder::caps`] for device-aware
/// tooling.
#[derive(Debug, Clone)]
pub struct SpyderCaps {
    /// For Spyder 2024 firmware: the high-level command set is available
    /// (detected by the `09 08 01` signature at reply bytes `[17..=19]`).
    pub high_level_commands: bool,
    /// Spyder 2024 only: max display-type number the high-level commands accept.
    pub max_display_type: Option<u8>,
    /// Spyder 2024 only: bitmask of valid display-type numbers.
    pub display_type_mask: Option<u16>,
}

/// An opened Spyder-family colorimeter (covers SpyderX2 and Spyder 2024).
///
/// Holds the active calibration + setup (downloaded at open and on
/// [`select_calibration`](Colorimeter::select_calibration)), so trait
/// measurements need no per-call calibration argument.
pub struct Spyder {
    handle: DeviceHandle<Context>,
    pid: u16,
    info: DeviceInfo,
    caps: SpyderCaps,
    cal: Calibration,
    setup: Setup,
}

impl Spyder {
    /// Find and open the first Spyder-family device on the bus, selecting cal
    /// index 0 ("General") as the active calibration.
    ///
    /// Tries PIDs `SPYDER_2024` (0x0A0B) and `SPYDERX2` (0x0A0A) — both use the
    /// spydX2 protocol implemented here. The original `SPYDERX` (0x0A00) is not
    /// handled (different opcode set).
    pub fn open_any() -> Result<Self> {
        let (handle, pid) = transport::open_first(&[pid::SPYDER_2024, pid::SPYDERX2])?;

        // Provisional self with placeholder calibration so we can issue
        // commands; real info + cal are filled in below before returning.
        let mut dev = Self {
            handle,
            pid,
            info: DeviceInfo {
                vendor: "Datacolor".into(),
                model: model_name(pid).into(),
                serial: String::new(),
                firmware: (0, 0),
                usb_pid: pid,
            },
            caps: SpyderCaps {
                high_level_commands: false,
                max_display_type: None,
                display_type_mask: None,
            },
            cal: Calibration::placeholder(),
            setup: Setup::placeholder(),
        };

        // Vendor-class reset; without it the device receives bulk writes but
        // never replies. See `send_reset()` for details.
        dev.send_reset()?;
        let (firmware, serial, caps) = dev.read_info()?;
        dev.info.firmware = firmware;
        dev.info.serial = serial;
        dev.caps = caps;
        dev.select_calibration(CalibrationId(0))?;
        Ok(dev)
    }

    /// USB product ID of the device we opened.
    pub fn pid(&self) -> u16 {
        self.pid
    }

    /// True if this is the Spyder 2024 lineup (vs. SpyderX2).
    pub fn is_spyder_2024(&self) -> bool {
        self.pid == pid::SPYDER_2024
    }

    /// Spyder-specific capability detail (see [`SpyderCaps`]).
    pub fn caps(&self) -> &SpyderCaps {
        &self.caps
    }

    /// Vendor-class reset that the SpyderX2/2024 firmware requires before it will
    /// respond to bulk commands. Mirrors `spydX2_reset()` in ArgyllCMS.
    /// Identical request for SpyderX, X2, and 2024.
    pub fn send_reset(&self) -> Result<()> {
        transport::send_reset(&self.handle)
    }

    /// Execute one command against the device. See [`transport`] module docs
    /// for the wire format.
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

    /// Read hardware version + serial + extended capabilities (opcode `0xC2`).
    /// Returns the generic `(firmware, serial)` plus the Spyder-only [`SpyderCaps`].
    fn read_info(&mut self) -> Result<((u32, u32), String, SpyderCaps)> {
        let reply = self.command(Opcode::GetInfo, &[], 0x25, false, DEFAULT_TIMEOUT)?;

        let major =
            parse_ascii_int(&reply[0..1]).ok_or_else(|| Error::BadVersionString(reply.clone()))?;
        let minor =
            parse_ascii_int(&reply[2..4]).ok_or_else(|| Error::BadVersionString(reply.clone()))?;

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

        Ok((
            (major, minor),
            serial,
            SpyderCaps {
                high_level_commands: high_level,
                max_display_type,
                display_type_mask,
            },
        ))
    }

    /// Download the per-unit calibration matrix for `cal_index` (0..7 on Spyder
    /// 2024; index 0 = "General"). Issues opcode `0xF6`.
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

    /// One raw 6-channel reading at `setup`, auto-zeroing first (opcode `0xF2`).
    /// Pub for device-aware tooling that drives an explicit setup; the generic
    /// path is [`Colorimeter::measure`].
    pub fn measure_raw_once(&mut self, setup: &Setup) -> Result<RawMeasurement> {
        // Argyll resets before every measurement (auto-zero behavior).
        self.send_reset()?;
        self.measure_once_no_reset(setup)
    }

    /// One raw reading at `setup` without the auto-zero reset. The caller must
    /// have issued [`send_reset`](Self::send_reset) at least once first.
    fn measure_once_no_reset(&mut self, setup: &Setup) -> Result<RawMeasurement> {
        let send = encode_measure_request(setup);
        // No checksum on the measurement reply per spydX2_Measure (last arg 0).
        // Integration time alone can be ~720 ms, so allow generous slack.
        let reply = self.command(Opcode::Measure, &send, 0xc, false, Duration::from_secs(3))?;
        Ok(parse_raw_measurement(&reply)?)
    }

    /// `n` raw readings at `setup` under the given reset discipline. `auto_zero`
    /// resets before every reading; otherwise it resets once and bursts.
    fn measure_repeated(
        &mut self,
        setup: &Setup,
        n: usize,
        auto_zero: bool,
    ) -> Result<Vec<RawMeasurement>> {
        let discipline = if auto_zero {
            ResetDiscipline::AutoZeroEach
        } else {
            ResetDiscipline::BurstOnce
        };
        let (raws, _times) = self.timed_repeated(setup, n, discipline)?;
        Ok(raws)
    }

    /// `n` raw readings at `setup` plus the per-reading wall times (ms). The one
    /// timing-aware primitive both [`measure_repeated`](Self::measure_repeated)
    /// and the [`RawDiagnostics`] methods share.
    fn timed_repeated(
        &mut self,
        setup: &Setup,
        n: usize,
        discipline: ResetDiscipline,
    ) -> Result<(Vec<RawMeasurement>, Vec<f64>)> {
        let mut raws = Vec::with_capacity(n);
        let mut times = Vec::with_capacity(n);
        if n == 0 {
            return Ok((raws, times));
        }
        if discipline == ResetDiscipline::BurstOnce {
            self.send_reset()?;
        }
        for _ in 0..n {
            let t0 = Instant::now();
            let raw = match discipline {
                ResetDiscipline::AutoZeroEach => self.measure_raw_once(setup)?,
                ResetDiscipline::BurstOnce => self.measure_once_no_reset(setup)?,
            };
            times.push(t0.elapsed().as_secs_f64() * 1000.0);
            raws.push(raw);
        }
        Ok((raws, times))
    }

    /// End-to-end XYZ measurement using calibration index `cal_index`. Kept as a
    /// concrete convenience for device-aware tooling; the generic path is
    /// [`Colorimeter::measure`].
    pub fn measure_xyz(
        &mut self,
        cal_index: u8,
    ) -> Result<(crate::sample::Xyz, RawMeasurement, Calibration, Setup)> {
        let cal = self.get_calibration(cal_index)?;
        let setup = self.get_setup(&cal)?;
        let raw = self.measure_raw_once(&setup)?;
        let xyz = raw_to_xyz(&raw, &setup, &cal);
        Ok((xyz, raw, cal, setup))
    }
}

/// Build a device-agnostic [`Sample`] from raw Spyder readings and the
/// calibration that scales them. The single point where Spyder counts become
/// generic XYZ + [`RawRepeats`] (with the per-channel ∂XYZ/∂count gradient the
/// confidence layer needs).
fn raws_to_sample(raws: &[RawMeasurement], setup: &Setup, cal: &Calibration) -> Sample {
    let counts: Vec<Vec<u32>> = raws
        .iter()
        .map(|r| r.0.iter().map(|&c| c as u32).collect())
        .collect();
    let floor: Vec<f64> = setup.s5.iter().map(|&s| s as f64).collect();
    // ∂XYZ/∂count for channel j is the j-th matrix column scaled by per-row gain.
    let grad: Vec<[f64; 3]> = (0..6)
        .map(|j| {
            [
                cal.matrix[0][j] * cal.gain[0],
                cal.matrix[1][j] * cal.gain[1],
                cal.matrix[2][j] * cal.gain[2],
            ]
        })
        .collect();
    let xyz: Vec<crate::sample::Xyz> = raws.iter().map(|r| raw_to_xyz(r, setup, cal)).collect();
    Sample {
        xyz,
        raw: Some(RawRepeats {
            counts,
            floor,
            grad,
        }),
    }
}

fn model_name(pid: u16) -> &'static str {
    if pid == pid::SPYDER_2024 {
        "Spyder 2024"
    } else {
        "SpyderX2"
    }
}

/// Display-type preset names by calibration index, as ArgyllCMS documents them
/// (`spydX2_disptypesel` / `spyd2024_disptypesel` in `spectro/spydX2.c`).
/// Index 0 ("General") is the calibration-base type; the rest are
/// panel-technology presets.
const SPYDERX2_CAL_NAMES: [&str; 5] = [
    "General",
    "Standard LED",
    "Wide Gamut LED",
    "GB LED",
    "High Brightness",
];
const SPYDER_2024_CAL_NAMES: [&str; 7] = [
    "General",
    "Standard LED",
    "Wide Gamut LED",
    "GB LED",
    "High Brightness",
    "OLED",
    "Mini-LED",
];

impl Colorimeter for Spyder {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn calibrations(&self) -> Vec<crate::colorimeter::CalibrationDesc> {
        let names: &[&str] = if self.is_spyder_2024() {
            &SPYDER_2024_CAL_NAMES
        } else {
            &SPYDERX2_CAL_NAMES
        };
        // 2024 high-level firmware advertises which display-type numbers are
        // valid; honor that over the static table. Bits beyond the known
        // names still list (as "Display type N") — the firmware says they
        // exist, we just don't know what Datacolor calls them. An all-zero
        // mask is firmware nonsense (cal 0 always exists — we selected it at
        // open); treat it as "no mask" rather than enumerating nothing.
        let mask = self.caps.display_type_mask.filter(|&m| m != 0);
        let valid = |i: usize| mask.is_none_or(|m| m & (1 << i) != 0);
        let count = match mask {
            Some(m) => names.len().max(16 - m.leading_zeros() as usize),
            None => names.len(),
        };
        (0..count)
            .filter(|&i| valid(i))
            .map(|i| crate::colorimeter::CalibrationDesc {
                id: CalibrationId(i as u8),
                name: names
                    .get(i)
                    .map_or_else(|| format!("Display type {i}"), |n| (*n).to_string()),
            })
            .collect()
    }

    fn select_calibration(&mut self, id: CalibrationId) -> Result<()> {
        let cal = self.get_calibration(id.0)?;
        let setup = self.get_setup(&cal)?;
        self.cal = cal;
        self.setup = setup;
        Ok(())
    }

    fn measure(&mut self, n: usize) -> Result<Sample> {
        let setup = self.setup.clone();
        let raws = self.measure_repeated(&setup, n, true)?;
        Ok(raws_to_sample(&raws, &self.setup, &self.cal))
    }

    fn measure_adaptive(
        &mut self,
        repeats: usize,
        fast_ms: Option<u16>,
    ) -> Result<AdaptiveMeasurement> {
        let fast_pair =
            fast_ms.and_then(|ms| override_integration(&self.setup, &self.cal, ms).ok());

        let Some((setup_fast, cal_fast)) = fast_pair else {
            // No usable override — one default-exposure burst.
            let setup = self.setup.clone();
            let raws = self.measure_repeated(&setup, repeats, false)?;
            return Ok(AdaptiveMeasurement {
                sample: raws_to_sample(&raws, &self.setup, &self.cal),
                tier: AdaptiveTier::SingleFull,
            });
        };

        let raws_fast = self.measure_repeated(&setup_fast, repeats, false)?;
        let sample_fast = raws_to_sample(&raws_fast, &setup_fast, &cal_fast);
        if MeasurementConfidence::from_sample(&sample_fast).is_trustworthy() {
            return Ok(AdaptiveMeasurement {
                sample: sample_fast,
                tier: AdaptiveTier::Fast,
            });
        }

        // Fast tier untrustworthy: re-measure at the calibrated default.
        let setup = self.setup.clone();
        let raws_full = self.measure_repeated(&setup, repeats, false)?;
        Ok(AdaptiveMeasurement {
            sample: raws_to_sample(&raws_full, &self.setup, &self.cal),
            tier: AdaptiveTier::EscalatedFull,
        })
    }

    fn raw_diagnostics(&mut self) -> Option<&mut dyn RawDiagnostics> {
        Some(self)
    }

    fn raw_conversion(&self) -> Option<RawConversion> {
        // Mirrors `measurement::raw_to_xyz`: subtract the `s5` floor from the
        // 6 sensor counts, apply the active cal's 3×6 matrix, then the
        // per-row gain and offset.
        Some(RawConversion {
            black_floor: self.setup.s5.iter().map(|&v| v as f64).collect(),
            matrix: std::array::from_fn(|i| self.cal.matrix[i].to_vec()),
            gain: self.cal.gain,
            offset: self.cal.offset,
        })
    }
}

impl RawDiagnostics for Spyder {
    fn reset(&mut self) -> Result<()> {
        self.send_reset()
    }

    fn measure_raw(&mut self, n: usize, discipline: ResetDiscipline) -> Result<(Sample, Vec<f64>)> {
        let setup = self.setup.clone();
        let (raws, times) = self.timed_repeated(&setup, n, discipline)?;
        Ok((raws_to_sample(&raws, &self.setup, &self.cal), times))
    }

    fn measure_raw_at(
        &mut self,
        n: usize,
        integration_ms: u16,
        discipline: ResetDiscipline,
    ) -> Result<(Sample, Vec<f64>)> {
        let (setup_at, cal_at) = override_integration(&self.setup, &self.cal, integration_ms)
            .map_err(|IntegrationError::OutOfRange { got, min, max }| {
                Error::IntegrationOutOfRange { got, min, max }
            })?;
        let (raws, times) = self.timed_repeated(&setup_at, n, discipline)?;
        Ok((raws_to_sample(&raws, &setup_at, &cal_at), times))
    }

    fn integration_range(&self) -> Option<(u16, u16)> {
        Some((measurement::MIN_INTEGRATION_MS, self.cal.integration_ms()))
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
