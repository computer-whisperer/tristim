# tristim-gui

An [aetna](https://github.com/computer-whisperer/aetna)-based presenter for
tristim captures. It loads a capture JSON, runs `tristim-analyze`, and opens a
window visualizing how faithfully a compositor reproduced color on the output
under test — per-trial aggregate statistics now, a CIE chromaticity field with
per-sample error vectors and tone-response plots to follow.

This is the *presentation* half of tristim's gather/present split. It holds no
measurement logic of its own; it is a view over an
`tristim_analyze::AnalyzedCapture`.

## Why this crate is outside the workspace

`tristim-gui` depends on aetna through a **local path dependency**
(`../../aetna/aetna.main`), which CI cannot fetch and which drags in the GPU
stack (wgpu, winit, …). To keep the portable backend libraries — the crates
intended for crates.io — building and testing in CI without aetna present, this
crate is listed under `[workspace].exclude` in the repo root rather than
`members`. It therefore lives in the repo but is its own standalone crate with
its own `Cargo.lock`, built independently:

```sh
cd tristim-gui
cargo run -- ../capture.json
```

It expects a sibling aetna checkout at `../../aetna/aetna.main` relative to the
tristim repo root. Adjust the path dependencies in `Cargo.toml` if yours lives
elsewhere.

## Headless layout check (`dump`)

The `dump` binary builds the presenter tree for each trial in a capture and runs
aetna's `render_bundle` + lint pass — no window required. It writes SVG, a
source-mapped tree dump, the draw-op list, and a shader manifest to an output
directory, and prints lint findings (overflow, clipped text, alignment/spacing
smells, raw non-token colors, panels that should be stock widgets). It exits
non-zero if any finding fires, so it doubles as a layout gate.

```sh
cargo run --bin dump -- ../capture.json out
# writes out/trial-0.{svg,tree.txt,draw_ops.txt,shader_manifest.txt,lint.txt}
```

This is the primary way to validate layout in a headless environment; the `out/`
directory is git-ignored.

## Color preferences

The presenter declares `ColorPreferences::wide_gamut()` so it is ready to render
the chromaticity field in wide-gamut color once aetna's host gains a
wgpu swapchain-colorspace path. Today that field is **advisory** — aetna still
composites in sRGB — so the window renders sRGB-clipped for now.
