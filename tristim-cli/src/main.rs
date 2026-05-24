//! `tristim` — the data gatherer.
//!
//! Drives a Datacolor Spyder colorimeter + a Wayland patch surface to
//! capture how a compositor reproduces color, and writes the result to a
//! `tristim-capture` JSON file. It records *facts only* — what the
//! compositor advertised, what color description we negotiated (and
//! whether it accepted), and the code-value→measurement samples. All
//! interpretation (computing the expected output, scoring error) is the
//! analysis tool's job.
//!
//! Subcommands:
//!   tristim list-outputs                 enumerate connected outputs
//!   tristim info                         open the colorimeter, print HW info
//!   tristim measure [--cal N]            take one XYZ measurement (aim manually)
//!   tristim capture --output NAME ...    run a capture session, write JSON

use std::collections::HashMap;
use std::error::Error;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tristim_analyze::{GroundTruth, GroundTruthSource, analyze};
use tristim_capture as cap;
use tristim_display::{
    self as display, BufferFormat, DescriptionRequest, DescriptionState, PatchSurface, list_outputs,
};
use tristim_driver::Colorimeter;
use tristim_driver::measurement::raw_to_xyz;

fn main() -> Result<(), Box<dyn Error>> {
    let argv: Vec<String> = std::env::args().collect();
    let cmd = argv.get(1).map(String::as_str).unwrap_or("help");

    match cmd {
        "list-outputs" | "outputs" | "ls" => cmd_list_outputs(),
        "info" => cmd_info(),
        "measure" => cmd_measure(&argv[2..]),
        "capture" => cmd_capture(&argv[2..]),
        "report" => cmd_report(&argv[2..]),
        "help" | "-h" | "--help" => {
            print_usage();
            Ok(())
        }
        other => {
            eprintln!("unknown subcommand: {other:?}");
            print_usage();
            std::process::exit(2);
        }
    }
}

fn print_usage() {
    eprintln!("USAGE: tristim <subcommand> [opts]");
    eprintln!();
    eprintln!("  list-outputs              enumerate Wayland outputs");
    eprintln!("  info                      open the colorimeter, print HW info");
    eprintln!("  measure [--cal N]         take one XYZ measurement (aim puck manually)");
    eprintln!("  capture --output NAME [opts]");
    eprintln!("        Run a capture session and write a tristim-capture JSON file.");
    eprintln!("        Options:");
    eprintln!("          --out FILE         output path (default: capture.json)");
    eprintln!("          --cal N            calibration index (default: 0)");
    eprintln!("          --settle-ms N      ms to wait after each patch (default: 250)");
    eprintln!("          --prep-secs N      seconds to wait for puck placement (default: 6)");
    eprintln!("          --window F         centered-window area fraction (default: 1.0)");
    eprintln!("          --border R,G,B     surround code values for windowed/anti-CABL use");
    eprintln!("          --format SPEC      color format to negotiate (repeatable, >=1 required)");
    eprintln!("          --seq SPEC         color sequence to run (repeatable, >=1 required)");
    eprintln!();
    eprintln!("        --format SPEC (name[:k=v,...]):");
    eprintln!("          unmanaged                      plain 8-bit buffer, no description");
    eprintln!("          srgb                           8-bit, sRGB TF + sRGB primaries");
    eprintln!("          srgb-p3                        8-bit, sRGB TF + Display-P3 primaries");
    eprintln!("          pq-bt2020[:peak=,maxcll=,maxfall=,min=]   fp16, PQ + BT.2020");
    eprintln!("          pq-p3[:...]                    fp16, PQ + Display-P3");
    eprintln!("        mastering params are in cd/m² (peak default 400).");
    eprintln!();
    eprintln!("        --seq SPEC:");
    eprintln!("          grey:N            N-step grey ramp 0..1 (default N=11)");
    eprintln!("          primaries:N       per-channel R/G/B ramps, N steps (default 5)");
    eprintln!(
        "          scatter:N         N uniform code-value points (deterministic; default 32)"
    );
    eprintln!();
    eprintln!("        Each --format runs every --seq (the same code values under each");
    eprintln!("        encoding). Example:");
    eprintln!("          tristim capture --output DP-4 --out s.json \\");
    eprintln!("              --format unmanaged --format srgb --format pq-bt2020:peak=400 \\");
    eprintln!("              --seq grey:11 --seq primaries:5 --seq scatter:64");
    eprintln!();
    eprintln!("  report FILE.json [--top N]");
    eprintln!("        Analyze a capture and print per-trial error (Δu'v', ΔE) and the");
    eprintln!("        worst-offending samples (default N=8).");
}

