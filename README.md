# tristim

A Rust toolkit for tristimulus colorimetry on Linux/Wayland: a clean-room
driver for Datacolor Spyder-family colorimeters, plus a standalone tool that
validates how a Wayland compositor actually reproduces color on each display.

tristim has two faces:

- **Reusable crates.** Other projects depend on these directly as a git
  Cargo dependency — most notably the Spyder device driver. The crates carry
  no compositor-specific assumptions.
- **A standalone compositor color-validation tool.** Point it at a display,
  let it drive test patches through the compositor's normal client path, and
  measure what the panel actually emits. It reports where color reproduction
  is faithful and where it drifts — white point across the brightness range,
  per-channel maximum emission, primaries and gamut, and EOTF/gamma response.

It is compositor-agnostic by design: it asks for colors the way any client
would and measures the result, rather than side-channeling into a specific
output's scanout. That makes it an independent check on the color behavior a
compositor builds — including
[prism](https://github.com/computer-whisperer/prism), which consumes these
crates from its own `prism-tune` calibration tool. Any closed-loop
calibration lives in the consuming project, not here.

**Status:** working measurement path on a Datacolor Spyder 2024 (USB
`085c:0a0b`). SDR and HDR (PQ / BT.2020 via `wp_color_management_v1`) patch
paths are operational. The general-purpose display-characterization analysis
is under active development.

**Affiliation:** unofficial. Not affiliated with, endorsed by, or sponsored by
Datacolor. "Spyder", "SpyderX", "SpyderX2", and "Spyder 2024" are Datacolor's
trademarks, referenced here only to identify supported hardware.

## Hardware support

| Product line   | USB ID        | Protocol            | Status |
|----------------|---------------|---------------------|--------|
| Spyder 2024    | `085c:0a0b`   | spydX2 (V3.4+)      | tested |
| SpyderX2       | `085c:0a0a`   | spydX2 (V3.4+)      | should work, untested |
| Original SpyderX | `085c:0a00` | spydX (older opset) | not handled (different opcode set) |

All targets use the vendor-class USB protocol reverse-engineered by Graeme Gill
for ArgyllCMS (`spectro/spydX2.c`). This is a clean-room Rust re-implementation
working from the documented wire format, not a code translation.

## Workspace layout

- `tristim-driver/` — device-generic colorimeter layer. The reusable core; no
  Wayland dependency. Exposes a `Colorimeter` trait (open a device with
  `open_any`), measurements as a device-agnostic `Sample` (absolute XYZ plus
  optional raw sensor counts), per-measurement [`MeasurementConfidence`] (σY/Y,
  Δu'v', floor σ vs. trust thresholds — computed from raw counts when present,
  else from XYZ-repeat scatter), a per-device `measure_adaptive` primitive for
  batch loops like dense 3D LUT calibration, and a `RawDiagnostics` capability
  for low-level characterization. The one implemented driver is the Datacolor
  `spyder` family (SpyderX2 / Spyder 2024); its wire protocol and calibration
  mechanics stay behind the trait.
- `tristim-display/` — Wayland layer-shell client that renders known SDR/HDR
  patches on a chosen output, with optional centered-window mode for
  ABL-limited OLED peak measurement.
- `tristim-capture/` — serde schema for capture files: the contract between
  the gatherer and the analysis/presentation tools. No heavy deps.
- `tristim-cli/` — the gatherer binary `tristim`. Subcommands:
  `list-outputs`, `info`, `measure` (one shot); `characterize` (sensor
  noise / trust sweep); `speed` (per-cell wall-time × repeat count probe);
  `integration` (sweep `setup.s2` integration time at one level);
  `gamut` (probe a format's reproduced gamut, optionally with adaptive
  integration via `--fast-integration MS`); `capture` (format × color-sequence
  sweep → capture JSON); `report` (analyze a capture).

ArgyllCMS source is referenced (read-only) under `refs/argyll/` for
protocol-decoding purposes. We don't link it.

## License

Dual MIT / Apache-2.0 (Rust ecosystem standard).

## Setup — udev rule

Datacolor colorimeters are vendor-class USB devices that need explicit access
for non-root users:

```sh
sudo cp 50-tristim.rules /etc/udev/rules.d/
sudo udevadm control --reload
# unplug + replug the colorimeter
```

After that the device is accessible to your logged-in user via systemd-logind's
`uaccess` tag — no group membership needed.

## How the display tool talks to the compositor

`tristim-display` is an ordinary Wayland client. It writes raw SDR RGB (or
PQ-encoded fp16 in HDR mode) into a `wl_buffer` and commits it on a
layer-shell surface — exactly the path any application's content takes. For
HDR it declares a `wp_color_management_v1` parametric description (PQ +
BT.2020 + mastering metadata) so the compositor treats the buffer as
already-encoded HDR content.

This is deliberate: the tool measures what the compositor legitimately does
with a client's color request, so the resulting characterization is an
honest check on the compositor's pipeline rather than a measurement of a
bypassed one.
