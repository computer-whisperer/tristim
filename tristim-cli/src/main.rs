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
//! Subcommands (see `tristim help` for the full option set):
//!   tristim list-outputs                 enumerate connected outputs
//!   tristim info                         open the colorimeter, print HW info
//!   tristim measure [--cal N]            take one XYZ measurement (aim manually)
//!   tristim characterize --output NAME   sweep grey levels to characterize sensor noise / trust
//!   tristim speed --output NAME          push the sensor: per-cell wall time × N at each level
//!   tristim integration --output NAME    sweep `setup.s2` integration time at one level
//!   tristim gamut --output NAME --format SPEC
//!                                        probe one encoding's reproduced gamut
//!   tristim capture --output NAME ...    run a capture session, write JSON
//!   tristim report FILE.json             analyze a capture, print per-trial error

use std::error::Error;
use std::time::Duration;

use tristim_analyze::{GroundTruth, GroundTruthSource, analyze};
use tristim_capture as cap;
use tristim_color::metrics::triangle_area_xy;
use tristim_display::{PatchSurface, list_outputs};
use tristim_driver::{
    AdaptiveTier, Colorimeter, MeasurementConfidence, TrustFlag, override_integration,
};
use tristim_gather as gather;

fn main() -> Result<(), Box<dyn Error>> {
    let argv: Vec<String> = std::env::args().collect();
    let cmd = argv.get(1).map(String::as_str).unwrap_or("help");

    match cmd {
        "list-outputs" | "outputs" | "ls" => cmd_list_outputs(),
        "info" => cmd_info(),
        "measure" => cmd_measure(&argv[2..]),
        "characterize" | "char" => cmd_characterize(&argv[2..]),
        "speed" => cmd_speed(&argv[2..]),
        "integration" => cmd_integration(&argv[2..]),
        "gamut" => cmd_gamut(&argv[2..]),
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
    eprintln!("  speed --output NAME [opts]");
    eprintln!("        Push the sensor as hard as it'll go: per (level × mode) cell, take");
    eprintln!("        K shots back-to-back (timed each) and derive trust at every N in the");
    eprintln!("        repeats list. Answers \"what's the fastest setting that still trusts?\"");
    eprintln!("        Options:");
    eprintln!("          --cal N            calibration index (default: 0)");
    eprintln!("          --shots K          shots per cell (default: 16; caps the repeats list)");
    eprintln!(
        "          --levels a,b,c     grey code values to test (default: 1.0,0.25,0.063,0.0)"
    );
    eprintln!("          --repeats N1,N2,.. N values to derive trust at (default: 1,2,4,8,16)");
    eprintln!("          --mode M           'auto-zero', 'burst', or 'both' (default: both)");
    eprintln!("          --settle-ms N      ms to wait after each level (default: 400)");
    eprintln!("          --prep-secs N      seconds to wait for puck placement (default: 6)");
    eprintln!("  integration --output NAME --integrations a,b,c [opts]");
    eprintln!("        Override the device's integration time (setup.s2) and sweep it at one");
    eprintln!("        grey level. Reports wall time and trust per s2 — answers \"does the");
    eprintln!("        device honor a non-default s2, and does it scale linearly?\"");
    eprintln!("        Options:");
    eprintln!("          --integrations a,b,c  s2 values in ms (required, e.g. 100,300,714,1000)");
    eprintln!("          --level CV         grey code value to sit on (default: 1.0)");
    eprintln!("          --cal N            calibration index (default: 0)");
    eprintln!("          --repeats N        burst repeats per s2 (default: 8)");
    eprintln!("          --settle-ms N      ms to wait after setting the level (default: 400)");
    eprintln!("          --prep-secs N      seconds to wait for puck placement (default: 6)");
    eprintln!("  gamut --output NAME --format SPEC [opts]");
    eprintln!("        Probe the display's reproduced gamut for one encoding by measuring");
    eprintln!("        the code-cube surface (8 corners + 6 face centers), each with a");
    eprintln!("        trust verdict. Options:");
    eprintln!("          --cal N            calibration index (default: 0)");
    eprintln!("          --repeats N        measurements per probe point (default: 4)");
    eprintln!("          --settle-ms N      ms to wait after each patch (default: 250)");
    eprintln!("          --prep-secs N      seconds to wait for puck placement (default: 6)");
    eprintln!("          --window F         centered-window area fraction (default: 1.0)");
    eprintln!("          --border R,G,B     surround code values for windowed/anti-ABL use");
    eprintln!("          --refine           adaptively subdivide the faces + detect clamping");
    eprintln!("          --max-depth N      max subdivision depth with --refine (default: 3)");
    eprintln!("          --fast-integration MS  adaptive integration: bright points probe at MS,");
    eprintln!(
        "                             escalate to default only on untrust (3× speedup at 200)"
    );
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
    eprintln!(
        "          --probe-gamut      probe each format's gamut first, record it on the trial"
    );
    eprintln!("          --gamut-repeats N  repeats per gamut probe point (default: 4)");
    eprintln!("          --gamut-max-depth N  gamut refinement depth (default: 3)");
    eprintln!("          --gamut-fast-integration MS  adaptive integration on the gamut probe");
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
    // that bare repeat-variance hides at low light; d_uv is the same idea for
    // chromaticity (Δu'v'), which degrades far faster toward black. flags mark
    // the floor regime, high luminance noise (NOISY), and coarse color (DUV).
    println!(
        "{:>7}  {:>10}  {:>7}  {:>8}  {:>8}  flags",
        "cv", "Y(cd/m²)", "sY/Y", "d_uv", "floor_s",
    );
    let auto_zero = !burst;
    for &cv in &levels {
        let cv = cv.clamp(0.0, 1.0);
        surface.set_code_values([cv, cv, cv])?;
        std::thread::sleep(Duration::from_millis(settle_ms));
        let raws = device.measure_raw_repeated(&setup, repeats, auto_zero)?;
        let conf = MeasurementConfidence::from_repeats(&raws, &setup, &cal);
        let rs = conf.raw.as_ref().expect("raw stats present on the Spyder path");
        println!(
            "{:>7.4}  {:>10.3}  {:>6.1}%  {:>8}  {:>8.1}  {}",
            cv,
            conf.mean.y,
            100.0 * conf.y_rel_uncertainty(),
            fmt_duv(conf.uv_std()),
            rs.min_floor_sigma,
            fmt_flags(&conf.flags()),
        );
        if verbose {
            for ch in 0..6 {
                println!(
                    "          ch{ch}: mean {:>8.1}  sd {:>6.2}  s5 {:>4}  corrected {:>8.1}  floor {:>7.1}s  {}",
                    rs.raw_mean[ch],
                    rs.raw_std[ch],
                    setup.s5[ch],
                    rs.corrected[ch],
                    rs.floor_sigma[ch],
                    if rs.is_signal[ch] { "signal" } else { "dark" },
                );
            }
        }
    }
    let _ = surface.set_code_values([0.0, 0.0, 0.0]);
    Ok(())
}

/// Format a Δu'v' uncertainty for the table; `—` when undefined (true black).
fn fmt_duv(d: Option<f64>) -> String {
    match d {
        Some(d) if d.is_finite() => format!("{d:.5}"),
        _ => "—".into(),
    }
}

/// Render trust flags for the table; `ok` when none are raised.
fn fmt_flags(flags: &[TrustFlag]) -> String {
    if flags.is_empty() {
        return "ok".into();
    }
    flags
        .iter()
        .map(|f| match f {
            TrustFlag::Floor => "FLOOR",
            TrustFlag::Noisy => "NOISY",
            TrustFlag::Chroma => "DUV",
        })
        .collect::<Vec<_>>()
        .join(",")
}

// ── speed ──────────────────────────────────────────────────────────────────

/// Levels chosen to span the trust regime: white (best case), upper-quarter
/// (well-trusted), the cv≈0.063 boundary where DUV first fires on DP-8, and
/// panel black (worst case). User can override with --levels.
const SPEED_DEFAULT_LEVELS: &[f64] = &[1.0, 0.25, 16.0 / 255.0, 0.0];

const SPEED_DEFAULT_REPEATS: &[usize] = &[1, 2, 4, 8, 16];

fn cmd_speed(args: &[String]) -> Result<(), Box<dyn Error>> {
    let output = arg_value(args, "--output")
        .ok_or("--output NAME is required (try `tristim list-outputs`)")?;
    let cal_index: u8 = parse_opt(args, "--cal", 0);
    let shots: usize = parse_opt(args, "--shots", 16);
    let settle_ms: u64 = parse_opt(args, "--settle-ms", 400);
    let prep_secs: u64 = parse_opt(args, "--prep-secs", 6);
    let levels: Vec<f64> = match arg_value(args, "--levels") {
        Some(s) => parse_f64_list(&s)?,
        None => SPEED_DEFAULT_LEVELS.to_vec(),
    };
    let mut repeats_list: Vec<usize> = match arg_value(args, "--repeats") {
        Some(s) => parse_usize_list(&s)?,
        None => SPEED_DEFAULT_REPEATS.to_vec(),
    };
    repeats_list.retain(|&n| n >= 1 && n <= shots);
    repeats_list.sort_unstable();
    repeats_list.dedup();
    if repeats_list.is_empty() {
        return Err(format!("--repeats: no valid N values (must be 1..={shots})").into());
    }
    let mode_arg = arg_value(args, "--mode").unwrap_or_else(|| "both".into());
    let modes: Vec<bool> = match mode_arg.as_str() {
        "auto-zero" | "auto" => vec![true],
        "burst" => vec![false],
        "both" => vec![true, false],
        other => {
            return Err(format!("--mode: expected auto-zero|burst|both, got {other:?}").into());
        }
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
        "{product} SN {} — speed test, cal {cal_index}, {shots} shots/cell, N ∈ {:?}",
        info.serial, repeats_list,
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

    for &cv in &levels {
        let cv = cv.clamp(0.0, 1.0);
        surface.set_code_values([cv, cv, cv])?;
        std::thread::sleep(Duration::from_millis(settle_ms));
        println!();
        println!("[level cv={cv:.4}]");

        for &auto_zero in &modes {
            let mode_label = if auto_zero { "auto-zero" } else { "burst" };

            if !auto_zero {
                device.send_reset()?;
            }
            let mut raws = Vec::with_capacity(shots);
            let mut shot_ms = Vec::with_capacity(shots);
            for _ in 0..shots {
                let t0 = std::time::Instant::now();
                let raw = if auto_zero {
                    device.measure_raw(&setup)?
                } else {
                    device.measure_raw_no_reset(&setup)?
                };
                let dt = t0.elapsed().as_secs_f64() * 1000.0;
                raws.push(raw);
                shot_ms.push(dt);
            }
            let mean_shot_ms = shot_ms.iter().sum::<f64>() / shot_ms.len() as f64;

            println!("  {mode_label} (t/shot = {:.0} ms):", mean_shot_ms,);
            println!(
                "    {:>4}  {:>8}  {:>10}  {:>8}  {:>9}  {:>8}  flags",
                "N", "total", "Y(cd/m²)", "sY/Y", "Δu'v'", "floor_σ",
            );
            for &n in &repeats_list {
                let conf = MeasurementConfidence::from_repeats(&raws[..n], &setup, &cal);
                let total_s = mean_shot_ms * n as f64 / 1000.0;
                // For N=1 the temporal std is necessarily zero — flag verdict as quant-only
                // so the reader doesn't mistake it for a real trust check.
                let flags_col = if n < 2 {
                    "quant-only".to_string()
                } else {
                    fmt_flags(&conf.flags())
                };
                println!(
                    "    {:>4}  {:>6.2} s  {:>10.3}  {:>7.2}%  {:>9}  {:>8.1}  {}",
                    n,
                    total_s,
                    conf.mean.y,
                    100.0 * conf.y_rel_uncertainty(),
                    fmt_duv(conf.uv_std()),
                    conf.raw.as_ref().map_or(0.0, |rs| rs.min_floor_sigma),
                    flags_col,
                );
            }

            // Drift check: does Y wander across the K shots? Compare mean Y of
            // the first vs the second half. Burst's worry is dark-current creep
            // between resets — a non-zero drift here is the first sign.
            if shots >= 4 {
                let half = shots / 2;
                let first: Vec<_> = raws[..half].to_vec();
                let last: Vec<_> = raws[shots - half..].to_vec();
                let cf = MeasurementConfidence::from_repeats(&first, &setup, &cal);
                let cl = MeasurementConfidence::from_repeats(&last, &setup, &cal);
                let dy = cl.mean.y - cf.mean.y;
                let rel = if cf.mean.y.abs() > 1e-9 {
                    dy / cf.mean.y
                } else {
                    0.0
                };
                println!(
                    "    drift: first {half}={:.3}, last {half}={:.3}  (ΔY={:+.3} = {:+.2}%)",
                    cf.mean.y,
                    cl.mean.y,
                    dy,
                    100.0 * rel,
                );
            }
        }
    }
    let _ = surface.set_code_values([0.0, 0.0, 0.0]);
    Ok(())
}

// ── integration ────────────────────────────────────────────────────────────

fn cmd_integration(args: &[String]) -> Result<(), Box<dyn Error>> {
    let output = arg_value(args, "--output")
        .ok_or("--output NAME is required (try `tristim list-outputs`)")?;
    let integrations_s = arg_value(args, "--integrations")
        .ok_or("--integrations LIST is required (e.g. --integrations 100,300,714)")?;
    let integrations: Vec<u16> = parse_usize_list(&integrations_s)?
        .into_iter()
        .map(|n| n.min(u16::MAX as usize) as u16)
        .collect();
    if integrations.is_empty() {
        return Err("--integrations: empty list".into());
    }
    let cal_index: u8 = parse_opt(args, "--cal", 0);
    let level: f64 = parse_opt(args, "--level", 1.0_f64).clamp(0.0, 1.0);
    let repeats: usize = parse_opt(args, "--repeats", 8);
    let settle_ms: u64 = parse_opt(args, "--settle-ms", 400);
    let prep_secs: u64 = parse_opt(args, "--prep-secs", 6);

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
        "{product} SN {} — integration sweep, cal {cal_index}, level cv={level:.4}, {repeats} burst repeats",
        info.serial,
    );
    eprintln!(
        "  device default: setup.s2 = 0x{:04x} ({} ms) = cal.v2",
        setup.s2, setup.s2,
    );
    eprintln!("  black-cal s5 = {:?}", setup.s5);
    eprintln!("Place the puck flat against '{output}'. Starting in {prep_secs}s.");

    let mut surface = PatchSurface::open_sdr(&output)?;
    surface.set_code_values([level, level, level])?;
    for remaining in (1..=prep_secs).rev() {
        eprintln!("  starting in {remaining}s...");
        std::thread::sleep(Duration::from_secs(1));
    }
    std::thread::sleep(Duration::from_millis(settle_ms));

    // Y here is absolute (the override scales the calibration), so cells should
    // agree across s2 — a quick mental check that the override is doing its job.
    println!(
        "{:>8}  {:>9}  {:>8}  {:>10}  {:>8}  {:>10}  {:>8}  flags",
        "s2(ms)", "t/shot", "raw_max", "Y(cd/m²)", "sY/Y", "Δu'v'", "floor_σ",
    );

    for &s2 in &integrations {
        let (setup_at, cal_at) = match override_integration(&setup, &cal, s2) {
            Ok(pair) => pair,
            Err(e) => {
                println!("{s2:>8}  rejected: {e}");
                continue;
            }
        };

        device.send_reset()?;
        let mut raws = Vec::with_capacity(repeats);
        let mut shot_ms = Vec::with_capacity(repeats);
        // measure_raw_no_reset is the right call here: send_reset above primes
        // the burst, so we burst all repeats then move on (next s2 resets again).
        let mut ok = true;
        for _ in 0..repeats {
            let t0 = std::time::Instant::now();
            match device.measure_raw_no_reset(&setup_at) {
                Ok(r) => {
                    shot_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                    raws.push(r);
                }
                Err(e) => {
                    println!("{s2:>8}  device error: {e}");
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        let mean_shot_ms = shot_ms.iter().sum::<f64>() / shot_ms.len() as f64;
        let conf = MeasurementConfidence::from_repeats(&raws, &setup_at, &cal_at);
        let raw_max = raws
            .iter()
            .map(|r| *r.0.iter().max().unwrap_or(&0))
            .max()
            .unwrap_or(0);
        let flags = if repeats < 2 {
            "quant-only".to_string()
        } else {
            fmt_flags(&conf.flags())
        };
        println!(
            "{:>8}  {:>6.0} ms  {:>8}  {:>10.3}  {:>7.2}%  {:>10}  {:>8.1}  {}",
            s2,
            mean_shot_ms,
            raw_max,
            conf.mean.y,
            100.0 * conf.y_rel_uncertainty(),
            fmt_duv(conf.uv_std()),
            conf.raw.as_ref().map_or(0.0, |rs| rs.min_floor_sigma),
            flags,
        );
    }
    let _ = surface.set_code_values([0.0, 0.0, 0.0]);
    Ok(())
}

// ── gamut ───────────────────────────────────────────────────────────────────

/// sRGB / BT.709 primary triangle area in the xy plane — a familiar yardstick
/// for the measured gamut's coverage.
const SRGB_TRIANGLE_AREA: f64 = 0.1121;

fn cmd_gamut(args: &[String]) -> Result<(), Box<dyn Error>> {
    let output = arg_value(args, "--output")
        .ok_or("--output NAME is required (try `tristim list-outputs`)")?;
    let format_spec =
        arg_value(args, "--format").ok_or("--format SPEC is required (see `tristim help`)")?;
    let cal_index: u8 = parse_opt(args, "--cal", 0);
    let repeats: usize = parse_opt(args, "--repeats", 4);
    let settle_ms: u64 = parse_opt(args, "--settle-ms", 250);
    let prep_secs: u64 = parse_opt(args, "--prep-secs", 6);
    let window_fraction: f64 = parse_opt(args, "--window", 1.0);
    let border: Option<[f64; 3]> = match arg_value(args, "--border") {
        Some(s) => Some(parse_rgb(&s)?),
        None => None,
    };
    let fast_integration_ms: Option<u16> = arg_value(args, "--fast-integration")
        .map(|s| s.parse())
        .transpose()
        .map_err(|_| "--fast-integration: expected integer ms")?;

    let config = gather::GamutConfig {
        output: output.clone(),
        cal_index,
        format: gather::parse_format(&format_spec)?,
        repeats,
        fast_integration_ms,
        settle: Duration::from_millis(settle_ms),
        prep: Duration::from_secs(prep_secs),
        window_fraction,
        border,
    };

    let refine = args.iter().any(|a| a == "--refine");
    let adapt_note = match fast_integration_ms {
        Some(ms) => format!(", adaptive fast={ms}ms"),
        None => String::new(),
    };
    eprintln!(
        "gamut: output={output}, format={format_spec}, {repeats} repeats/point{}{}",
        if refine { ", refine" } else { "" },
        adapt_note,
    );
    eprintln!("Place the puck flat against '{output}'. Probe starts in {prep_secs}s.");

    if refine {
        let params = gather::RefineParams {
            max_depth: parse_opt(args, "--max-depth", 3),
            ..Default::default()
        };
        let mesh = gather::probe_gamut_refined(&config, &params, log_gamut_event, || false)?;
        print_gamut_mesh(&mesh);
    } else {
        let probe = gather::probe_gamut(&config, log_gamut_event, || false)?;
        print_gamut_summary(&probe);
    }
    Ok(())
}

/// Mirror the progress of a gamut probe to stderr.
fn log_gamut_event(ev: gather::GamutEvent) {
    use gather::GamutEvent::*;
    match ev {
        DeviceReady {
            product,
            serial,
            hw_version,
        } => eprintln!(
            "{product} SN {serial} HW {}.{:02}",
            hw_version.0, hw_version.1
        ),
        Negotiation(n) => match n {
            cap::Negotiation::Accepted { identity } => {
                eprintln!("  accepted (identity {identity})")
            }
            cap::Negotiation::Rejected { cause, message } => {
                eprintln!("  rejected: {cause}: {message}")
            }
            cap::Negotiation::Unmanaged => eprintln!("  unmanaged buffer (assumed sRGB)"),
        },
        Countdown { remaining } => eprintln!("  starting in {remaining}s..."),
        Point {
            index,
            total,
            label,
            measured,
            flags,
            tier,
            ..
        } => eprintln!(
            "  [{:>2}/{}] {:>8}  Y={:>8.3}  xy={}  {}{}",
            index + 1,
            total,
            label,
            measured.y,
            fmt_xy(measured.chromaticity().map(|(x, y)| [x, y])),
            fmt_flags(&flags),
            fmt_tier(tier),
        ),
        Measured {
            index,
            code_value,
            measured,
            flags,
            tier,
        } => eprintln!(
            "  [{:>3}] ({:.3},{:.3},{:.3})  Y={:>8.3}  xy={}  {}{}",
            index + 1,
            code_value[0],
            code_value[1],
            code_value[2],
            measured.y,
            fmt_xy(measured.chromaticity().map(|(x, y)| [x, y])),
            fmt_flags(&flags),
            fmt_tier(tier),
        ),
    }
}

/// Trailing badge for the adaptive tier — empty when adaptive is off, so the
/// non-adaptive log stays uncluttered.
fn fmt_tier(t: AdaptiveTier) -> &'static str {
    match t {
        AdaptiveTier::SingleFull => "",
        AdaptiveTier::Fast => " [fast]",
        AdaptiveTier::EscalatedFull => " [esc]",
    }
}

/// Print a refined-mesh summary: vertex/patch counts, the leaf-status
/// breakdown (notably any clamped folds), the white point, and the measured
/// RGB primary triangle vs sRGB.
fn print_gamut_mesh(mesh: &gather::GamutMesh) {
    use gather::PatchStatus::*;
    println!();
    println!(
        "refined gamut: {} measured vertices, {} leaf patches",
        mesh.vertices.len(),
        mesh.patches.len()
    );
    println!(
        "  {} flat, {} folded (clamped), {} max-depth, {} low-trust",
        mesh.count(Flat),
        mesh.count(Folded),
        mesh.count(MaxDepth),
        mesh.count(LowTrust),
    );
    for p in mesh.patches.iter().filter(|p| p.status == Folded) {
        let v = &mesh.vertices[p.corners[0]];
        println!(
            "    fold on face {} near cv ({:.2},{:.2},{:.2})",
            p.face_label(),
            v.code_value[0],
            v.code_value[1],
            v.code_value[2],
        );
    }

    if let Some((wx, wy)) = mesh.white.chromaticity() {
        println!();
        println!("white: xy=({wx:.4}, {wy:.4})  Y={:.2} cd/m²", mesh.white.y);
    }

    let prim = |cv: [f64; 3]| {
        mesh.vertex_at(cv)
            .and_then(|v| v.measured.chromaticity())
            .map(|(x, y)| [x, y])
    };
    if let (Some(r), Some(g), Some(b)) = (
        prim([1.0, 0.0, 0.0]),
        prim([0.0, 1.0, 0.0]),
        prim([0.0, 0.0, 1.0]),
    ) {
        let area = triangle_area_xy(r, g, b);
        println!(
            "measured RGB primary triangle (xy): area {area:.4}  ({:.0}% of sRGB)",
            100.0 * area / SRGB_TRIANGLE_AREA,
        );
    }
}

/// Print the measured gamut: a per-vertex table, the white point, and the
/// measured RGB primary triangle area relative to sRGB.
fn print_gamut_summary(probe: &gather::GamutProbe) {
    println!();
    println!("measured gamut ({} points):", probe.vertices.len());
    println!(
        "{:>8}  {:>16}  {:>9}  {:>17}  flags",
        "point", "code value", "Y(cd/m²)", "xy"
    );
    for v in &probe.vertices {
        println!(
            "{:>8}  {:>16}  {:>9.3}  {:>17}  {}",
            v.label,
            format!(
                "({:.2},{:.2},{:.2})",
                v.code_value[0], v.code_value[1], v.code_value[2]
            ),
            v.measured.y,
            fmt_xy(v.measured.chromaticity().map(|(x, y)| [x, y])),
            fmt_flags(&v.confidence.flags()),
        );
    }

    if let Some((wx, wy)) = probe.white.chromaticity() {
        println!();
        println!("white: xy=({wx:.4}, {wy:.4})  Y={:.2} cd/m²", probe.white.y);
    }

    // Measured RGB primary triangle (xy) vs the sRGB yardstick.
    let prim = |label: &str| {
        probe
            .vertices
            .iter()
            .find(|v| v.label == label)
            .and_then(|v| v.measured.chromaticity())
            .map(|(x, y)| [x, y])
    };
    if let (Some(r), Some(g), Some(b)) = (prim("red"), prim("green"), prim("blue")) {
        let area = triangle_area_xy(r, g, b);
        println!(
            "measured RGB primary triangle (xy): area {area:.4}  ({:.0}% of sRGB)",
            100.0 * area / SRGB_TRIANGLE_AREA,
        );
    }
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
    // Grey/primaries are deterministic and shared across formats; scatter is
    // split out so it can be generated per-format (and gamut-constrained).
    let mut sequence = Vec::new();
    let mut scatter_count = 0;
    for s in &seq_specs {
        if let Some(n) = gather::parse_scatter(s)? {
            scatter_count += n;
        } else {
            sequence.extend(gather::parse_sequence(s)?);
        }
    }
    let scatter = (scatter_count > 0).then_some(gather::ScatterRequest {
        count: scatter_count,
        seed: gather::SCATTER_SEED,
    });

    // Optional per-format gamut-probe prerequisite.
    let gamut = if args.iter().any(|a| a == "--probe-gamut") {
        let fast_ms: Option<u16> = arg_value(args, "--gamut-fast-integration")
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| "--gamut-fast-integration: expected integer ms")?;
        Some(gather::GamutProbeOpts {
            repeats: parse_opt(args, "--gamut-repeats", 4),
            fast_integration_ms: fast_ms,
            refine: gather::RefineParams {
                max_depth: parse_opt(args, "--gamut-max-depth", 3),
                ..Default::default()
            },
        })
    } else {
        None
    };

    let config = gather::CaptureConfig {
        output: output.clone(),
        cal_index,
        settle: Duration::from_millis(settle_ms),
        prep: Duration::from_secs(prep_secs),
        window_fraction,
        border,
        formats,
        sequence,
        scatter,
        gamut,
    };

    let per_format = config.sequence.len() + config.scatter.as_ref().map_or(0, |s| s.count);
    eprintln!(
        "capture: output={output}, {} formats, ~{per_format} samples/format, window={window_fraction}{}, -> {out_path}",
        config.formats.len(),
        if config.gamut.is_some() {
            ", gamut probe"
        } else {
            ""
        },
    );
    eprintln!("Place the puck flat against '{output}'. Capture starts in {prep_secs}s.");

    // Drive the shared gatherer with a stderr-logging callback; the CLI never
    // cancels (run to completion).
    let capture = gather::run_capture(&config, |ev| log_event(&ev), || false)?;

    capture.save(&out_path)?;
    eprintln!("Done. Wrote capture to {out_path}.");
    Ok(())
}

/// Mirror the progress of a capture run to stderr.
fn log_event(ev: &gather::GatherEvent) {
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
        ProbeSample { sample, .. } => {
            let cv = sample.requested;
            let xyz = sample.measured.xyz;
            eprintln!(
                "  [probe] cv=({:.3},{:.3},{:.3}) -> Y={:.3} (×{})",
                cv[0], cv[1], cv[2], xyz[1], sample.repeats,
            );
        }
        GamutProbed {
            vertices, folds, ..
        } => eprintln!("  gamut probed: {vertices} vertices, {folds} folds (clamped)"),
        Sample {
            index,
            total,
            sample,
            ..
        } => {
            let cv = sample.requested;
            let xyz = sample.measured.xyz;
            eprintln!(
                "  [{:>3}/{}] cv=({:.3},{:.3},{:.3}) -> X={:.3} Y={:.3} Z={:.3}",
                index + 1,
                total,
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

fn parse_f64_list(s: &str) -> Result<Vec<f64>, String> {
    s.split(',')
        .map(|p| {
            p.trim()
                .parse::<f64>()
                .map_err(|_| format!("bad number {p:?} in list {s:?}"))
        })
        .collect()
}

fn parse_usize_list(s: &str) -> Result<Vec<usize>, String> {
    s.split(',')
        .map(|p| {
            p.trim()
                .parse::<usize>()
                .map_err(|_| format!("bad integer {p:?} in list {s:?}"))
        })
        .collect()
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