// ── diagnostics ─────────────────────────────────────────────────────────────

fn cmd_list_outputs() -> Result<(), Box<dyn Error>> {
    for o in list_outputs()? {
        println!(
            "{:14} {:25} {:>10}   {}",
            o.name,
            o.model,
            o.size
                .map(|(w, h)| format!("{w}x{h}"))
                .unwrap_or_else(|| "?".into()),
            o.description
        );
    }
    Ok(())
}

fn cmd_info() -> Result<(), Box<dyn Error>> {
    let mut device = Colorimeter::open_any()?;
    let info = device.get_info()?;
    println!(
        "{} (PID 0x{:04x}) — HW {}.{:02} — SN {}",
        if device.is_spyder_2024() {
            "Spyder 2024"
        } else {
            "SpyderX2"
        },
        device.pid(),
        info.hw_version.0,
        info.hw_version.1,
        info.serial,
    );
    println!("high-level cmds: {}", info.high_level_commands);
    Ok(())
}

fn cmd_measure(args: &[String]) -> Result<(), Box<dyn Error>> {
    let cal: u8 = parse_opt(args, "--cal", 0);
    let mut device = Colorimeter::open_any()?;
    let info = device.get_info()?;
    println!(
        "{} SN {} — HW {}.{:02}",
        if device.is_spyder_2024() {
            "Spyder 2024"
        } else {
            "SpyderX2"
        },
        info.serial,
        info.hw_version.0,
        info.hw_version.1
    );
    println!("Measuring (cal {cal})... aim the puck.");
    let (xyz, raw, _, _) = device.measure_xyz(cal)?;
    println!("Raw  : {:?}", raw.0);
    println!("X={:.4}  Y={:.4} cd/m²  Z={:.4}", xyz.x, xyz.y, xyz.z);
    if let Some((x, y)) = xyz.chromaticity() {
        println!("xy   : ({x:.4}, {y:.4})");
    }
    Ok(())
}

// ── report ──────────────────────────────────────────────────────────────────

