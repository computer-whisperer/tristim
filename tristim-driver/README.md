# tristim-driver

A clean-room Rust driver for Datacolor Spyder-family colorimeters, behind a
device-generic `Colorimeter` trait. No Wayland or display dependency — this
crate talks USB and returns absolute CIE XYZ; what you point the puck at is
your business.

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

| Product line     | USB ID      | Protocol            | Status |
|------------------|-------------|---------------------|--------|
| Spyder 2024      | `085c:0a0b` | spydX2 (V3.4+)      | tested |
| SpyderX2         | `085c:0a0a` | spydX2 (V3.4+)      | should work, untested |
| Original SpyderX | `085c:0a00` | spydX (older opset) | not handled (different opcode set) |

The wire protocol was reverse-engineered by Graeme Gill for ArgyllCMS
(`spectro/spydX2.c`). This crate is a clean-room Rust re-implementation
working from the documented wire format, not a code translation, and does
not link ArgyllCMS.

**Unofficial.** Not affiliated with, endorsed by, or sponsored by Datacolor.
"Spyder", "SpyderX", "SpyderX2", and "Spyder 2024" are Datacolor's
trademarks, referenced only to identify supported hardware.

## API layers

- **`Colorimeter` trait** — the device-generic surface. `open_any()` probes
  the bus and returns the first supported instrument with a sensible default
  calibration selected. `measure(n)` takes `n` repeats and returns a
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
- **`spyder` module** — the concrete driver. Device-aware tooling can reach
  the calibration matrices, setup block, and wire protocol directly; generic
  consumers never need to.

## Setup — udev rule

Spyders are vendor-class USB devices that need explicit access for non-root
users. Install a udev rule tagging them `uaccess` (see
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
