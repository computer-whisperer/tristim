//! Capture orchestration for tristim.
//!
//! Drives a colorimeter and a Wayland [`PatchSurface`] through a set of color
//! formats and sequences, measuring how a compositor reproduces each patch,
//! and returns a [`Capture`]. This is the *gather* half of tristim's
//! gather/present split: it records facts only (what was advertised, what was
//! negotiated, code-value→measurement samples) — interpretation is the
//! analysis tool's job.
//!
//! [`run_capture`] is the whole loop. It is frontend-agnostic: progress is
//! reported through a [`GatherEvent`] callback and the run can be interrupted
//! through a `should_cancel` predicate, so the same code drives both the
//! `tristim` CLI (logging to stderr) and the GUI (streaming live into plots on
//! a background thread). It performs no I/O of its own beyond the device and
//! the surface — saving the returned [`Capture`] is the caller's job.

mod format;
mod gamut;
mod sequence;
mod time;

pub use format::{FormatSpec, KNOWN_FORMATS, parse_format};
pub use gamut::{
    GamutConfig, GamutEvent, GamutMesh, GamutProbe, GamutVertex, MeshVertex, Patch, PatchStatus,
    RefineParams, probe_gamut, probe_gamut_refined, refine_gamut,
};
pub use sequence::{grey_ramp, parse_sequence, primary_ramps, scatter};
pub use time::rfc3339_utc_now;

use std::thread::sleep;
use std::time::Duration;

use thiserror::Error;
use tristim_capture as cap;
use tristim_display::{self as display, DescriptionState, PatchSurface, list_outputs};
use tristim_driver::Colorimeter;
use tristim_driver::measurement::raw_to_xyz;

/// Everything a capture run needs. Built by a frontend (CLI args or the GUI
/// form) from the shared [`parse_format`] / [`parse_sequence`] helpers.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Connector name of the output under test (e.g. `"DP-4"`).
    pub output: String,
    /// On-device calibration index used for raw→XYZ.
    pub cal_index: u8,
    /// How long to wait after committing a patch before measuring.
    pub settle: Duration,
    /// Countdown given for puck placement before the first measurement.
    pub prep: Duration,
    /// Centered-window area fraction: `1.0` = fullscreen patch.
    pub window_fraction: f64,
    /// Surround code values when `window_fraction < 1.0` (`None` = black).
    pub border: Option<[f64; 3]>,
    /// Color formats to put under test, in order. Each runs the full sequence.
    pub formats: Vec<FormatSpec>,
    /// Code-value triples every format steps through (flattened sequences).
    pub sequence: Vec<[f64; 3]>,
}

/// Progress reported by [`run_capture`] as it proceeds. Owned data, so it can
/// be sent across a channel to a GUI thread.
#[derive(Debug, Clone)]
pub enum GatherEvent {
    /// The colorimeter opened and is ready; fired once at the start.
    DeviceReady {
        product: String,
        serial: String,
        hw_version: (u32, u32),
    },
    /// Puck-placement countdown, fired once per second (`remaining` counts
    /// down to 1) with a black patch already on screen.
    Countdown { remaining: u64 },
    /// Beginning a format trial (`index` of `total`). `requested` is the
    /// description we'll negotiate (`None` = unmanaged) — carried so a live
    /// consumer can score the trial's samples before the run finishes.
    FormatStart {
        index: usize,
        total: usize,
        token: String,
        pixel_format: String,
        requested: Option<cap::ColorDescription>,
    },
    /// The compositor's response to the format's description.
    Negotiation(cap::Negotiation),
    /// A patch was just measured (`index` of `total` within the format).
    Sample {
        format_index: usize,
        index: usize,
        total: usize,
        sample: cap::Sample,
    },
    /// A format trial finished with `samples` measurements recorded.
    FormatDone { index: usize, samples: usize },
}