fn cmd_report(args: &[String]) -> Result<(), Box<dyn Error>> {
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("usage: tristim report FILE.json [--top N]")?;
    let top: usize = parse_opt(args, "--top", 8);

    let capture = cap::Capture::load(path)?;
    let analyzed = analyze(&capture);

    println!("{path}");
    println!(
        "  {} {} | {} SN {} cal {} | {} {} | {}",
        capture.tool.name,
        capture.tool.version,
        capture.device.product,
        capture.device.serial,
        capture.device.cal_index,
        capture.output.name,
        capture
            .output
            .mode
            .map(|m| format!("{}x{}", m.width, m.height))
            .unwrap_or_else(|| "?".into()),
        capture.timestamp,
    );
    let caps = &capture.capabilities;
    if caps.supported_transfer_functions.is_empty() && caps.supported_primaries.is_empty() {
        println!("  color management: none advertised");
    } else {
        println!(
            "  color management: {} TFs, {} primaries, intents {:?}",
            caps.supported_transfer_functions.len(),
            caps.supported_primaries.len(),
            caps.supported_render_intents,
        );
    }

    for (ct, at) in capture.trials.iter().zip(&analyzed.trials) {
        let label = ct
            .requested
            .as_ref()
            .map(|d| format!("{}/{}", d.transfer_function, d.primaries))
            .unwrap_or_else(|| "unmanaged".into());
        let basis = match &at.ground_truth {
            GroundTruth::Known {
                source: GroundTruthSource::Negotiated,
                ..
            } => "negotiated".to_string(),
            GroundTruth::Known {
                source: GroundTruthSource::AssumedSrgb,
                ..
            } => "assumed sRGB".to_string(),
            GroundTruth::Unscored { reason } => format!("unscored — {reason}"),
        };
        println!();
        println!("  ── {label} [{}] — {basis} ──", at.pixel_format);

        let Some(summary) = at.summary else {
            println!("     {} samples, not scored", at.samples.len());
            continue;
        };
        println!(
            "     {} samples | measured white Y = {:.2} cd/m²",
            summary.scored_samples, summary.measured_white_y
        );
        println!(
            "     Δu'v'   mean {:.4}  max {:.4}   {}",
            summary.mean_delta_uv,
            summary.max_delta_uv,
            duv_verdict(summary.max_delta_uv),
        );
        println!(
            "     ΔE*ab   mean {:.2}    max {:.2}",
            summary.mean_delta_e, summary.max_delta_e,
        );

        // Worst offenders by ΔE.
        let mut scored: Vec<&tristim_analyze::AnalyzedSample> =
            at.samples.iter().filter(|s| s.delta_e.is_some()).collect();
        scored.sort_by(|a, b| b.delta_e.unwrap().total_cmp(&a.delta_e.unwrap()));
        if !scored.is_empty() {
            println!("     worst {} by ΔE:", top.min(scored.len()));
            println!(
                "        {:>21}  {:>15}  {:>15}  {:>7}  {:>6}",
                "requested cv", "measured xy", "expected xy", "Δu'v'", "ΔE"
            );
            for s in scored.iter().take(top) {
                println!(
                    "        ({:.3},{:.3},{:.3})  {}  {}  {}  {:>6.2}",
                    s.requested[0],
                    s.requested[1],
                    s.requested[2],
                    fmt_xy(s.measured_xy),
                    fmt_xy(s.expected_xy),
                    fmt_opt(s.delta_uv, 4),
                    s.delta_e.unwrap(),
                );
            }
        }
    }
    Ok(())
}

fn duv_verdict(max: f64) -> &'static str {
    if max < 0.005 {
        "(imperceptible)"
    } else if max < 0.015 {
        "(perceptible)"
    } else if max < 0.030 {
        "(obvious)"
    } else {
        "(severe)"
    }
}

fn fmt_xy(xy: Option<[f64; 2]>) -> String {
    match xy {
        Some([x, y]) => format!("({x:.3},{y:.3})"),
        None => "       —       ".to_string(),
    }
}

fn fmt_opt(v: Option<f64>, prec: usize) -> String {
    match v {
        Some(v) => format!("{v:>7.prec$}"),
        None => format!("{:>7}", "—"),
    }
}

// ── capture ───────────────────────────────────────────────────────────────

