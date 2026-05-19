//! `spyder` — orchestrate the Spyder colorimeter + a Wayland test-patch
//! surface to characterize displays.
//!
//! Subcommands:
//!   spyder list-outputs                          enumerate connected outputs
//!   spyder info                                  open the spyder, print HW info
//!   spyder measure [--cal N]                     take one XYZ measurement
//!                                                (puck must be aimed manually)
//!   spyder sweep --output NAME [opts]            walk a color set on NAME,
//!                                                measure each, write CSV
//!   spyder analyze FILE.csv [FILE.csv ...]       summarize sweep(s)

mod analyze;

use spyder_display::{list_outputs, PatchSurface};
use spyder_driver::Spyder;
use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Duration;

fn main() -> Result<(), Box<dyn Error>> {
    let argv: Vec<String> = std::env::args().collect();
    let cmd = argv.get(1).map(String::as_str).unwrap_or("help");

    match cmd {
        "list-outputs" | "outputs" | "ls" => cmd_list_outputs(),
        "info" => cmd_info(),
        "measure" => cmd_measure(&argv[2..]),
        "sweep" => cmd_sweep(&argv[2..]),
        "analyze" | "analyse" => analyze::run(&argv[2..]),
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
    eprintln!("USAGE: spyder <subcommand> [opts]");
    eprintln!();
    eprintln!("  list-outputs              enumerate Wayland outputs");
    eprintln!("  info                      open the Spyder, print HW info");
    eprintln!("  measure [--cal N]         take one XYZ measurement (aim puck manually)");
    eprintln!("  sweep --output NAME [opts]");
    eprintln!("        Walk a color set on NAME, measuring each. Options:");
    eprintln!("          --out FILE           write CSV (default: sweep.csv)");
    eprintln!("          --cal N              calibration index (default: 0)");
    eprintln!("          --grey-steps N       grayscale ramp size (default: 11)");
    eprintln!("          --prep-secs N        seconds to wait for puck placement (default: 6)");
    eprintln!("          --settle-ms N        ms to wait after each color change (default: 250)");
    eprintln!("  analyze FILE.csv [FILE.csv ...]");
    eprintln!("        Summarize one sweep (detailed) or compare several (table + ΔuV matrix).");
}

fn cmd_list_outputs() -> Result<(), Box<dyn Error>> {
    for o in list_outputs()? {
        println!(
            "{:14} {:25} {:>10}   {}",
            o.name,
            o.model,
            o.size
                .map(|(w, h)| format!("{}x{}", w, h))
                .unwrap_or_else(|| "?".into()),
            o.description
        );
    }
    Ok(())
}

fn cmd_info() -> Result<(), Box<dyn Error>> {
    let mut spyder = Spyder::open_any()?;
    let info = spyder.get_info()?;
    println!(
        "{} (PID 0x{:04x}) — HW {}.{:02} — SN {}",
        if spyder.is_spyder_2024() {
            "Spyder 2024"
        } else {
            "SpyderX2"
        },
        spyder.pid(),
        info.hw_version.0,
        info.hw_version.1,
        info.serial,
    );
    println!("high-level cmds: {}", info.high_level_commands);
    Ok(())
}

fn cmd_measure(args: &[String]) -> Result<(), Box<dyn Error>> {
    let cal: u8 = arg_value(args, "--cal")
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut spyder = Spyder::open_any()?;
    let info = spyder.get_info()?;
    println!(
        "Spyder 2024 SN {} — HW {}.{:02}",
        info.serial, info.hw_version.0, info.hw_version.1
    );

    println!("Measuring (cal {})... aim the puck.", cal);
    let (xyz, raw, _, _) = spyder.measure_xyz(cal)?;
    println!("Raw  : {:?}", raw.0);
    println!("X={:.4}  Y={:.4} cd/m²  Z={:.4}", xyz.x, xyz.y, xyz.z);
    if let Some((x, y)) = xyz.chromaticity() {
        println!("xy   : ({:.4}, {:.4})", x, y);
    }
    Ok(())
}

fn cmd_sweep(args: &[String]) -> Result<(), Box<dyn Error>> {
    let output = arg_value(args, "--output")
        .ok_or("--output NAME is required (try `spyder list-outputs`)")?;
    let out_path: PathBuf = arg_value(args, "--out").map(PathBuf::from).unwrap_or_else(|| "sweep.csv".into());
    let cal_index: u8 = arg_value(args, "--cal")
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let grey_steps: usize = arg_value(args, "--grey-steps")
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(11);
    let prep_secs: u64 = arg_value(args, "--prep-secs")
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    let settle_ms: u64 = arg_value(args, "--settle-ms")
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(250);

    // Build the color set:
    //   - grayscale ramp (0..1 in N steps)
    //   - R/G/B ramps (4 steps each, 0.25 / 0.50 / 0.75 / 1.00)
    //   - extra: pure white and pure black bookends are already in greys
    let mut patches: Vec<Patch> = Vec::new();
    for k in 0..grey_steps {
        let v = k as f64 / (grey_steps - 1) as f64;
        patches.push(Patch::new(format!("grey_{:03}", (v * 1000.0) as i32), [v, v, v]));
    }
    for &v in &[0.25, 0.5, 0.75, 1.0] {
        patches.push(Patch::new(format!("red_{:03}", (v * 1000.0) as i32), [v, 0.0, 0.0]));
        patches.push(Patch::new(format!("grn_{:03}", (v * 1000.0) as i32), [0.0, v, 0.0]));
        patches.push(Patch::new(format!("blu_{:03}", (v * 1000.0) as i32), [0.0, 0.0, v]));
    }

    eprintln!(
        "sweep: output={}, cal={}, {} patches, settle {}ms, csv -> {}",
        output, cal_index, patches.len(), settle_ms, out_path.display()
    );

    // Open device + display surface up front so we fail fast if either is broken.
    let mut spyder = Spyder::open_any()?;
    let info = spyder.get_info()?;
    eprintln!("Spyder SN {} HW {}.{:02}", info.serial, info.hw_version.0, info.hw_version.1);

    // Pre-fetch calibration + setup once. (We re-fetch setup before each
    // measure inside the driver, but downloading the cal matrix is slow.)
    let cal = spyder.get_calibration(cal_index)?;
    eprintln!(
        "cal[{}] downloaded: gain={:?}, offset={:?}",
        cal_index, cal.gain, cal.offset
    );

    let mut patch_surface = PatchSurface::open(&output)?;

    // Initial dark patch so the user knows where to put the puck.
    patch_surface.set_color([0.0, 0.0, 0.0])?;
    eprintln!(
        "Place the puck flat against output '{}' now. Sweep starts in {}s.",
        output, prep_secs
    );
    for sec in (1..=prep_secs).rev() {
        eprintln!("  starting in {sec}s...");
        std::thread::sleep(Duration::from_secs(1));
    }

    // CSV
    let csv_file = File::create(&out_path)?;
    let mut csv = BufWriter::new(csv_file);
    writeln!(
        csv,
        "name,r_in,g_in,b_in,raw0,raw1,raw2,raw3,raw4,raw5,X,Y,Z,x,y"
    )?;

    let settle = Duration::from_millis(settle_ms);
    let mut max_y = 0.0f64;
    for (idx, patch) in patches.iter().enumerate() {
        eprint!(
            "[{:>2}/{}] {:10} ({:.2}, {:.2}, {:.2}) ... ",
            idx + 1,
            patches.len(),
            patch.name,
            patch.rgb[0],
            patch.rgb[1],
            patch.rgb[2]
        );
        patch_surface.set_color(patch.rgb)?;
        std::thread::sleep(settle);

        // Take one measurement.
        let setup = spyder.get_setup(&cal)?;
        let raw = spyder.measure_raw(&setup)?;
        let xyz = spyder_driver::measurement::raw_to_xyz(&raw, &setup, &cal);
        let chroma = xyz.chromaticity().unwrap_or((0.0, 0.0));

        eprintln!(
            "X={:.3} Y={:.3} Z={:.3}  xy=({:.4},{:.4})",
            xyz.x, xyz.y, xyz.z, chroma.0, chroma.1
        );
        max_y = max_y.max(xyz.y);

        writeln!(
            csv,
            "{},{},{},{},{},{},{},{},{},{},{:.6},{:.6},{:.6},{:.6},{:.6}",
            patch.name,
            patch.rgb[0],
            patch.rgb[1],
            patch.rgb[2],
            raw.0[0],
            raw.0[1],
            raw.0[2],
            raw.0[3],
            raw.0[4],
            raw.0[5],
            xyz.x,
            xyz.y,
            xyz.z,
            chroma.0,
            chroma.1,
        )?;
    }

    // Black patch on the way out so the panel isn't left glaring.
    let _ = patch_surface.set_color([0.0, 0.0, 0.0]);

    eprintln!();
    eprintln!("Done. Peak measured Y = {:.2} cd/m². CSV at {}.", max_y, out_path.display());
    Ok(())
}

struct Patch {
    name: String,
    rgb: [f64; 3],
}

impl Patch {
    fn new(name: String, rgb: [f64; 3]) -> Self {
        Self { name, rgb }
    }
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).cloned()
}
