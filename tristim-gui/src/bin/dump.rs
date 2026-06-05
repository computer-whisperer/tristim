//! Headless layout check for the presenter.
//!
//! Builds the presenter tree for each main interface state and runs
//! damascene's [`render_bundle_themed`] + lint pass, writing the SVG /
//! tree-dump / draw-ops / shader-manifest artifacts and printing lint
//! findings. This is the chief way to validate the GUI's layout without
//! opening a window: damascene's lint catches overflow, clipped text,
//! alignment/spacing smells, raw (non-token) colors, and panels that should be
//! stock widgets.
//!
//! States covered, per viewport: the setup form (plain, gamut-probe expanded,
//! capability-gated, and with both error rows), the running view frozen at
//! each phase of a capture, the presenter over every trial in every view and
//! projection (legend and inspector variants), the open-error banner over the
//! presenter, and a trial-less capture.
//!
//! Usage: `cargo run --bin dump -- <capture.json> [out_dir]`
//! (out_dir defaults to `out/`). Exits non-zero if any lint finding fires.
//! CI runs this over `fixtures/reference-capture.json` (three trials —
//! unmanaged, managed sRGB, PQ/BT.2020 fp16 — all gamut-probed) and fails on
//! any finding; reproduce locally with that file as the argument.

use std::path::Path;
use std::process::ExitCode;

use damascene_core::prelude::*;
use tristim_gui::PresenterApp;
use tristim_gui::app::{DebugRunPhase, Tab};
use tristim_gui::plot::Space;
use tristim_gui::space3d::{N_REF_GAMUTS, RefCages, Space3dView};

