# tristim-display

A Wayland client for showing solid-color test patches on a chosen output —
the display half of a colorimeter measurement loop. It renders through the
compositor's normal client path (a layer-shell surface with an ordinary
`wl_buffer`), so what a colorimeter reads off the panel is what the
compositor *actually does* with a client's pixels, not a bypassed scanout.

```rust,no_run
use tristim_display::{PatchSurface, BufferFormat, DescriptionRequest, Mastering};

// Unmanaged 8-bit SDR: the compositor interprets the buffer by its default.
let mut patch = PatchSurface::open_sdr("DP-1")?;
patch.set_code_values([1.0, 1.0, 1.0])?;

// fp16 surface declaring PQ + BT.2020 via wp_color_management_v1.
let desc = DescriptionRequest {
    transfer_function: "st2084_pq".into(),
    primaries: "bt2020".into(),
    luminances: None,
    mastering: Some(Mastering {
        min_nits: 0.0005,
        max_nits: 400.0,
        max_cll_nits: 400.0,
        max_fall_nits: 200.0,
    }),
};
let mut hdr = PatchSurface::open("DP-4", BufferFormat::Xbgr16161616f, Some(desc))?;
hdr.set_code_values([0.5081, 0.5081, 0.5081])?; // PQ code ≈ 100 cd/m²
# Ok::<(), tristim_display::Error>(())
```

Code values are written to the buffer **verbatim** — no encoding or
interpretation. What they *mean* is the negotiated color description's job;
the crate reports the negotiation outcome (`description_state()`) and the
compositor's advertised capabilities so the caller can record the facts.

## Features

- **Patch surface** — fullscreen layer-shell surface in the `Overlay` layer
  on a named output, `exclusive_zone = -1` (doesn't reflow the desktop), no
  keyboard grab. 8-bit (`Xrgb8888`) or fp16 (`Xbgr16161616f`) buffers.
- **Color management** — parametric `wp_color_management_v1` image
  descriptions: named transfer function + primaries, optional luminances and
  mastering metadata. Negotiation outcome is exposed as
  `Pending / Ready / Failed`, and `query_capabilities()` /
  `color_capabilities()` report what the compositor supports.
- **Windowed patches** — `set_window_fraction(0.04)` paints a centered
  bright window on black, for OLED/ABL-limited peak measurement at
  industry-spec window sizes (the surface stays fullscreen so the desktop
  stays hidden); `set_border()` controls the surround.
- **Output enumeration** — `list_outputs()` returns name, description,
  make/model, and size per output.
- **Compositor identity** — `advertised_globals()` (protocol fingerprint)
  and `compositor_process()` (binary name via `SO_PEERCRED` +
  `/proc/<pid>/comm`), for tagging measurements with what produced them.

### Supported description parameters

| Transfer functions | Primaries |
|---|---|
| `bt1886`, `gamma22`, `gamma28`, `srgb`, `ext_srgb`, `st2084_pq`, `hlg` | `srgb`, `pal`, `ntsc`, `bt2020`, `dci_p3`, `display_p3`, `adobe_rgb` |

Names are the protocol's own enum entries. The `pq` module provides
`nits_to_pq` for computing PQ code values from target luminance.

## Current limitations (roadmap)

- **Parametric descriptions only** — ICC file/blob descriptions
  (`icc_file` / `icc_blob`) are not yet supported.
- **Render intent is fixed to `perceptual`** — intent selection
  (photo/movie/graphic) is planned as a `DescriptionRequest` field.
- Custom (non-named) primaries/transfer characteristics are not yet
  exposed.

## Requirements

- A Wayland compositor with `wlr-layer-shell` (required) and
  `wp_color_management_v1` (only for managed/HDR surfaces — SDR-unmanaged
  works without it).
- Linux. The compositor-identity lookup uses `SO_PEERCRED`, and the crate
  targets Wayland generally.

## Versioning note

`smithay-client-toolkit` (0.19), `wayland-client` (0.31), and
`wayland-protocols` (0.32, staging) types appear in this crate's public API
(notably the `Error` enum). Bumps of those dependencies are semver-breaking
here and will be released as such.

## Example

```sh
cargo run -p tristim-display --example show_patch -- --list
cargo run -p tristim-display --example show_patch -- --output DP-1 --color 1,0.5,0 --secs 5
cargo run -p tristim-display --example show_hdr_patch -- --output DP-4 --nits 100 --secs 5
```

## Part of tristim

This crate is the display half of
[tristim](https://github.com/computer-whisperer/tristim), a Wayland
compositor color-validation toolkit; `tristim-driver` is the matching
colorimeter half. Neither depends on the other — pair them, or use this
crate standalone wherever a known patch on a known output is useful.

## License

Dual MIT / Apache-2.0.
