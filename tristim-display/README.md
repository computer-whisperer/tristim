# tristim-display

A Wayland client for showing solid-color test patches on a chosen output —
the display half of a colorimeter measurement loop. It renders through the
compositor's normal client path (a layer-shell surface with an ordinary
`wl_buffer`), so what a colorimeter reads off the panel is what the
compositor *actually does* with a client's pixels, not a bypassed scanout.

```rust,no_run
use tristim_display::{
    PatchSurface, BufferFormat, DescriptionRequest, Mastering, ParametricDescription,
};

// Unmanaged 8-bit SDR: the compositor interprets the buffer by its default.
let mut patch = PatchSurface::open_sdr("DP-1")?;
patch.set_code_values([1.0, 1.0, 1.0])?;

// fp16 surface declaring PQ + BT.2020 via wp_color_management_v1.
let mut params = ParametricDescription::named("st2084_pq", "bt2020");
params.mastering = Some(Mastering {
    luminance_nits: Some((0.0005, 400.0)),
    max_cll_nits: Some(400.0),
    max_fall_nits: Some(200.0),
    ..Default::default()
});
let desc = DescriptionRequest::parametric(params);
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
  keyboard grab.
- **Every RGB `wl_shm` buffer format** — 48 formats from `rgb332` through
  the 4444/1555/565 families, 8-bit, 10-bit packed, 16-bit unorm, and fp16,
  all driven by one table-based encoder (`BufferFormat::encode`). Float
  formats carry extended-range values (negative / >1.0) verbatim, as scRGB
  requires. YUV is deliberately excluded: writing it would mean color
  conversion, i.e. interpreting the code values, which this crate refuses
  to do.
- **The full parametric `wp_color_management_v1` surface** — all 14 named
  transfer functions (incl. `ext_linear` and v2's `compound_power_2_4`) and
  all 10 named primaries, power-law TFs (`set_tf_power`), custom CIE-xy
  primaries (`set_primaries`), luminances, ST 2086 mastering metadata
  (luminance, display primaries, max CLL/FALL), render-intent selection,
  and the `windows_scrgb` description shortcut. Negotiation outcome is
  exposed as `Pending / Ready / Failed`.
- **Pipeline planning** — consumers think in *representations*
  (`RenderMode`: a color description + `BufferPolicy`), not buffer
  formats. Under `BufferPolicy::Auto` the crate picks an adequate
  advertised buffer (float required for extended-range encodings like
  scRGB, fp16 preferred then deep unorm for PQ/HLG, 8-bit for SDR);
  `BufferPolicy::Exact` pins one for when the buffer itself is the
  question. `DisplayCapabilities::plan()` is the pre-flight twin of
  `PatchSurface::open_mode()` — same checks, no connection — returning
  the chosen buffer + "limited by" notes, or a typed `Unarrangeable`
  with chip-sized reason text.
- **Capability gating everywhere** — optional protocol requests are fatal
  protocol errors when the compositor didn't advertise the feature, so
  planning/attaching checks the advertised features/TFs/primaries/intents
  first and refuses client-side instead of dying. `query_capabilities()`
  reports advertised buffer formats and color capabilities up front.
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

| | Named values |
|---|---|
| Transfer functions | `bt1886`, `gamma22`, `gamma28`, `st240`, `ext_linear`, `log_100`, `log_316`, `xvycc`, `srgb`, `ext_srgb`, `st2084_pq`, `st428`, `hlg`, `compound_power_2_4` — or a power-law exponent (1.0–10.0) |
| Primaries | `srgb`, `pal_m`, `pal`, `ntsc`, `generic_film`, `bt2020`, `cie1931_xyz`, `dci_p3`, `display_p3`, `adobe_rgb` — or custom CIE-xy coordinates |
| Render intents | `perceptual` (baseline), `relative`, `saturation`, `absolute`, `relative_bpc`, `absolute_no_adaptation` |

Names are the protocol's own enum entries. The `pq` module provides
`nits_to_pq` for computing PQ code values from target luminance.

## Current limitations (roadmap)

- **ICC descriptions** — file/blob descriptions (`icc_file` / `icc_blob`)
  are not yet supported; the parametric path and `windows_scrgb` are.

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