fn cmd_capture(args: &[String]) -> Result<(), Box<dyn Error>> {
    let output = arg_value(args, "--output")
        .ok_or("--output NAME is required (try `tristim list-outputs`)")?;
    let out_path = arg_value(args, "--out").unwrap_or_else(|| "capture.json".into());
    let cal_index: u8 = parse_opt(args, "--cal", 0);
    let settle_ms: u64 = parse_opt(args, "--settle-ms", 250);
    let prep_secs: u64 = parse_opt(args, "--prep-secs", 6);
    let window_fraction: f64 = parse_opt(args, "--window", 1.0);
    let border: Option<[f64; 3]> = match arg_value(args, "--border") {
        Some(s) => Some(parse_rgb(&s)?),
        None => None,
    };

    let format_specs = collect_values(args, "--format")
        .iter()
        .map(|s| parse_format(s))
        .collect::<Result<Vec<_>, _>>()?;
    if format_specs.is_empty() {
        return Err("at least one --format is required (see `tristim help`)".into());
    }

    let seq_specs = collect_values(args, "--seq");
    if seq_specs.is_empty() {
        return Err("at least one --seq is required (see `tristim help`)".into());
    }
    let mut sequence = Vec::new();
    for s in &seq_specs {
        sequence.extend(parse_sequence(s)?);
    }

    // Colorimeter up front so we fail fast.
    let mut device = Colorimeter::open_any()?;
    let info = device.get_info()?;
    let cal = device.get_calibration(cal_index)?;
    let setup = device.get_setup(&cal)?;
    eprintln!(
        "{} SN {} HW {}.{:02}, cal[{cal_index}]",
        if device.is_spyder_2024() {
            "Spyder 2024"
        } else {
            "SpyderX2"
        },
        info.serial,
        info.hw_version.0,
        info.hw_version.1,
    );

    let out_desc = list_outputs()?.into_iter().find(|o| o.name == output);
    eprintln!(
        "capture: output={output}, {} formats, {} samples/format, window={window_fraction}, -> {out_path}",
        format_specs.len(),
        sequence.len(),
    );

    // Probe surface: collect capabilities + run the one-time puck-placement
    // countdown with a black patch on screen.
    let capabilities = {
        let mut probe = PatchSurface::open_sdr(&output)?;
        probe.set_code_values([0.0, 0.0, 0.0])?;
        let caps = to_cap_capabilities(probe.color_capabilities());
        eprintln!("Place the puck flat against '{output}'. Capture starts in {prep_secs}s.");
        for s in (1..=prep_secs).rev() {
            eprintln!("  starting in {s}s...");
            sleep(Duration::from_secs(1));
        }
        caps
    };

    let settle = Duration::from_millis(settle_ms);
    let mut trials = Vec::new();
    for fs in &format_specs {
        eprintln!("── format {} ({}) ──", fs.token, fs.pixel_format_str());
        let (surface, outcome) =
            match PatchSurface::open(&output, fs.buffer_format, fs.description.clone()) {
                Ok(s) => {
                    let outcome = match s.description_state() {
                        None => cap::Negotiation::Unmanaged,
                        Some(DescriptionState::Ready { identity }) => {
                            cap::Negotiation::Accepted { identity }
                        }
                        // open() only returns Ok once Ready, so these are
                        // defensive — record them as best we can.
                        Some(DescriptionState::Failed { cause, message }) => {
                            cap::Negotiation::Rejected { cause, message }
                        }
                        Some(DescriptionState::Pending) => cap::Negotiation::Unmanaged,
                    };
                    (Some(s), outcome)
                }
                Err(display::Error::DescriptionFailed { cause, message }) => {
                    // The compositor has color management but refused this
                    // description. Record the refusal; don't send it anyway.
                    eprintln!("  compositor rejected this format: {cause}: {message}");
                    (None, cap::Negotiation::Rejected { cause, message })
                }
                Err(display::Error::NoColorManager) => {
                    // The compositor exposes no color management at all. Still
                    // useful: send a plain buffer of the same pixel format and
                    // measure. The outcome is Unmanaged (the analysis tool
                    // assumes sRGB for unmanaged); `requested` still records
                    // what we intended.
                    eprintln!("  no color manager — sending unmanaged buffer (assumed sRGB)");
                    match PatchSurface::open(&output, fs.buffer_format, None) {
                        Ok(s) => (Some(s), cap::Negotiation::Unmanaged),
                        Err(e) => (
                            None,
                            cap::Negotiation::Rejected {
                                cause: "unmanaged_fallback_failed".into(),
                                message: e.to_string(),
                            },
                        ),
                    }
                }
                Err(e) => return Err(e.into()),
            };

        let mut samples = Vec::new();
        if let Some(mut surface) = surface {
            surface.set_window_fraction(window_fraction)?;
            if let Some(b) = border {
                surface.set_border(b)?;
            }
            for (i, cv) in sequence.iter().enumerate() {
                surface.set_code_values(*cv)?;
                sleep(settle);
                let raw = device.measure_raw(&setup)?;
                let xyz = raw_to_xyz(&raw, &setup, &cal);
                let xy = xyz.chromaticity().map(|(x, y)| [x, y]);
                eprintln!(
                    "  [{:>3}/{}] cv=({:.3},{:.3},{:.3}) -> X={:.3} Y={:.3} Z={:.3}",
                    i + 1,
                    sequence.len(),
                    cv[0],
                    cv[1],
                    cv[2],
                    xyz.x,
                    xyz.y,
                    xyz.z,
                );
                samples.push(cap::Sample {
                    requested: *cv,
                    measured: cap::Measured {
                        raw: raw.0,
                        xyz: [xyz.x, xyz.y, xyz.z],
                        xy,
                    },
                    context: cap::SampleContext {
                        window_fraction,
                        border,
                        settle_ms,
                    },
                });
            }
            // Leave the panel dark before the next format.
            let _ = surface.set_code_values([0.0, 0.0, 0.0]);
        }

        trials.push(cap::FormatTrial {
            requested: fs.color_description(),
            pixel_format: fs.pixel_format_str().to_string(),
            outcome,
            samples,
        });
    }

    let capture = cap::Capture {
        schema_version: cap::SCHEMA_VERSION,
        timestamp: rfc3339_utc_now(),
        tool: cap::ToolInfo {
            name: "tristim".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            git_revision: None,
        },
        device: cap::DeviceInfo {
            product: if device.is_spyder_2024() {
                "Spyder 2024".into()
            } else {
                "SpyderX2".into()
            },
            usb_pid: device.pid(),
            serial: info.serial.clone(),
            hw_version: info.hw_version,
            cal_index,
        },
        output: cap::OutputInfo {
            name: output.clone(),
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
        trials,
    };

    capture.save(&out_path)?;
    eprintln!("Done. Wrote capture to {out_path}.");
    Ok(())
}

fn to_cap_capabilities(c: &display::ColorCapabilities) -> cap::Capabilities {
    cap::Capabilities {
        supported_transfer_functions: c.transfer_functions.clone(),
        supported_primaries: c.primaries.clone(),
        supported_features: c.features.clone(),
        supported_render_intents: c.render_intents.clone(),
    }
}

// ── format / sequence parsing ───────────────────────────────────────────────

/// A parsed `--format` spec: the buffer format + the description to
/// negotiate (None = unmanaged).
struct FormatSpec {
    token: String,
    buffer_format: BufferFormat,
    description: Option<DescriptionRequest>,
}

impl FormatSpec {
    fn pixel_format_str(&self) -> &'static str {
        match self.buffer_format {
            BufferFormat::Xrgb8888 => "xrgb8888",
            BufferFormat::Xbgr16161616f => "xbgr16161616f",
        }
    }

    /// The capture-schema description mirroring what we requested.
    fn color_description(&self) -> Option<cap::ColorDescription> {
        self.description.as_ref().map(|d| cap::ColorDescription {
            transfer_function: d.transfer_function.clone(),
            primaries: d.primaries.clone(),
            reference_white_nits: d.luminances.map(|l| l.reference_nits),
            mastering: d.mastering.map(|m| cap::Mastering {
                min_luminance_nits: m.min_nits,
                max_luminance_nits: m.max_nits,
                max_cll_nits: m.max_cll_nits,
                max_fall_nits: m.max_fall_nits,
            }),
        })
    }
}

