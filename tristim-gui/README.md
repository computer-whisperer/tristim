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

## Color preferences

The presenter declares `ColorPreferences::wide_gamut()` so it is ready to render
the chromaticity field in wide-gamut color once aetna's host gains a
wgpu swapchain-colorspace path. Today that field is **advisory** — aetna still
composites in sRGB — so the window renders sRGB-clipped for now.
