# tristim-gui

An [aetna](https://github.com/computer-whisperer/aetna)-based front end for
tristim. Launched with a capture JSON it opens straight into the visualization
of how faithfully a compositor reproduced color on the output under test — a CIE
chromaticity field with per-sample error vectors, a measured-vs-expected
luminance plot, and aggregate statistics. Launched with no argument it opens a
**capture-setup form** to run a new capture in-process.

The visualization is a pure view over a `tristim_analyze::AnalyzedCapture` and
holds no measurement logic of its own. The capture flow does not either: it
drives the shared `tristim-gather` crate (the same loop the `tristim` CLI uses)
on a background thread, so the GUI is a second front end over the gather libraries
rather than a fork of them.

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
cargo run -- ../capture.json   # view an existing capture
cargo run                      # capture-setup form (run a new capture)
```

It expects a sibling aetna checkout at `../../aetna/aetna.main` relative to the
tristim repo root. Adjust the path dependencies in `Cargo.toml` if yours lives
elsewhere.

## Running a capture

Launched with no argument, the GUI opens a capture-setup form: pick the output
to measure, toggle which color formats (`unmanaged`, `srgb`, `srgb-p3`,
`pq-bt2020`, `pq-p3`) and sequences (`grey`, `primaries`, `scatter`, each with a
step count) to run, and adjust settle / prep / window / calibration. The footer
previews the total measurement count and a rough duration. **Start capture**
runs `tristim_gather::run_capture` on a background thread; a live progress view
shows the device, the current format and patch, and a cancellable progress bar.

The patch is a fullscreen overlay on the **selected output** (where the puck
sits), so run this window on a *different* display to watch progress during the
run, with the chromaticity / luminance plots filling in as each patch is read.
When the run finishes (or you cancel — partial results are kept), the capture is
**auto-saved** to `capture-<timestamp>.json` in the working directory and opened
in the visualization; the file in focus is named in the header.

## Opening a capture

**Open…** (in the presenter header and on the setup screen) brings up a native
file dialog to load another capture, replacing the one in view. The dialog uses
the `xdg-desktop-portal` file chooser and runs off the main thread, so it works
on Wayland without linking GTK and without freezing the window.

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

## Views

A view selector switches the main plot between **Chromaticity** (the CIE
diagram) and **Luminance** (measured vs. expected luminance per sample, with the
ideal `y = x` diagonal). The luminance plot doubles as a tone-response curve
when fed a grey-ramp capture; per-channel peak emission needs a primary-ramp
capture (`tristim capture --seq grey:N --seq primaries:N`).

## Color field

The chromaticity diagram has an opt-in color-field backdrop (the "color fill"
toggle in the trial heading), painted by a custom WGSL shader
(`chroma_field.wgsl`) and clipped to the presenter window's negotiated gamut, so
every painted color is actually displayable. Set `TRISTIM_GUI_FIELD=1` to start
with it on. The `xy ⇄ u'v'` toggle switches the chromaticity projection.

## Color preferences

The presenter declares `ColorPreferences::wide_gamut()` so it is ready to render
the chromaticity field in wide-gamut color once aetna's host gains a
wgpu swapchain-colorspace path. Today that field is **advisory** — aetna still
composites in sRGB — so the window renders sRGB-clipped for now.
