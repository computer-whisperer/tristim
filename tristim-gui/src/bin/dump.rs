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

const VIEWPORT: (f32, f32) = (1280.0, 800.0);

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
    let theme = Theme::default();
    let viewport = Rect::new(0.0, 0.0, VIEWPORT.0, VIEWPORT.1);

    // Lay out and lint every trial's panel, not just the default selection.
    let count = app.trial_count().max(1);
    let mut total_findings = 0usize;
    for i in 0..count {
        app.select(i);
        let cx = BuildCx::new(&theme);
        let mut root = app.build(&cx);
        let bundle = render_bundle_themed(&mut root, viewport, &theme);

        let name = format!("trial-{i}");
        match write_bundle(&bundle, Path::new(&out_dir), &name) {
            Ok(written) => {
                for p in &written {
                    println!("wrote {}", p.display());
                }
            }
            Err(e) => {
                eprintln!("dump: write_bundle({name}): {e}");
                return ExitCode::FAILURE;
            }
        }

        if bundle.lint.findings.is_empty() {
            println!("[{name}] lint clean ({} draw ops)", bundle.draw_ops.len());
        } else {
            total_findings += bundle.lint.findings.len();
            eprintln!("[{name}] {} lint finding(s):", bundle.lint.findings.len());
            eprint!("{}", bundle.lint.text());
        }
    }

    if total_findings == 0 {
        ExitCode::SUCCESS
    } else {
        eprintln!("dump: {total_findings} total lint finding(s)");
        ExitCode::FAILURE
    }
}
