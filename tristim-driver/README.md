# tristim-driver

Clean-room Rust drivers for display colorimeters (Datacolor Spyder family,
X-Rite i1Display Pro family), behind a device-generic `Colorimeter` trait.
No Wayland or display dependency — this crate talks USB and returns absolute
CIE XYZ; what you point the puck at is your business.

```rust,no_run
use tristim_driver::{MeasurementConfidence, open_any};

fn main() -> tristim_driver::Result<()> {
    let mut device = open_any()?; // Box<dyn Colorimeter>, default calibration selected
    let sample = device.measure(3)?; // 3 repeats: absolute XYZ + raw counts
    for xyz in &sample.xyz {
        print!("Y = {:.2} cd/m²", xyz.y);
        if let Some((x, y)) = xyz.chromaticity() {
            print!("  xy = ({:.4}, {:.4})", x, y);
        }
        println!();
    }
    let conf = MeasurementConfidence::from_sample(&sample);
    println!("trustworthy: {} ({:?})", conf.is_trustworthy(), conf.flags());
    Ok(())
}
```

## Hardware support

| Product line | USB ID | Protocol | Status |
|---|---|---|---|
| Spyder 2024 | `085c:0a0b` | spydX2 | tested |
| SpyderX2 | `085c:0a0a` | spydX2 | should work, untested |
| Original SpyderX | `085c:0a00` | spydX | **untested** — same framing as spydX2, different opcodes |
| i1Display Pro / ColorMunki Display | `0765:5020` | i1d3 | **untested** — covers the OEM rebadges too (NEC SpectraSensor Pro, HP DreamColor, …) |

Untested drivers were written from the documented wire formats without
hardware on hand; validation reports are very welcome. `open_any()` prefers
hardware-validated drivers over untested ones when several devices are
plugged in.

Not supported: earlier Spyders (1–5; the Spyder 2 needs a vendor firmware
blob and the Spyder 4/5 need vendor spectral calibration data, neither
redistributable) and spectrometers (a different instrument class).

The wire protocols were reverse-engineered by Graeme Gill for ArgyllCMS
(`spectro/spydX2.c`, `spectro/spydX.c`, `spectro/i1d3.c`). This crate is a
clean-room Rust re-implementation working from the documented wire formats,
not a code translation, and does not link ArgyllCMS.

Driver-specific notes:

- **Original SpyderX** — supports an optional user-side dark calibration
  (lens cap on): `SpyderX::dark_calibrate()`. Without it, only readings
  very close to black carry a small uncorrected residual.
- **i1d3 family** — XYZ comes from a 3×3 matrix computed from the per-unit
  sensor spectral sensitivities in the instrument's EEPROM against the
  CIE 1931 2° observer (ArgyllCMS's own default calibration). Display-type
  spectral corrections (CCSS), ambient mode, refresh-synchronized
  integration, and the Rev-B AIO mode are not implemented yet.

**Unofficial.** Not affiliated with, endorsed by, or sponsored by Datacolor
or X-Rite. "Spyder", "SpyderX", "SpyderX2", and "Spyder 2024" are
Datacolor's trademarks; "i1Display" and "ColorMunki" are X-Rite's. All are
referenced only to identify supported hardware.

## API layers

- **`Colorimeter` trait** — the device-generic surface. `open_any()` probes
  the bus and returns the first supported instrument with a sensible default
  calibration selected; when nothing opens, the error distinguishes a
  permission failure (udev rule missing), a known vendor's unsupported
  model, and an empty bus. `calibrations()` lists the on-board display-type
  presets by id and name. `measure(n)` takes `n` repeats and returns a
  [`Sample`]: absolute XYZ plus raw 6-channel sensor counts when the device
  exposes them.
- **`MeasurementConfidence`** — trust metrics computed from any `Sample`
  (`MeasurementConfidence::from_sample`), derived from raw counts when
  present (else from XYZ-repeat scatter): relative
  luminance uncertainty (σY/Y), chromaticity uncertainty (Δu′v′), and
  black-cal floor proximity, each compared against documented thresholds and
  summarized as `TrustFlag`s (`Floor`, `Noisy`, `Chroma`).
- **`measure_adaptive`** — a two-tier primitive for batch loops (e.g. dense
  3D-LUT characterization): take a fast short-exposure burst first, keep it
  if trustworthy, escalate to a default-exposure burst otherwise. The
  returned `AdaptiveTier` records which path produced the data.
- **`RawDiagnostics`** — optional low-level capability (reset discipline
  control, integration-time override, per-reading wall times) for sensor
  characterization tooling. Fetched via `Colorimeter::raw_diagnostics()`;
  `None` on devices that only expose XYZ.
- **`spyder` / `i1d3` modules** — the concrete drivers. Device-aware tooling
  can reach the calibration matrices, setup blocks, and wire protocols
  directly; generic consumers never need to.

## Setup — udev rule

These instruments need explicit USB access for non-root users. Install a
udev rule tagging them `uaccess` (see
[`50-tristim.rules`](https://github.com/computer-whisperer/tristim/blob/main/50-tristim.rules)
in the repository):

```sh
sudo cp 50-tristim.rules /etc/udev/rules.d/
sudo udevadm control --reload
# unplug + replug the colorimeter
```

## Examples

- `probe` — enumerate and open supported devices
- `measure` — one XYZ measurement with raw counts and the calibration matrix
- `dump_calibrations` — full diagnostic dump of the on-board calibrations

```sh
cargo run -p tristim-driver --example measure
```

## Part of tristim

This crate is the hardware layer of
[tristim](https://github.com/computer-whisperer/tristim), a Wayland
compositor color-validation toolkit — but it has no dependency on the rest
of the workspace and is designed to be consumed standalone (e.g. by
closed-loop display calibration tools).

## License

Dual MIT / Apache-2.0.
