# SpyderExpress 2024 (PID 0x0A0B) protocol notes

## TL;DR — solved

The 2024 Spyder lineup speaks a **different opcode set** from the original SpyderX (2019). The wire framing, endpoints, and reset control transfer are identical, but the command opcodes diverged. ArgyllCMS V3.4.0 added support; V3.5.0 fixed cases (like ours) where the firmware doesn't advertise high-level commands.

Our Rust driver works end-to-end as of 2026-05-19 evening:

```
opened device, USB PID = 0x0a0b
(family: Spyder 2024)

--- 0xC2 (get device info) ---
hw version:     6.00
serial:         "87000216"
high-level cmds:false

--- 0xF6 (get calibration matrix, index 0) ---
raw 108-byte reply: 00 03 02 ca 00 01 02 03 04 05 3e c7 87 3c ... 01 fe   ← checksum verified
```

## Protocol summary

### Shared with SpyderX

- USB endpoints: bulk OUT `0x01`, bulk IN `0x81`, both 64-byte max-packet
- Vendor-class reset before first bulk command: `bmRequestType=0x41, bRequest=0x02, wValue=2, wIndex=0, no data, 500ms sleep`
- Wire format: 5-byte header (opcode + nonce_u16_BE + size_u16_BE) + payload
- Reply header: nonce echo (u16 BE) + instrument error (u8) + size (u16 BE) + payload
- Optional checksum: last payload byte is `(sum of preceding payload bytes) & 0xFF`

### Different from SpyderX

| Function | SpyderX (`0x0A00`) | SpyderX2 / 2024 (`0x0A0A`, `0x0A0B`) |
|----------|--------------------|---------------------------------------|
| Get HW version | `0xD9` (23-byte reply, version only) | folded into `0xC2` |
| Get info (version + serial + caps) | `0xC2` (37-byte reply, serial only) | **`0xC2`** (37-byte reply, version + serial + 2024 caps) |
| Get calibration matrix | `0xCB` (42-byte reply) | **`0xF6`** (108-byte reply, more cal data) |
| Get measurement setup | `0xC3` (10-byte reply) | **`0xF7`** (22-byte reply) |
| Take measurement | `0xD2` (7-byte send / 8-byte reply) | **`0xF2`** (15-byte send / 12-byte reply) |
| High-level measure (2024 only) | — | **`0xFA`** (1-byte send / 13-byte reply) |
| Ambient measure | `0xD4` | `0xD4` (unchanged) |

### Spyder 2024 firmware variants

The 2024 lineup ships in at least two firmware variants distinguished by capability advertisement in the `0xC2` reply:

- **High-level enabled**: bytes `[17..=19] == 09 08 01`. Use `0xFA` to take measurements; the device handles display-type-specific calibration internally and returns XYZ directly.
- **Low-level only** (our device, firmware 6.00): bytes `[17..=19] == 09 08 00`. Must use `0xF6`/`0xF7`/`0xF2` flow — download per-unit cal matrix, fetch setup, trigger measurement, convert raw sensor counts to XYZ using the downloaded matrix.

ArgyllCMS V3.5.0's bugfix was for the low-level fallback (per changelog).

## Key files in the V3.5.0 reference

All under `/home/christian/workspace/spyder/refs/argyll-3.5.0/spectro/`:

- `spydX2.c` (1836 lines) — driver implementation
- `spydX2.h` (164 lines) — types + state struct
- `insttypes.c:475` — PID `0x0A0B` → `instSpyder2024` mapping
- `insttypes.c:472` — PID `0x0A0A` → `instSpyderX2` mapping

## Where the Rust driver stands

- `spyder-driver/src/protocol.rs` — opcode constants + wire-format docs
- `spyder-driver/src/device.rs` — `Spyder` handle with `open_any()`, `command()`, `get_info()`. ~280 LOC.
- `spyder-driver/examples/probe.rs` — proof-of-life, dumps info and calibration matrix raw bytes

## Next steps (no longer blocked)

1. **Parse `0xF6` reply into a calibration matrix.** The 108 bytes include floats (visible `3e c7 87 3c` = 0x3c87c73e in little-endian = ~0.0166 as IEEE 754 — looks like big-endian f32 in the wire). See `spydX2_getCalibration()` for the exact field layout.
2. **Parse `0xF7` reply into setup parameters** (gain, integration time, black-cal trim values).
3. **Implement `measure()` using `0xF2`**: send setup params, read raw sensor counts.
4. **Convert raw → XYZ** using the calibration matrix.
5. **Write `spyder-display`**: layer-shell Wayland client to put a known RGB patch on a chosen output.
6. **Write `spyder-cli`**: orchestrate `characterize` (patch series), `profile` (fit model), `verify`.