/// Window sizes to lay out under (default + a large display), so responsive
/// plot sizing is exercised and lint-checked at both.
const VIEWPORTS: [(f32, f32); 2] = [(1280.0, 800.0), (2560.0, 1440.0)];

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: dump <capture.json> [out_dir]");
        return ExitCode::FAILURE;
    };
    let out_dir = args.next().unwrap_or_else(|| "out".to_string());

    match run(&path, &out_dir) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(n) => {
            eprintln!("dump: {n} total lint finding(s)");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("dump: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Render + lint every state at every viewport; returns the total finding count.
fn run(path: &str, out_dir: &str) -> Result<usize, String> {
    let capture =
        tristim_capture::Capture::load(path).map_err(|e| format!("failed to load {path}: {e}"))?;
    std::fs::create_dir_all(out_dir).map_err(|e| format!("cannot create {out_dir}: {e}"))?;

    let mut app = PresenterApp::new(capture.clone());
    app.set_show_field(true); // exercise the (heavier) color-field layout
    app.set_hovered_sample(Some(0)); // exercise the inspector + highlight + hit targets
    app.set_source_path(path.to_string()); // exercise the filename header subtitle
    let theme = Theme::default();

    // Feed representative host diagnostics so the dump exercises the *populated*
    // "Presenter display" panel — the path the live window takes. Without this
    // the panel collapses to a one-line placeholder and the dump never sees the
    // long adapter/format strings that drive sidebar layout. The adapter name
    // is a deliberate worst case (long, real-world string).
    let diags = HostDiagnostics {
        backend: "Vulkan",
        surface_color: Some(SurfaceColorInfo {
            adapter: "AMD Radeon RX 7900 XTX (RADV NAVI31)".to_string(),
            driver: "Mesa 24.0.0".to_string(),
            formats: vec![
                SurfaceFormatInfo {
                    name: "Bgra8UnormSrgb".to_string(),
                    srgb: true,
                    wide: false,
                },
                SurfaceFormatInfo {
                    name: "Rgba16Float".to_string(),
                    srgb: false,
                    wide: true,
                },
            ],
            chosen_format: "Bgra8UnormSrgb".to_string(),
            present_mode: "Fifo".to_string(),
            alpha_mode: "Opaque".to_string(),
        }),
        ..HostDiagnostics::default()
    };

    let count = app.trial_count().max(1);
    // The capture-setup form. It enumerates outputs (a Wayland roundtrip);
    // without a compositor the list is just empty, which still exercises the
    // form layout.
    let setup_app = PresenterApp::setup();
    // The same form with the gamut-probe controls expanded (the repeats/depth
    // steppers row only renders when probing is on).
    let mut setup_gamut_app = PresenterApp::setup();
    setup_gamut_app.set_setup_probe_gamut(true);
    // The form gated to a minimal (niri-like) compositor: no color management,
    // no fp16, so every managed format renders disabled with its reason chip —
    // the grayed-row layout we want to lint.
    let mut setup_capgated_app = PresenterApp::setup();
    setup_capgated_app.set_setup_capabilities(tristim_display::DisplayCapabilities::default());
    // The form's two error rows at once: the open-file banner above it and the
    // validation error beneath it (both deliberately long).
    let mut setup_error_app = PresenterApp::setup();
    setup_error_app.set_open_error(Some(format!(
        "failed to load {path}: invalid capture: missing field `trials` at line 1 column 2048"
    )));
    setup_error_app
        .set_setup_error("no patches selected: enable a sequence or set scatter > 0".to_string());
    // The live running view (progress strip + live plots), frozen at each
    // distinct moment of a run: countdown, gamut probe (open-ended ~total),
    // mid-sweep, cancelling, and all formats done.
    let running_apps: Vec<(PresenterApp, &str)> = [
        (DebugRunPhase::Countdown, "running-countdown"),
        (DebugRunPhase::Probing, "running-probe"),
        (DebugRunPhase::Sweeping, "running-sweep"),
        (DebugRunPhase::Cancelling, "running-cancel"),
        (DebugRunPhase::Done, "running"),
    ]
    .into_iter()
    .map(|(phase, tag)| (PresenterApp::debug_running(capture.clone(), phase), tag))
    .collect();
    // The presenter with the open-file banner over a loaded capture (a failed
    // "Open…" keeps presenting what was already in focus).
    let mut present_error_app = PresenterApp::new(capture.clone());
    present_error_app.set_source_path(path.to_string());
    present_error_app.set_open_error(Some(format!(
        "failed to load {path}: invalid capture: missing field `trials` at line 1 column 2048"
    )));
    // A capture with no trials (e.g. a run cancelled before its first format):
    // the sidebar and content panel fall back to their empty placeholders.
    let empty_app = PresenterApp::new({
        let mut c = capture.clone();
        c.trials.clear();
        c
    });

    let mut total = 0usize;
    for (vw, vh) in VIEWPORTS {
        let viewport = Rect::new(0.0, 0.0, vw, vh);
        let render = |app: &PresenterApp, name: &str| -> Result<usize, String> {
            let cx = BuildCx::new(&theme)
                .with_viewport(vw, vh)
                .with_diagnostics(&diags);
            let mut root = app.build(&cx);
            let bundle = render_bundle_themed(&mut root, viewport, &theme);
            emit(&bundle, out_dir, name).map_err(|e| e.to_string())
        };
        let w = vw as u32;

        for (probe, tag) in [
            (&setup_app, "setup"),
            (&setup_gamut_app, "setup-gamut"),
            (&setup_capgated_app, "setup-capgated"),
            (&setup_error_app, "setup-error"),
            (&present_error_app, "present-error"),
            (&empty_app, "present-empty"),
        ] {
            total += render(probe, &format!("{tag}-{w}w"))?;
        }
        for (probe, tag) in &running_apps {
            total += render(probe, &format!("{tag}-{w}w"))?;
        }

        app.set_view(Tab::Chromaticity);
        for (space, tag) in [(Space::UvPrime, "uv"), (Space::Xy, "xy")] {
            app.set_space(space);
            for i in 0..count {
                app.select(i);
                total += render(&app, &format!("chroma-{tag}-trial{i}-{w}w"))?;
            }
            // With nothing hovered the inspector yields to the projection's
            // legend; lint that variant too.
            app.select(0);
            app.set_hovered_sample(None);
            total += render(&app, &format!("chroma-{tag}-legend-{w}w"))?;
            app.set_hovered_sample(Some(0));
        }

        app.set_view(Tab::Luminance);
        for i in 0..count {
            app.select(i);
            total += render(&app, &format!("lum-trial{i}-{w}w"))?;
        }
        // The luminance legend (un-hovered variant), on the first and last
        // trials — its units line differs between SDR and HDR formats.
        app.set_hovered_sample(None);
        for i in [0, count - 1] {
            app.select(i);
            total += render(&app, &format!("lum-legend-trial{i}-{w}w"))?;
        }
        app.set_hovered_sample(Some(0));

        // The 3D sample space, in each projection. The scene itself can't
        // rasterize headlessly (the SVG/bundle path degrades 3D to a
        // placeholder), but this still exercises `Space3dScene::build` on real
        // samples and lints the tab + (busy) projection-selector + per-view
        // legend layout. `set_view` / `set_space3d_view` rebuild the cached
        // scene for the focus. (Headless there is no scene pick, so these
        // render the legend, not the inspector.)
        app.set_view(Tab::Space3D);
        for (view, tag) in [
            (Space3dView::LabRelative, "lab"),
            (Space3dView::LabAbsolute, "lababs"),
            (Space3dView::XyYNits, "xyy"),
            (Space3dView::ICtCp, "ictcp"),
        ] {
            app.set_space3d_view(view);
            for i in 0..count {
                app.select(i);
                app.set_space3d_view(view); // rebuild for the newly focused trial
                total += render(&app, &format!("space3d-{tag}-trial{i}-{w}w"))?;
            }
        }
        // Every 3D overlay at once: all reference cages (abs + rel) plus the
        // measured-gamut shell. Builds the cage / nits-label / shell geometry
        // on real samples and lints the controls row in its busiest, all-
        // toggled-on state. xyY is the projection where the absolute cages
        // separate by peak white, so their labels all draw.
        app.set_space3d_view(Space3dView::XyYNits);
        app.select(0);
        app.set_space3d_overlays(
            RefCages {
                abs: [true; N_REF_GAMUTS],
                rel: [true; N_REF_GAMUTS],
            },
            true,
        );
        total += render(&app, &format!("space3d-overlays-{w}w"))?;
        app.set_space3d_overlays(RefCages::default(), false);
    }
    Ok(total)
}

/// Write a bundle's artifacts and report its lint findings; returns the count.
fn emit(bundle: &Bundle, out_dir: &str, name: &str) -> std::io::Result<usize> {
    for p in write_bundle(bundle, Path::new(out_dir), name)? {
        println!("wrote {}", p.display());
    }
    let n = bundle.lint.findings.len();
    if n == 0 {
        println!("[{name}] lint clean ({} draw ops)", bundle.draw_ops.len());
    } else {
        eprintln!("[{name}] {n} lint finding(s):");
        eprint!("{}", bundle.lint.text());
    }
    Ok(n)
}
