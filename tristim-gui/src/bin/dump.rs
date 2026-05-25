//! Headless layout check for the presenter.
//!
//! Builds the presenter tree for each trial in a capture and runs aetna's
//! [`render_bundle_themed`] + lint pass, writing the SVG / tree-dump /
//! draw-ops / shader-manifest artifacts and printing lint findings. This is
//! the chief way to validate the GUI's layout without opening a window:
//! aetna's lint catches overflow, clipped text, alignment/spacing smells,
//! raw (non-token) colors, and panels that should be stock widgets.
//!
//! Usage: `cargo run --bin dump -- <capture.json> [out_dir]`
//! (out_dir defaults to `out/`). Exits non-zero if any lint finding fires.

use std::path::Path;
use std::process::ExitCode;

use aetna_core::prelude::*;
use tristim_gui::PresenterApp;
use tristim_gui::app::Tab;
use tristim_gui::plot::Space;

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

    let capture = match tristim_capture::Capture::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dump: failed to load {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("dump: cannot create {out_dir}: {e}");
        return ExitCode::FAILURE;
    }

    let mut app = PresenterApp::new(capture);
    app.set_show_field(true); // exercise the (heavier) color-field layout
    app.set_hovered_sample(Some(0)); // exercise the inspector + highlight + hit targets
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

    // Lay out and lint every trial's panel for each view, at each window size
    // (responsive plot sizing reads the viewport). Chromaticity is rendered in
    // both projections; luminance is projection-independent.
    let count = app.trial_count().max(1);
    // Also lint the capture-setup form. It enumerates outputs (a Wayland
    // roundtrip); without a compositor the list is just empty, which still
    // exercises the form layout.
    let setup_app = PresenterApp::setup();
    let mut total_findings = 0usize;
    for (vw, vh) in VIEWPORTS {
        let viewport = Rect::new(0.0, 0.0, vw, vh);
        let render = |app: &PresenterApp, name: &str| {
            let cx = BuildCx::new(&theme)
                .with_viewport(vw, vh)
                .with_diagnostics(&diags);
            let mut root = app.build(&cx);
            let bundle = render_bundle_themed(&mut root, viewport, &theme);
            emit(&bundle, &out_dir, name)
        };

        match render(&setup_app, &format!("setup-{}w", vw as u32)) {
            Ok(n) => total_findings += n,
            Err(e) => {
                eprintln!("dump: {e}");
                return ExitCode::FAILURE;
            }
        }

        app.set_view(Tab::Chromaticity);
        for (space, tag) in [(Space::UvPrime, "uv"), (Space::Xy, "xy")] {
            app.set_space(space);
            for i in 0..count {
                app.select(i);
                match render(&app, &format!("chroma-{tag}-trial{i}-{}w", vw as u32)) {
                    Ok(n) => total_findings += n,
                    Err(e) => {
                        eprintln!("dump: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
        }

        app.set_view(Tab::Luminance);
        for i in 0..count {
            app.select(i);
            match render(&app, &format!("lum-trial{i}-{}w", vw as u32)) {
                Ok(n) => total_findings += n,
                Err(e) => {
                    eprintln!("dump: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    if total_findings == 0 {
        ExitCode::SUCCESS
    } else {
        eprintln!("dump: {total_findings} total lint finding(s)");
        ExitCode::FAILURE
    }
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
