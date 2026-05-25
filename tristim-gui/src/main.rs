//! tristim-gui — an Aetna presenter for tristim captures.
//!
//! Loads a capture JSON, runs [`tristim_analyze::analyze`], and opens a window
//! that visualizes where the compositor reproduced color faithfully versus
//! where it drifted. This is the *presentation* half of tristim's
//! gather/present split: it carries no measurement logic of its own, only a
//! view over an [`tristim_analyze::AnalyzedCapture`].

use std::process::ExitCode;
use std::time::Duration;

use aetna_core::color::ColorPreferences;
use aetna_core::prelude::Rect;
use aetna_winit_wgpu::{HostConfig, run_with_config};

use tristim_gui::PresenterApp;

const USAGE: &str = "\
usage: tristim-gui [capture.json]

With a capture file, opens straight into the visualization: for each format
trial, how the compositor's color reproduction compares to the negotiated (or
assumed-sRGB) target — per-sample chromaticity / luminance error and aggregate
statistics. With no argument, opens the capture-setup form to run a new capture
in-process (drives the colorimeter + a Wayland patch surface on the chosen
output).";

fn main() -> ExitCode {
    let mut app = match std::env::args().nth(1) {
        Some(p) if p == "-h" || p == "--help" => {
            eprintln!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(path) => match tristim_capture::Capture::load(&path) {
            Ok(capture) => PresenterApp::new(capture),
            Err(e) => {
                eprintln!("tristim-gui: failed to load {path}: {e}");
                return ExitCode::FAILURE;
            }
        },
        // No file: open the capture-setup form.
        None => PresenterApp::setup(),
    };

    // Convenience: start with the color-field backdrop on (otherwise it's an
    // in-app toggle, off by default).
    if std::env::var_os("TRISTIM_GUI_FIELD").is_some() {
        app.set_show_field(true);
    }

    // Declare wide-gamut intent. aetna's host treats `color_preferences` as
    // advisory today — it still composites in sRGB pending a wgpu
    // swapchain-colorspace knob — so this is forward-looking: when that path
    // lands, the chromaticity field can render true wide-gamut color with no
    // change here.
    //
    // A fixed redraw cadence lets a running capture's progress (arriving on a
    // background thread, drained in `before_build`) animate without waiting on
    // input events.
    let config = HostConfig::default()
        .with_app_id("dev.tristim.gui")
        .with_color_preferences(ColorPreferences::wide_gamut())
        .with_redraw_interval(Duration::from_millis(120));

    let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);
    if let Err(e) = run_with_config("tristim — color validation", viewport, app, config) {
        eprintln!("tristim-gui: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