#[derive(Debug, Error)]
pub enum GatherError {
    #[error("colorimeter: {0}")]
    Device(#[from] tristim_driver::device::Error),
    #[error("display: {0}")]
    Display(#[from] display::Error),
    #[error("compositor rejected format ({cause}): {message}")]
    FormatRejected { cause: String, message: String },
}

/// Run a full capture session, reporting progress through `on_event` and
/// stopping early (between patches) if `should_cancel` returns `true`.
///
/// Returns the [`Capture`](cap::Capture) built from whatever was measured — a
/// cancelled run yields a partial capture rather than an error. The colorimeter
/// is opened first, so a missing device fails fast before the puck countdown.
pub fn run_capture(
    config: &CaptureConfig,
    mut on_event: impl FnMut(GatherEvent),
    should_cancel: impl Fn() -> bool,
) -> Result<cap::Capture, GatherError> {
    // Colorimeter up front so we fail fast (before asking for the puck).
    let mut device = Colorimeter::open_any()?;
    let info = device.get_info()?;
    let cal = device.get_calibration(config.cal_index)?;
    let setup = device.get_setup(&cal)?;
    let product = if device.is_spyder_2024() {
        "Spyder 2024"
    } else {
        "SpyderX2"
    };
    on_event(GatherEvent::DeviceReady {
        product: product.to_string(),
        serial: info.serial.clone(),
        hw_version: info.hw_version,
    });

    let out_desc = list_outputs()?
        .into_iter()
        .find(|o| o.name == config.output);

    // Probe surface: collect capabilities, then run the one-time puck-placement
    // countdown with a black patch on screen.
    let (capabilities, compositor) = {
        let mut probe = PatchSurface::open_sdr(&config.output)?;
        probe.set_code_values([0.0, 0.0, 0.0])?;
        let caps = to_cap_capabilities(probe.color_capabilities());
        // Compositor identity: the socket peer process + the advertised globals
        // (both from the probe's Wayland connection) plus the session's
        // XDG_CURRENT_DESKTOP hint. All best-effort facts (see CompositorInfo).
        let compositor = cap::CompositorInfo {
            process: probe.compositor_process().map(str::to_string),
            desktop: std::env::var("XDG_CURRENT_DESKTOP")
                .ok()
                .filter(|s| !s.is_empty()),
            globals: probe
                .advertised_globals()
                .iter()
                .map(|(interface, version)| cap::GlobalInfo {
                    interface: interface.clone(),
                    version: *version,
                })
                .collect(),
        };
        for remaining in (1..=config.prep.as_secs()).rev() {
            on_event(GatherEvent::Countdown { remaining });
            if should_cancel() {
                break;
            }
            sleep(Duration::from_secs(1));
        }
        (caps, compositor)
    };

    let settle_ms = config.settle.as_millis() as u64;
    let format_count = config.formats.len();
    let seq_len = config.sequence.len();
    let mut trials = Vec::new();

    for (fi, fs) in config.formats.iter().enumerate() {
        if should_cancel() {
            break;
        }
        on_event(GatherEvent::FormatStart {
            index: fi,
            total: format_count,
            token: fs.token().to_string(),
            pixel_format: fs.pixel_format_str().to_string(),
            requested: fs.color_description(),
        });

        let (surface, outcome) = open_format(&config.output, fs)?;
        on_event(GatherEvent::Negotiation(outcome.clone()));

        let mut samples = Vec::new();
        if let Some(mut surface) = surface {
            surface.set_window_fraction(config.window_fraction)?;
            if let Some(b) = config.border {
                surface.set_border(b)?;
            }
            for (i, cv) in config.sequence.iter().enumerate() {
                if should_cancel() {
                    break;
                }
                surface.set_code_values(*cv)?;
                sleep(config.settle);
                let raw = device.measure_raw(&setup)?;
                let xyz = raw_to_xyz(&raw, &setup, &cal);
                let xy = xyz.chromaticity().map(|(x, y)| [x, y]);
                let sample = cap::Sample {
                    requested: *cv,
                    measured: cap::Measured {
                        raw: raw.0,
                        xyz: [xyz.x, xyz.y, xyz.z],
                        xy,
                    },
                    context: cap::SampleContext {
                        window_fraction: config.window_fraction,
                        border: config.border,
                        settle_ms,
                    },
                };
                on_event(GatherEvent::Sample {
                    format_index: fi,
                    index: i,
                    total: seq_len,
                    sample: sample.clone(),
                });
                samples.push(sample);
            }
            // Leave the panel dark before the next format.
            let _ = surface.set_code_values([0.0, 0.0, 0.0]);
        }

        let n = samples.len();
        trials.push(cap::FormatTrial {
            requested: fs.color_description(),
            pixel_format: fs.pixel_format_str().to_string(),
            outcome,
            samples,
        });
        on_event(GatherEvent::FormatDone {
            index: fi,
            samples: n,
        });
    }

    Ok(cap::Capture {
        schema_version: cap::SCHEMA_VERSION,
        timestamp: rfc3339_utc_now(),
        tool: cap::ToolInfo {
            name: "tristim".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            git_revision: None,
        },
        device: cap::DeviceInfo {
            product: product.into(),
            usb_pid: device.pid(),
            serial: info.serial,
            hw_version: info.hw_version,
            cal_index: config.cal_index,
        },
        output: cap::OutputInfo {
            name: config.output.clone(),
            make: out_desc
                .as_ref()
                .map(|o| o.make.clone())
                .unwrap_or_default(),
            model: out_desc
                .as_ref()
                .map(|o| o.model.clone())
                .unwrap_or_default(),
            description: out_desc
                .as_ref()
                .map(|o| o.description.clone())
                .unwrap_or_default(),
            mode: out_desc
                .as_ref()
                .and_then(|o| o.size)
                .map(|(w, h)| cap::OutputMode {
                    width: w,
                    height: h,
                    refresh_mhz: None,
                }),
        },
        capabilities,
        compositor,
        trials,
    })
}

/// Open a patch surface for one format, returning the surface (if patches can
/// be shown) and the negotiation outcome to record. Mirrors the gatherer's
/// negotiation policy: a compositor that exposes no color manager still gets a
/// plain unmanaged buffer of the same pixel format; an outright rejection is
/// recorded without sending anything.
pub(crate) fn open_format(
    output: &str,
    fs: &FormatSpec,
) -> Result<(Option<PatchSurface>, cap::Negotiation), GatherError> {
    match PatchSurface::open(output, fs.buffer_format(), fs.description()) {
        Ok(s) => {
            let outcome = match s.description_state() {
                None => cap::Negotiation::Unmanaged,
                Some(DescriptionState::Ready { identity }) => {
                    cap::Negotiation::Accepted { identity }
                }
                // open() only returns Ok once Ready, so these are defensive.
                Some(DescriptionState::Failed { cause, message }) => {
                    cap::Negotiation::Rejected { cause, message }
                }
                Some(DescriptionState::Pending) => cap::Negotiation::Unmanaged,
            };
            Ok((Some(s), outcome))
        }
        // The compositor has color management but refused this description.
        // Record the refusal; don't send it anyway.
        Err(display::Error::DescriptionFailed { cause, message }) => {
            Ok((None, cap::Negotiation::Rejected { cause, message }))
        }
        // No color management at all: still useful — send a plain buffer of the
        // same pixel format and measure (the analysis tool assumes sRGB for
        // unmanaged); `requested` still records what we intended.
        Err(display::Error::NoColorManager) => {
            match PatchSurface::open(output, fs.buffer_format(), None) {
                Ok(s) => Ok((Some(s), cap::Negotiation::Unmanaged)),
                Err(e) => Ok((
                    None,
                    cap::Negotiation::Rejected {
                        cause: "unmanaged_fallback_failed".into(),
                        message: e.to_string(),
                    },
                )),
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn to_cap_capabilities(c: &display::ColorCapabilities) -> cap::Capabilities {
    cap::Capabilities {
        supported_transfer_functions: c.transfer_functions.clone(),
        supported_primaries: c.primaries.clone(),
        supported_features: c.features.clone(),
        supported_render_intents: c.render_intents.clone(),
    }
}
