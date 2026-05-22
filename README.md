# tristim

A Rust toolkit for driving tristimulus colorimeters from Linux/Wayland and using them to measure + characterize + calibrate displays. Built to close the loop on the [prism compositor](../compositor/prism/) — sweep measurements feed into per-monitor color calibration data that the compositor applies via CTM, GAMMA_LUT, and shader-side response correction.

**Status:** working measurement path on a Datacolor Spyder 2024 (USB `085c:0a0b`). HDR PQ sweep mode via `wp_color_management_v1` is operational; per-channel response curve fitting against compositor IPC is the current closed-loop target.

**Affiliation:** unofficial. Not affiliated with, endorsed by, or sponsored by Datacolor. "Spyder", "SpyderX", "SpyderX2", and "Spyder 2024" are Datacolor's trademarks, referenced here only to identify supported hardware.

## Hardware support

| Product line   | USB ID        | Protocol            | Status |
|----------------|---------------|---------------------|--------|
| Spyder 2024    | `085c:0a0b`   | spydX2 (V3.4+)      | tested |
| SpyderX2       | `085c:0a0a`   | spydX2 (V3.4+)      | should work, untested |
| Original SpyderX | `085c:0a00` | spydX (older opset) | not handled (different opcode set) |

All targets use the vendor-class USB protocol reverse-engineered by Graeme Gill for ArgyllCMS (`spectro/spydX2.c`). This is a clean-room Rust re-implementation working from the documented wire format, not a code translation.

## Workspace layout

- `tristim-driver/` — rusb-based device protocol (init, calibration-data download, measurement)
- `tristim-display/` — Wayland layer-shell client that renders known SDR/HDR patches on a chosen output, with optional centered-window mode for ABL-limited OLED peak measurement
- `tristim-cli/` — orchestrator binary `tristim` (`info`, `measure`, `sweep`, `analyze`)

ArgyllCMS source is referenced (read-only) under `refs/argyll/` for protocol-decoding purposes. We don't link it.

## License

Dual MIT / Apache-2.0 (Rust ecosystem standard).

## Setup — udev rule

Datacolor colorimeters are vendor-class USB devices that need explicit access for non-root users:

```sh
sudo cp 50-tristim.rules /etc/udev/rules.d/
sudo udevadm control --reload
# unplug + replug the colorimeter
```

After that the device is accessible to your logged-in user via systemd-logind's `uaccess` tag — no group membership needed.

## Coordination with the compositor

`tristim-display` writes raw SDR RGB (or PQ-encoded fp16 in HDR mode) into a wl_buffer and pairs with a compositor that scans those pixels out without intervening color transforms. For HDR, that means declaring `wp_color_management_v1` parametric description (PQ + BT.2020 + mastering metadata) so the compositor / display pipeline treats the buffer as already-encoded HDR content.

Closed-loop calibration uses the prism IPC (`prism-tune msg output …`) to flip the compositor's per-output response correction live between sweep iterations — the resulting fit lands in the per-output KDL config.