fn parse_format(spec: &str) -> Result<FormatSpec, String> {
    let (name, params_str) = spec.split_once(':').unwrap_or((spec, ""));
    let params = parse_params(params_str)?;
    let token = spec.to_string();

    let mk_mastering = |default_peak: f64| {
        let peak = params.get("peak").copied().unwrap_or(default_peak);
        display::Mastering {
            min_nits: params.get("min").copied().unwrap_or(0.0005),
            max_nits: peak,
            max_cll_nits: params.get("maxcll").copied().unwrap_or(peak),
            max_fall_nits: params.get("maxfall").copied().unwrap_or(peak / 2.0),
        }
    };
    let managed = |bf, tf: &str, prim: &str, mastering| FormatSpec {
        token: token.clone(),
        buffer_format: bf,
        description: Some(DescriptionRequest {
            transfer_function: tf.to_string(),
            primaries: prim.to_string(),
            luminances: None,
            mastering,
        }),
    };

    Ok(match name {
        "unmanaged" => FormatSpec {
            token: token.clone(),
            buffer_format: BufferFormat::Xrgb8888,
            description: None,
        },
        "srgb" => managed(BufferFormat::Xrgb8888, "srgb", "srgb", None),
        "srgb-p3" => managed(BufferFormat::Xrgb8888, "srgb", "display_p3", None),
        "pq-bt2020" => managed(
            BufferFormat::Xbgr16161616f,
            "st2084_pq",
            "bt2020",
            Some(mk_mastering(400.0)),
        ),
        "pq-p3" => managed(
            BufferFormat::Xbgr16161616f,
            "st2084_pq",
            "display_p3",
            Some(mk_mastering(400.0)),
        ),
        other => {
            return Err(format!(
                "unknown format {other:?} (known: unmanaged, srgb, srgb-p3, pq-bt2020, pq-p3)"
            ));
        }
    })
}

