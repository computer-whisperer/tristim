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
    RefineParams, ReproChecker, probe_gamut, probe_gamut_refined, refine_gamut,
};
pub use sequence::{
    SCATTER_SEED, grey_ramp, parse_scatter, parse_sequence, primary_ramps, scatter,
};
pub use time::rfc3339_utc_now;

use std::thread::sleep;
use std::time::Duration;

use thiserror::Error;
use tristim_capture as cap;
use tristim_display::{self as display, DescriptionState, PatchSurface, list_outputs};
use tristim_driver::{CalibrationId, MeasurementConfidence};

/// First-repeat raw sensor counts for the capture file: exactly the channels
/// the device reported, or empty when it exposes none (schema v5 records
/// absence as absence — no zero-padding).
fn raw_counts(sample: &tristim_driver::Sample) -> Vec<u16> {
    match &sample.raw {
        Some(rr) if !rr.counts.is_empty() => rr.counts[0]
            .iter()
            .map(|&c| c.min(u16::MAX as u32) as u16)
            .collect(),
        _ => Vec::new(),
    }
}

/// The capture-schema tag for the adaptive tier that produced a measurement.
/// `None` (omitted from the file) when the run didn't use adaptive
/// integration — the tier is only meaningful relative to
/// [`RunInfo::fast_integration_ms`](cap::RunInfo::fast_integration_ms).
fn tier_tag(tier: tristim_driver::AdaptiveTier, adaptive: bool) -> Option<cap::AdaptiveTier> {
    if !adaptive {
        return None;
    }
    Some(match tier {
        tristim_driver::AdaptiveTier::Fast => cap::AdaptiveTier::Fast,
        tristim_driver::AdaptiveTier::EscalatedFull => cap::AdaptiveTier::EscalatedFull,
        // `AdaptiveTier` is non-exhaustive; unknown future tiers read as a
        // plain full measurement.
        _ => cap::AdaptiveTier::SingleFull,
    })
}

/// The capture-schema record of a device's raw→XYZ conversion.
fn to_cap_calibration(c: tristim_driver::RawConversion) -> cap::CalibrationInfo {
    cap::CalibrationInfo {
        black_floor: c.black_floor,
        matrix: c.matrix,
        gain: c.gain,
        offset: c.offset,
    }
}

use crate::gamut::ProbeSample;

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
    /// Optional adaptive fast-tier integration in milliseconds, applied to
    /// every measurement of the run — gamut-probe vertices and sweep patches
    /// alike. Each point first reads at this shorter integration and only
    /// re-measures at the calibration default if the fast result isn't
    /// trustworthy (see [`GamutConfig::fast_integration_ms`]). `None` measures
    /// everything at the default integration.
    pub fast_integration_ms: Option<u16>,
    /// Countdown given for puck placement before the first measurement.
    pub prep: Duration,
    /// Centered-window area fraction: `1.0` = fullscreen patch.
    pub window_fraction: f64,
    /// Surround code values when `window_fraction < 1.0` (`None` = black).
    pub border: Option<[f64; 3]>,
    /// Color formats to put under test, in order. Each runs the full sequence.
    pub formats: Vec<FormatSpec>,
    /// Deterministic code-value triples (grey/primaries) every format steps
    /// through. Scatter is generated separately (see `scatter`).
    pub sequence: Vec<[f64; 3]>,
    /// If set, scatter samples generated per format and appended to the sweep.
    /// When a gamut is probed, they're constrained to the measured gamut
    /// (generate-to-fill); otherwise it's plain deterministic scatter.
    pub scatter: Option<ScatterRequest>,
    /// If set, probe each format's reproduced gamut (on the same surface,
    /// before its sweep) and record it on the trial.
    pub gamut: Option<GamutProbeOpts>,
}

/// A per-format scatter request: how many points, and the seed to draw them
/// from. Generated at capture time so it can be constrained to that format's
/// measured gamut.
#[derive(Debug, Clone)]
pub struct ScatterRequest {
    pub count: usize,
    pub seed: u64,
}

