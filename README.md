# spyder

A Rust toolkit for driving the Datacolor SpyderExpress colorimeter from Linux/Wayland and using it to measure + characterize + calibrate displays. Built specifically to close the loop on the [compositor project](../compositor/) — measurements feed into per-monitor color calibration data that the compositor applies via CTM + LUTs.

**Status:** scaffolding. No working measurement path yet.

## Hardware target

- **Datacolor SpyderExpress** (Spyder 2024 lineup) — USB `085c:0a0b`
- Vendor-specific USB class, 2 bulk endpoints, accessed via `rusb`
- First-party Datacolor software is Windows-only; ArgyllCMS support varies by PID

## Approach

Roll our own:
- `spyder-driver/` — rusb-based device protocol (init, calibration-data download, measurement)
- `spyder-display/` — Wayland layer-shell client that renders known RGB patches on a chosen output
- `spyder-cli/` — orchestrator (`probe`, `measure`, `characterize`, `profile`, `verify`)

ArgyllCMS source is referenced (read-only) under `refs/argyll/` for protocol-decoding purposes. We don't link it.

## License

Dual MIT / Apache-2.0 (Rust ecosystem standard).

## Setup — udev rule

The Spyder is a vendor-class USB device that requires explicit access for non-root users:

```sh
sudo cp 50-spyder.rules /etc/udev/rules.d/
sudo udevadm control --reload
# unplug + replug the Spyder
```

After that the device should be accessible to your logged-in user via systemd-logind's `uaccess` tag — no group membership needed.

## Coordination with the compositor

Initially `spyder-display` writes raw RGB into a wl_buffer and trusts niri to display untouched pixels (currently true — niri does no color processing). Once the compositor's color pipeline lands, `spyder-display` will need a "do not color-transform this surface" signal (eventually via `wp_color_management_v1` pass-through mode, until then via a custom niri-ipc opt-out).

Output of `spyder profile` is a calibration file (TOML, schema TBD) that niri-config will reference per-output. The actual application of the calibration happens in the compositor (CTM + GAMMA_LUT atomic commits, shader-side 3D LUT if needed) — this tool just generates the data.