fn parse_params(s: &str) -> Result<HashMap<String, f64>, String> {
    let mut m = HashMap::new();
    if s.is_empty() {
        return Ok(m);
    }
    for kv in s.split(',') {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("bad param {kv:?} (expected key=value)"))?;
        let val: f64 = v
            .parse()
            .map_err(|_| format!("bad number in param {kv:?}"))?;
        m.insert(k.to_string(), val);
    }
    Ok(m)
}

/// Parse a `--seq` spec into a list of `0..=1` code-value triples.
fn parse_sequence(spec: &str) -> Result<Vec<[f64; 3]>, String> {
    let (name, arg) = spec.split_once(':').unwrap_or((spec, ""));
    match name {
        "grey" | "gray" => Ok(grey_ramp(parse_count(arg, 11)?)),
        "primaries" => Ok(primary_ramps(parse_count(arg, 5)?)),
        // Fixed seed → reproducible scatter across runs.
        "scatter" => Ok(scatter(parse_count(arg, 32)?, 0x7472_6973_7469_6d01)),
        other => Err(format!(
            "unknown sequence {other:?} (known: grey, primaries, scatter)"
        )),
    }
}

fn parse_count(arg: &str, default: usize) -> Result<usize, String> {
    if arg.is_empty() {
        return Ok(default);
    }
    arg.parse()
        .map_err(|_| format!("bad count {arg:?} (expected a positive integer)"))
}

/// N-step grey ramp from 0 to 1 inclusive.
fn grey_ramp(n: usize) -> Vec<[f64; 3]> {
    let n = n.max(2);
    (0..n)
        .map(|k| {
            let v = k as f64 / (n - 1) as f64;
            [v, v, v]
        })
        .collect()
}

/// Per-channel R/G/B ramps, `n - 1` steps each from `1/(n-1)` to `1.0`
/// (skipping 0, the shared black already covered by grey ramps).
fn primary_ramps(n: usize) -> Vec<[f64; 3]> {
    let n = n.max(2);
    let mut out = Vec::new();
    for ch in 0..3 {
        for k in 1..n {
            let v = k as f64 / (n - 1) as f64;
            let mut rgb = [0.0; 3];
            rgb[ch] = v;
            out.push(rgb);
        }
    }
    out
}