/// Options for the optional per-format gamut-probe prerequisite of a capture.
/// The output, format, settle, and window come from the [`CaptureConfig`]; this
/// adds only the measurement depth.
#[derive(Debug, Clone)]
pub struct GamutProbeOpts {
    /// Repeated measurements per probe point (burst within a point).
    pub repeats: usize,
    /// Adaptive-refinement thresholds.
    pub refine: RefineParams,
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
    /// A gamut-probe vertex was just measured, during the probe and before the
    /// sweep. Streamed so a live consumer can plot the boundary points as they
    /// come in. It carries no `index`/`total` (the adaptive probe's count isn't
    /// known in advance) and is *not* part of the sweep progress.
    ProbeSample {
        format_index: usize,
        sample: cap::Sample,
    },
    /// A format's gamut probe finished, before its sweep. `folds` is the number
    /// of clamped (`Folded`) leaf patches detected.
    GamutProbed {
        index: usize,
        vertices: usize,
        folds: usize,
    },
    /// A format trial finished with `samples` measurements recorded.
    FormatDone { index: usize, samples: usize },
}

#[derive(Debug, Error)]
pub enum GatherError {
    #[error("colorimeter: {0}")]
    Device(#[from] tristim_driver::Error),
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
    let mut device = tristim_driver::open_any()?;
    device.select_calibration(CalibrationId(config.cal_index))?;
    let info = device.info().clone();
    // The conversion behind the selected calibration, recorded so stored raw
    // counts can be re-converted and audited offline.
    let calibration = device.raw_conversion().map(to_cap_calibration);
    on_event(GatherEvent::DeviceReady {
        product: info.model.clone(),
        serial: info.serial.clone(),
        hw_version: info.firmware,
    });

    let out_desc = list_outputs()?
        .into_iter()
        .find(|o| o.name == config.output);

    // Probe surface: collect capabilities, then run the one-time puck-placement
    // countdown with a black patch on screen. `display_caps` is the planning
    // snapshot each format's pipeline is pre-flighted against.
    let (capabilities, display_caps, compositor) = {
        let mut probe = PatchSurface::open_sdr(&config.output)?;
        probe.set_code_values([0.0, 0.0, 0.0])?;
        let caps = to_cap_capabilities(probe.color_capabilities());
        let display_caps = probe.display_capabilities();
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
        (caps, display_caps, compositor)
    };

    let settle_ms = config.settle.as_millis() as u64;
    let format_count = config.formats.len();
    let mut trials = Vec::new();
    // Sample timestamps (`elapsed_ms`) count from here — the start of the
    // measurement phase, prep countdown excluded.
    let run_start = std::time::Instant::now();

    for (fi, fs) in config.formats.iter().enumerate() {
        if should_cancel() {
            break;
        }
        // Pre-flight: can tristim-display arrange this representation here,
        // and through which buffer? Refusals are recorded as facts on the
        // trial, never aborting the run.
        let plan = display_caps.plan(fs.mode());
        let mut pixel_format = match &plan {
            Ok(p) => p.buffer.name().to_string(),
            Err(_) => fs.buffer_label().to_string(),
        };
        on_event(GatherEvent::FormatStart {
            index: fi,
            total: format_count,
            token: fs.token().to_string(),
            pixel_format: pixel_format.clone(),
            requested: fs.color_description(),
        });

        let (surface, outcome) = match plan {
            // Pin the planned buffer so the recorded pixel_format above is
            // exactly what the surface uses.
            Ok(p) => open_format(
                &config.output,
                &display::RenderMode {
                    description: fs.description(),
                    buffer: display::BufferPolicy::Exact(p.buffer),
                },
            )?,
            // Gatherer policy: a compositor with no color management at all
            // still gets measured — show an unmanaged buffer under the same
            // buffer policy; `requested` records what we intended and the
            // analysis tool assumes sRGB for unmanaged.
            Err(display::Unarrangeable::NoColorManagement) => {
                let fallback = display::RenderMode {
                    description: None,
                    buffer: fs.mode().buffer,
                };
                match display_caps
                    .plan(&fallback)
                    .map(|p| PatchSurface::open(&config.output, p.buffer, None))
                {
                    Ok(Ok(s)) => {
                        pixel_format = s.buffer_format().name().to_string();
                        (Some(s), cap::Negotiation::Unmanaged)
                    }
                    Ok(Err(e)) => (
                        None,
                        cap::Negotiation::Rejected {
                            cause: "unmanaged_fallback_failed".into(),
                            message: e.to_string(),
                        },
                    ),
                    Err(u) => (
                        None,
                        cap::Negotiation::Rejected {
                            cause: "not_arrangeable".into(),
                            message: u.to_string(),
                        },
                    ),
                }
            }
            // Anything else (description needs an unadvertised value or
            // feature, no adequate buffer): record the refusal and move on.
            Err(u) => (
                None,
                cap::Negotiation::Rejected {
                    cause: "not_arrangeable".into(),
                    message: u.to_string(),
                },
            ),
        };
        on_event(GatherEvent::Negotiation(outcome.clone()));

        let mut samples = Vec::new();
        let mut gamut = None;
        let mut mesh_opt = None;
        if let Some(mut surface) = surface {
            surface.set_window_fraction(config.window_fraction)?;
            if let Some(b) = config.border {
                surface.set_border(b)?;
            }
            // Optional prerequisite: probe this encoding's reproduced gamut on
            // the same surface (one puck placement) before the sweep, and record
            // it on the trial. The measure closure drives the surface + device;
            // its borrows release when `refine_gamut` returns, before the sweep.
            if let Some(opts) = &config.gamut {
                if !should_cancel() {
                    // Each unique probed vertex is also a real measurement, so
                    // fold it into the trial's samples rather than discarding it
                    // after the gamut shell is built. The refine cache calls
                    // `measure` exactly once per distinct code value, so this
                    // collects one (repeat-averaged) sample per vertex.
                    let measure = |cv: [f64; 3]| -> Result<ProbeSample, GatherError> {
                        surface.set_code_values(cv)?;
                        sleep(config.settle);
                        let result =
                            device.measure_adaptive(opts.repeats, config.fast_integration_ms)?;
                        let conf = MeasurementConfidence::from_sample(&result.sample);
                        let xyz = conf.mean;
                        let sample = cap::Sample {
                            requested: cv,
                            measured: cap::Measured {
                                // Repeat-averaged per-channel means when the
                                // device reports raw counts; empty for
                                // XYZ-only devices (e.g. the i1d3).
                                raw: conf.raw.as_ref().map_or_else(Vec::new, |rs| {
                                    rs.raw_mean
                                        .iter()
                                        .map(|m| m.round().clamp(0.0, u16::MAX as f64) as u16)
                                        .collect()
                                }),
                                xyz: [xyz.x, xyz.y, xyz.z],
                                xy: xyz.chromaticity().map(|(x, y)| [x, y]),
                            },
                            context: cap::SampleContext {
                                window_fraction: config.window_fraction,
                                border: config.border,
                                settle_ms,
                            },
                            source: cap::SampleSource::GamutProbe,
                            repeats: conf.n as u32,
                            adaptive_tier: tier_tag(
                                result.tier,
                                config.fast_integration_ms.is_some(),
                            ),
                            elapsed_ms: Some(run_start.elapsed().as_millis() as u64),
                        };
                        // Stream it for the live view, then keep it for the file.
                        on_event(GatherEvent::ProbeSample {
                            format_index: fi,
                            sample: sample.clone(),
                        });
                        samples.push(sample);
                        Ok(ProbeSample {
                            measured: xyz,
                            trustworthy: conf.is_trustworthy(),
                        })
                    };
                    let mesh = refine_gamut(&opts.refine, measure, &should_cancel)?;
                    // A cancelled probe returns an incomplete mesh: its samples
                    // are real measurements and stay on the trial, but a holey
                    // shell would mislead downstream (3D shell view, scatter
                    // constraint), so only a completed probe is recorded.
                    if !should_cancel() {
                        on_event(GatherEvent::GamutProbed {
                            index: fi,
                            vertices: mesh.vertices.len(),
                            folds: mesh.count(crate::gamut::PatchStatus::Folded),
                        });
                        gamut = Some(mesh.to_capture());
                        mesh_opt = Some(mesh);
                    }
                }
            }

            // This format's sweep: the deterministic sequence + scatter. When a
            // gamut was probed, scatter is constrained to it (generate-to-fill);
            // otherwise it's plain deterministic scatter.
            let mut sweep = config.sequence.clone();
            if let Some(req) = &config.scatter {
                let checker = mesh_opt
                    .as_ref()
                    .and_then(|m| ReproChecker::new(m, fs.color_description().as_ref()));
                let pts = match &checker {
                    Some(c) => {
                        sequence::scatter_accepted(req.count, req.seed, |cv| c.reproducible(cv))
                    }
                    None => sequence::scatter_accepted(req.count, req.seed, |_| true),
                };
                sweep.extend(pts);
            }
            let total = sweep.len();

            for (i, cv) in sweep.iter().enumerate() {
                if should_cancel() {
                    break;
                }
                surface.set_code_values(*cv)?;
                sleep(config.settle);
                // Adaptive like the probe: a single repeat still carries the
                // quantization/floor trust analysis (it needs counts, not
                // repeat scatter), so a dim patch read at the fast tier
                // escalates to the calibration default instead of being kept.
                let result = device.measure_adaptive(1, config.fast_integration_ms)?;
                let measured = result.sample;
                let xyz = measured.xyz[0];
                let raw = raw_counts(&measured);
                let xy = xyz.chromaticity().map(|(x, y)| [x, y]);
                let sample = cap::Sample {
                    requested: *cv,
                    measured: cap::Measured {
                        raw,
                        xyz: [xyz.x, xyz.y, xyz.z],
                        xy,
                    },
                    context: cap::SampleContext {
                        window_fraction: config.window_fraction,
                        border: config.border,
                        settle_ms,
                    },
                    source: cap::SampleSource::Sweep,
                    repeats: 1,
                    adaptive_tier: tier_tag(result.tier, config.fast_integration_ms.is_some()),
                    elapsed_ms: Some(run_start.elapsed().as_millis() as u64),
                };
                on_event(GatherEvent::Sample {
                    format_index: fi,
                    index: i,
                    total,
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
            pixel_format,
            outcome,
            gamut,
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
            product: info.model,
            usb_pid: info.usb_pid,
            serial: info.serial,
            hw_version: info.firmware,
            cal_index: config.cal_index,
            calibration,
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
        run: Some(cap::RunInfo {
            prep_ms: config.prep.as_millis() as u64,
            fast_integration_ms: config.fast_integration_ms,
            scatter: config.scatter.as_ref().map(|s| cap::ScatterInfo {
                count: s.count as u32,
                seed: s.seed,
            }),
            gamut_probe: config.gamut.as_ref().map(|g| cap::GamutProbeInfo {
                repeats: g.repeats as u32,
                max_depth: g.refine.max_depth,
                flat_eps: g.refine.flat_eps,
                fold_eps: g.refine.fold_eps,
                fold_min_side: g.refine.fold_min_side,
            }),
        }),
        trials,
    })
}

/// Arrange one mode on the output, returning the surface (if patches can be
/// shown) and the negotiation outcome to record. Refusals — client-side
/// (unarrangeable here) or wire-level (compositor rejected the description)
/// — come back as [`cap::Negotiation::Rejected`] facts rather than aborting
/// the run; only transport-level failures error out.
pub(crate) fn open_format(
    output: &str,
    mode: &display::RenderMode,
) -> Result<(Option<PatchSurface>, cap::Negotiation), GatherError> {
    match PatchSurface::open_mode(output, mode) {
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
        // The compositor has color management but refused this description
        // on the wire. Record the refusal.
        Err(display::Error::DescriptionFailed { cause, message }) => {
            Ok((None, cap::Negotiation::Rejected { cause, message }))
        }
        // The mode can't be arranged here (description needs an unadvertised
        // value or feature, no adequate buffer, or no color management at
        // all). A fact worth recording, not a run-stopper.
        Err(display::Error::Unarrangeable(u)) => Ok((
            None,
            cap::Negotiation::Rejected {
                cause: "not_arrangeable".into(),
                message: u.to_string(),
            },
        )),
        Err(display::Error::BadDescription(e)) => Ok((
            None,
            cap::Negotiation::Rejected {
                cause: "not_arrangeable".into(),
                message: e.to_string(),
            },
        )),
        Err(display::Error::NoColorManager) => Ok((
            None,
            cap::Negotiation::Rejected {
                cause: "no_color_management".into(),
                message: "compositor doesn't advertise wp_color_manager_v1".into(),
            },
        )),
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
