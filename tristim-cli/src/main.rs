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
//! The capture orchestration itself lives in `tristim-gather` (shared with
//! the GUI); this binary parses args, drives it with a stderr-logging
//! callback, and saves the result.
//!
//! Subcommands:
//!   tristim list-outputs                 enumerate connected outputs
//!   tristim info                         open the colorimeter, print HW info
//!   tristim measure [--cal N]            take one XYZ measurement (aim manually)
//!   tristim capture --output NAME ...    run a capture session, write JSON

use std::error::Error;
use std::time::Duration;

use tristim_analyze::{GroundTruth, GroundTruthSource, analyze};
use tristim_capture as cap;
use tristim_display::{PatchSurface, list_outputs};
use tristim_driver::Colorimeter;
use tristim_driver::measurement::{Calibration, RawMeasurement, Setup, raw_to_xyz};
use tristim_gather as gather;

fn main() -> Result<(), Box<dyn Error>> {
    let argv: Vec<String> = std::env::args().collect();
    let cmd = argv.get(1).map(String::as_str).unwrap_or("help");

    match cmd {
        "list-outputs" | "outputs" | "ls" => cmd_list_outputs(),
        "info" => cmd_info(),
        "measure" => cmd_measure(&argv[2..]),
        "characterize" | "char" => cmd_characterize(&argv[2..]),
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
    eprintln!("  characterize --output NAME [opts]");
    eprintln!("        Sweep grey levels and repeat-measure each to characterize sensor");
    eprintln!("        noise and the black-cal floor (how trustworthy low-light readings");
    eprintln!("        are). Options:");
    eprintln!("          --cal N            calibration index (default: 0)");
    eprintln!("          --repeats N        measurements per level (default: 16)");
    eprintln!("          --levels a,b,c     grey code values to sweep (default: low-light ramp)");
    eprintln!("          --settle-ms N      ms to wait after each level (default: 400)");
    eprintln!("          --prep-secs N      seconds to wait for puck placement (default: 6)");
    eprintln!("          --burst            reset once then read back-to-back (isolates read");
    eprintln!("                             noise); default auto-zeros before every reading");
    eprintln!("          --raw              also print per-channel raw mean/sd/headroom");
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

// ── characterize ──────────────────────────────────────────────────────────

/// Default grey-level sweep, weighted toward the low end where the black-cal
/// floor bites. Powers of two over 8-bit code values (1/255 … 128/255) plus
/// the endpoints — each maps cleanly onto the 8-bit SDR patch surface.
const DEFAULT_LEVELS: &[f64] = &[
    0.0,
    1.0 / 255.0,
    2.0 / 255.0,
    4.0 / 255.0,
    8.0 / 255.0,
    16.0 / 255.0,
    32.0 / 255.0,
    64.0 / 255.0,
    128.0 / 255.0,
    1.0,
];

fn cmd_characterize(args: &[String]) -> Result<(), Box<dyn Error>> {
    let output = arg_value(args, "--output")
        .ok_or("--output NAME is required (try `tristim list-outputs`)")?;
    let cal_index: u8 = parse_opt(args, "--cal", 0);
    let repeats: usize = parse_opt(args, "--repeats", 16);
    let settle_ms: u64 = parse_opt(args, "--settle-ms", 400);
    let prep_secs: u64 = parse_opt(args, "--prep-secs", 6);
    let burst = args.iter().any(|a| a == "--burst");
    let verbose = args.iter().any(|a| a == "--raw" || a == "--verbose");
    let levels: Vec<f64> = match arg_value(args, "--levels") {
        Some(s) => s
            .split(',')
            .map(|p| p.trim().parse::<f64>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| format!("bad --levels list {s:?} (expected comma-separated numbers)"))?,
        None => DEFAULT_LEVELS.to_vec(),
    };

    let mut device = Colorimeter::open_any()?;
    let info = device.get_info()?;
    let cal = device.get_calibration(cal_index)?;
    let setup = device.get_setup(&cal)?;
    let product = if device.is_spyder_2024() {
        "Spyder 2024"
    } else {
        "SpyderX2"
    };

    eprintln!(
        "{product} SN {} — characterize cal {cal_index}, {repeats} repeats/level, auto-zero {}",
        info.serial,
        if burst { "off (burst)" } else { "on" },
    );
    eprintln!(
        "  black-cal s5 = {:?}   integration s2 = 0x{:04x}",
        setup.s5, setup.s2,
    );
    eprintln!("Place the puck flat against '{output}'. Starting in {prep_secs}s.");

    let mut surface = PatchSurface::open_sdr(&output)?;
    surface.set_code_values([0.0, 0.0, 0.0])?;
    for remaining in (1..=prep_secs).rev() {
        eprintln!("  starting in {remaining}s...");
        std::thread::sleep(Duration::from_secs(1));
    }

    // floor_s = how many noise-σ the dimmest *signal* channel sits above its
    // black-cal floor (s5). Small ⇒ the max(0, raw−s5) clamp is biting and the
    // reading is untrustworthy. sY/Y folds in the ±½-count quantization floor
    // that bare repeat-variance hides at low light. flags mark both regimes.
    println!(
        "{:>7}  {:>10}  {:>9}  {:>7}  {:>8}  flags",
        "cv", "Y(cd/m²)", "sigmaY", "sY/Y", "floor_s",
    );
    let auto_zero = !burst;
    for &cv in &levels {
        let cv = cv.clamp(0.0, 1.0);
        surface.set_code_values([cv, cv, cv])?;
        std::thread::sleep(Duration::from_millis(settle_ms));
        let raws = device.measure_raw_repeated(&setup, repeats, auto_zero)?;
        let st = LevelStats::compute(cv, &raws, &setup, &cal);
        println!(
            "{:>7.4}  {:>10.3}  {:>9.4}  {:>6.1}%  {:>8.1}  {}",
            st.cv,
            st.y_mean,
            st.y_std(),
            st.rel_noise_pct(),
            st.min_floor_sigma,
            st.flags(),
        );
        if verbose {
            for ch in 0..6 {
                println!(
                    "          ch{ch}: mean {:>8.1}  sd {:>6.2}  s5 {:>4}  corrected {:>8.1}  floor {:>7.1}s  {}",
                    st.raw_mean[ch],
                    st.raw_std[ch],
                    setup.s5[ch],
                    st.corrected[ch],
                    st.floor_sigma[ch],
                    if st.is_signal[ch] { "signal" } else { "dark" },
                );
            }
        }
    }
    let _ = surface.set_code_values([0.0, 0.0, 0.0]);
    Ok(())
}

/// 1σ-equivalent uncertainty of a single integer count (uniform quantization
/// noise, `q/√12`). Used as a noise floor that repeat-variance can't reveal:
/// when the integer counts happen to agree across repeats, the spread reads as
/// zero even though the reading is only good to ±½ count.
const QUANT_SIGMA: f64 = 0.288_675_13; // 1.0 / 12_f64.sqrt()

/// Per-level repeat statistics. Computed in the CLI (not the driver) while the
/// trust metrics are still being worked out — the driver just hands back raw
/// repeats.
struct LevelStats {
    cv: f64,
    /// Per-channel mean / sample-std of the raw sensor counts.
    raw_mean: [f64; 6],
    raw_std: [f64; 6],
    /// Per-channel black-cal-corrected counts, `max(0, raw_mean − s5)`.
    corrected: [f64; 6],
    /// Which channels carry signal at this level: corrected count above ~1% of
    /// the brightest channel and at least one count. The Spyder's dark/unused
    /// channels (cal 0: ch3/ch4) fall out here and are kept out of every trust
    /// metric, so a permanently-floored channel can't poison the verdict.
    is_signal: [bool; 6],
    /// Per-channel headroom above the black-cal floor in noise-σ units:
    /// `corrected / hypot(raw_std, QUANT_SIGMA)`.
    floor_sigma: [f64; 6],
    /// The worst *signal* channel's `floor_sigma` — what limits trust. Small ⇒
    /// a contributing channel is near its floor and the `max(0, raw−s5)` clamp
    /// is starting to rectify (bias) the reading. `0` when nothing carries signal.
    min_floor_sigma: f64,
    /// Brightest channel's corrected count: the overall signal level in counts.
    max_corrected: f64,
    /// Mean measured Y across the repeats (full raw→XYZ pipeline).
    y_mean: f64,
    /// Temporal (repeat) σ of Y, and the quantization-floor σ that repeats
    /// can't see (±½-count discretization propagated through the Y row). The
    /// reported uncertainty [`y_std`](Self::y_std) combines them in quadrature.
    y_repeat_std: f64,
    y_quant_std: f64,
}

impl LevelStats {
    fn compute(cv: f64, raws: &[RawMeasurement], setup: &Setup, cal: &Calibration) -> Self {
        let mut raw_mean = [0.0; 6];
        let mut raw_std = [0.0; 6];
        for ch in 0..6 {
            let vals: Vec<f64> = raws.iter().map(|r| r.0[ch] as f64).collect();
            raw_mean[ch] = mean(&vals);
            raw_std[ch] = sample_std(&vals, raw_mean[ch]);
        }

        // Black-cal-corrected counts and the signal-channel mask. A channel
        // counts as signal only if it rises meaningfully above its own floor
        // (>1% of the brightest channel, and ≥1 count); this drops the dark
        // channels that otherwise pin every floor metric at zero.
        let mut corrected = [0.0; 6];
        for ch in 0..6 {
            corrected[ch] = (raw_mean[ch] - setup.s5[ch] as f64).max(0.0);
        }
        let max_corrected = corrected.iter().copied().fold(0.0, f64::max);
        let signal_threshold = (max_corrected * 0.01).max(1.0);
        let mut is_signal = [false; 6];
        for ch in 0..6 {
            is_signal[ch] = corrected[ch] >= signal_threshold;
        }

        // Headroom above the floor in σ-units (repeat noise ⊕ quantization),
        // limited by the worst signal channel.
        let mut floor_sigma = [0.0; 6];
        let mut min_floor_sigma = f64::INFINITY;
        for ch in 0..6 {
            floor_sigma[ch] = corrected[ch] / raw_std[ch].hypot(QUANT_SIGMA);
            if is_signal[ch] {
                min_floor_sigma = min_floor_sigma.min(floor_sigma[ch]);
            }
        }
        if !min_floor_sigma.is_finite() {
            min_floor_sigma = 0.0; // nothing above the floor — we're measuring it
        }

        // Y from the full pipeline per repeat → temporal σ.
        let ys: Vec<f64> = raws.iter().map(|r| raw_to_xyz(r, setup, cal).y).collect();
        let y_mean = mean(&ys);
        let y_repeat_std = sample_std(&ys, y_mean);

        // Quantization floor on Y: ±½-count on each signal channel, propagated
        // through the Y row of the calibration matrix (with its gain). Repeats
        // can't reveal this when the integer counts happen to agree.
        let mut quant_var = 0.0;
        for (ch, &signal) in is_signal.iter().enumerate() {
            if signal {
                let w = cal.matrix[1][ch] * cal.gain[1];
                quant_var += (w * QUANT_SIGMA).powi(2);
            }
        }
        let y_quant_std = quant_var.sqrt();

        LevelStats {
            cv,
            raw_mean,
            raw_std,
            corrected,
            is_signal,
            floor_sigma,
            min_floor_sigma,
            max_corrected,
            y_mean,
            y_repeat_std,
            y_quant_std,
        }
    }

    /// Combined Y uncertainty: temporal repeat noise ⊕ quantization floor.
    fn y_std(&self) -> f64 {
        self.y_repeat_std.hypot(self.y_quant_std)
    }

    /// Relative luminance uncertainty σY/Y as a percentage; `inf` at true black.
    fn rel_noise_pct(&self) -> f64 {
        if self.y_mean.abs() < 1e-9 {
            f64::INFINITY
        } else {
            100.0 * self.y_std() / self.y_mean
        }
    }

    fn flags(&self) -> String {
        let mut f = Vec::new();
        // Near the black-cal floor: either nothing carries usable signal, or the
        // limiting signal channel is within a few σ of its floor (the
        // max(0, raw−s5) clamp begins to rectify/bias the reading).
        if self.max_corrected < 1.0 || self.min_floor_sigma < 3.0 {
            f.push("FLOOR");
        }
        if self.rel_noise_pct() > 5.0 {
            f.push("NOISY");
        }
        if f.is_empty() {
            "ok".into()
        } else {
            f.join(",")
        }
    }
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

/// Sample standard deviation (Bessel-corrected, `n−1`); `0` for fewer than two
/// samples.
fn sample_std(v: &[f64], mean: f64) -> f64 {
    if v.len() < 2 {
        return 0.0;
    }
    let var = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (v.len() - 1) as f64;
    var.sqrt()
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

    let formats = collect_values(args, "--format")
        .iter()
        .map(|s| gather::parse_format(s))
        .collect::<Result<Vec<_>, _>>()?;
    if formats.is_empty() {
        return Err("at least one --format is required (see `tristim help`)".into());
    }

    let seq_specs = collect_values(args, "--seq");
    if seq_specs.is_empty() {
        return Err("at least one --seq is required (see `tristim help`)".into());
    }
    let mut sequence = Vec::new();
    for s in &seq_specs {
        sequence.extend(gather::parse_sequence(s)?);
    }

    let config = gather::CaptureConfig {
        output: output.clone(),
        cal_index,
        settle: Duration::from_millis(settle_ms),
        prep: Duration::from_secs(prep_secs),
        window_fraction,
        border,
        formats,
        sequence,
    };

    eprintln!(
        "capture: output={output}, {} formats, {} samples/format, window={window_fraction}, -> {out_path}",
        config.formats.len(),
        config.sequence.len(),
    );
    eprintln!("Place the puck flat against '{output}'. Capture starts in {prep_secs}s.");

    // Drive the shared gatherer with a stderr-logging callback; the CLI never
    // cancels (run to completion).
    let seq_len = config.sequence.len();
    let capture = gather::run_capture(&config, |ev| log_event(&ev, seq_len), || false)?;

    capture.save(&out_path)?;
    eprintln!("Done. Wrote capture to {out_path}.");
    Ok(())
}

/// Mirror the progress of a capture run to stderr.
fn log_event(ev: &gather::GatherEvent, seq_len: usize) {
    use gather::GatherEvent::*;
    match ev {
        DeviceReady {
            product,
            serial,
            hw_version,
        } => eprintln!(
            "{product} SN {serial} HW {}.{:02}",
            hw_version.0, hw_version.1
        ),
        Countdown { remaining } => eprintln!("  starting in {remaining}s..."),
        FormatStart {
            token,
            pixel_format,
            ..
        } => eprintln!("── format {token} ({pixel_format}) ──"),
        Negotiation(n) => match n {
            cap::Negotiation::Accepted { identity } => {
                eprintln!("  accepted (identity {identity})")
            }
            cap::Negotiation::Rejected { cause, message } => {
                eprintln!("  compositor rejected this format: {cause}: {message}")
            }
            cap::Negotiation::Unmanaged => eprintln!("  unmanaged buffer (assumed sRGB)"),
        },
        Sample { index, sample, .. } => {
            let cv = sample.requested;
            let xyz = sample.measured.xyz;
            eprintln!(
                "  [{:>3}/{}] cv=({:.3},{:.3},{:.3}) -> X={:.3} Y={:.3} Z={:.3}",
                index + 1,
                seq_len,
                cv[0],
                cv[1],
                cv[2],
                xyz[0],
                xyz[1],
                xyz[2],
            );
        }
        FormatDone { .. } => {}
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_and_std_basic() {
        let v = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let m = mean(&v);
        assert!((m - 5.0).abs() < 1e-12);
        // Sample (n−1) std of this classic set is 2.138...
        assert!((sample_std(&v, m) - 2.1380899).abs() < 1e-6);
    }

    #[test]
    fn std_degenerate_cases() {
        assert_eq!(mean(&[]), 0.0);
        assert_eq!(sample_std(&[], 0.0), 0.0);
        assert_eq!(sample_std(&[3.0], 3.0), 0.0); // need ≥2 samples
        assert_eq!(sample_std(&[5.0, 5.0, 5.0], 5.0), 0.0); // no spread
    }

    /// Dark/unused channels must not poison the verdict: a level whose live
    /// channels are well above the floor reads "ok" even with some channels
    /// pinned at their floor. A level where *everything* sits at the floor
    /// carries no signal and flags FLOOR.
    #[test]
    fn level_stats_excludes_dark_channels_and_flags_floor() {
        let cal = unit_cal();
        let setup = setup_with_floor([20; 6]);

        // ch0–2 well above the floor; ch3–5 pinned at it (dark). The dark
        // channels must drop out of the signal set, not drag floor_σ to zero.
        let bright = vec![
            RawMeasurement([200, 200, 200, 20, 20, 20]),
            RawMeasurement([202, 198, 201, 20, 20, 20]),
            RawMeasurement([198, 202, 199, 20, 20, 20]),
        ];
        let st = LevelStats::compute(0.5, &bright, &setup, &cal);
        assert_eq!(st.is_signal, [true, true, true, false, false, false]);
        assert!(st.min_floor_sigma > 3.0);
        assert_eq!(st.flags(), "ok");

        // Every channel at/below its floor: nothing carries signal → FLOOR.
        let dark = vec![
            RawMeasurement([20, 18, 21, 19, 20, 22]),
            RawMeasurement([19, 21, 20, 18, 21, 19]),
            RawMeasurement([21, 19, 19, 20, 20, 20]),
        ];
        let st = LevelStats::compute(0.004, &dark, &setup, &cal);
        assert!(
            st.is_signal.iter().all(|&s| !s),
            "no channel should count as signal at the floor"
        );
        assert!(st.flags().contains("FLOOR"));
    }

    fn unit_cal() -> Calibration {
        // Y = sum of corrected counts (row 1 all ones); X/Z irrelevant here.
        let mut matrix = [[0.0; 6]; 3];
        matrix[1] = [1.0; 6];
        Calibration {
            index: 0,
            v1: 0,
            v2: 0,
            v4: [0; 6],
            matrix,
            gain: [1.0; 3],
            offset: [0.0; 3],
            v3: 0,
        }
    }

    fn setup_with_floor(s5: [u8; 6]) -> Setup {
        Setup {
            s1: 0,
            s2: 0,
            s3: [0; 6],
            s4: [0; 6],
            s5,
        }
    }
}