/// `n` uniform code-value triples in `[0, 1)`, deterministic from `seed`
/// (splitmix64) so captures are reproducible and comparable across runs.
fn scatter(n: usize, seed: u64) -> Vec<[f64; 3]> {
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    };
    (0..n).map(|_| [next(), next(), next()]).collect()
}

// ── arg helpers ─────────────────────────────────────────────────────────────

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).cloned()
}

/// All values for a flag that may be repeated (e.g. `--format`).
fn collect_values(args: &[String], flag: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            if let Some(v) = args.get(i + 1) {
                out.push(v.clone());
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn parse_opt<T: std::str::FromStr>(args: &[String], flag: &str, default: T) -> T {
    arg_value(args, flag)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn parse_rgb(s: &str) -> Result<[f64; 3], String> {
    let parts = s
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| format!("bad RGB triple {s:?}"))?;
    if parts.len() != 3 {
        return Err(format!("expected R,G,B (three values), got {s:?}"));
    }
    Ok([parts[0], parts[1], parts[2]])
}

// ── time ────────────────────────────────────────────────────────────────────

/// Current UTC time as an RFC 3339 string (second precision). Dependency-free
/// civil-date conversion (Howard Hinnant's algorithm).
fn rfc3339_utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Days since 1970-01-01 → (year, month, day). Valid across the Gregorian
/// range we care about.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_unmanaged_has_no_description() {
        let f = parse_format("unmanaged").unwrap();
        assert!(f.description.is_none());
        assert_eq!(f.pixel_format_str(), "xrgb8888");
        assert!(f.color_description().is_none());
    }

    #[test]
    fn format_srgb_declares_srgb() {
        let f = parse_format("srgb").unwrap();
        let d = f.description.unwrap();
        assert_eq!(d.transfer_function, "srgb");
        assert_eq!(d.primaries, "srgb");
        assert!(d.mastering.is_none());
    }

    #[test]
    fn format_pq_params_override_mastering() {
        let f = parse_format("pq-bt2020:peak=600,maxfall=300").unwrap();
        assert_eq!(f.pixel_format_str(), "xbgr16161616f");
        let d = f.description.unwrap();
        assert_eq!(d.transfer_function, "st2084_pq");
        assert_eq!(d.primaries, "bt2020");
        let m = d.mastering.unwrap();
        assert_eq!(m.max_nits, 600.0);
        assert_eq!(m.max_cll_nits, 600.0); // defaults to peak
        assert_eq!(m.max_fall_nits, 300.0);
    }

    #[test]
    fn format_unknown_errors() {
        assert!(parse_format("nope").is_err());
        assert!(parse_format("pq-bt2020:peak=x").is_err());
    }

    #[test]
    fn grey_ramp_spans_zero_to_one() {
        let r = grey_ramp(11);
        assert_eq!(r.len(), 11);
        assert_eq!(r[0], [0.0, 0.0, 0.0]);
        assert_eq!(r[10], [1.0, 1.0, 1.0]);
    }

    #[test]
    fn primary_ramps_cover_three_channels() {
        let r = primary_ramps(5);
        assert_eq!(r.len(), 3 * 4); // (n-1) per channel
        assert_eq!(r[3], [1.0, 0.0, 0.0]); // last red step = full red
        assert_eq!(r[7], [0.0, 1.0, 0.0]); // last green step = full green
    }

    #[test]
    fn scatter_is_deterministic_and_in_range() {
        let a = scatter(16, 0x1234);
        let b = scatter(16, 0x1234);
        assert_eq!(a, b);
        assert_ne!(scatter(16, 0x1234), scatter(16, 0x5678));
        for p in a {
            for c in p {
                assert!((0.0..1.0).contains(&c), "{c} out of range");
            }
        }
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-05-24 is 20597 days after the epoch.
        assert_eq!(civil_from_days(20_597), (2026, 5, 24));
    }
}
