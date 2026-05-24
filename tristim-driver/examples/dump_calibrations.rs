//! Dump every calibration preset the device carries, with optional
//! measurement-comparison mode.
//!
//! Spyder colorimeters store up to 7 per-display-technology calibrations
//! (indices 0..=6 on the 2024 generation; index 0 is documented as
//! "General"). Datacolor's official software labels the other indices
//! ("LCD CCFL", "LCD WLED", "OLED", etc.) but the device firmware
//! itself doesn't return those labels — it just gives us the raw matrix
//! plus a few scalars.
//!
//! This tool downloads every cal index the device acknowledges and
//! prints:
//!  - Matrix summary (norms, diagonal-vs-off-diagonal, "trace") — lets
//!    us spot which indices are structurally distinct.
//!  - The matrix itself (3×6), gain, offset, v1/v2/v4 — lets us
//!    cross-reference against any external Spyder protocol notes.
//!
//! Pass `--measure` to also take ONE raw 6-channel sample and run it
//! through each calibration. That lets us see, empirically, how each
//! preset interprets the same scene — which is the most direct way to
//! identify the right preset for a given panel ("the one whose D65
//! reading is closest to the panel's actual D65").
//!
//! Usage:
//!   cargo run -p tristim-driver --example dump_calibrations
//!   cargo run -p tristim-driver --example dump_calibrations -- --measure

use std::time::Duration;
use tristim_driver::Colorimeter;
use tristim_driver::measurement::raw_to_xyz;

const MAX_INDEX: u8 = 7;
const PREP_SECS: u64 = 5;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let measure = std::env::args().any(|a| a == "--measure");

    let mut device = Colorimeter::open_any()?;
    let info = device.get_info()?;
    eprintln!(
        "Spyder SN {:?} HW {}.{:02}",
        info.serial, info.hw_version.0, info.hw_version.1
    );
    if let Some(mask) = info.display_type_mask {
        eprintln!(
            "Device display-type mask: 0x{mask:04x} (high-level-cmd presets the firmware advertises)"
        );
    }
    if let Some(mx) = info.max_display_type {
        eprintln!("Device max display-type (high-level): {mx}");
    }

    // Download every cal we can read. Some indices may fail / return
    // duplicates of index 0 if the device only has fewer than 7 unique
    // sets — we report what we find.
    let mut cals: Vec<(u8, tristim_driver::measurement::Calibration)> = Vec::new();
    for idx in 0..MAX_INDEX {
        match device.get_calibration(idx) {
            Ok(c) => {
                eprintln!(
                    "  cal {idx}: OK (v1={:#04x} v2={} v3={:#04x})",
                    c.v1, c.v2, c.v3
                );
                cals.push((idx, c));
            }
            Err(e) => {
                eprintln!("  cal {idx}: download FAILED ({e})");
            }
        }
    }
    if cals.is_empty() {
        eprintln!("No calibrations downloaded; bailing.");
        return Ok(());
    }

    // Structural summary table.
    println!();
    println!(
        "┌─────┬──────┬──────┬────────────┬────────────┬────────────┬─────────────┬──────────────┬─────────────┬─────────────┐"
    );
    println!(
        "│ idx │  v1  │  v2  │ ‖row X‖    │ ‖row Y‖    │ ‖row Z‖    │ diag-dom    │ off-diag mag │ gain        │ offset      │"
    );
    println!(
        "├─────┼──────┼──────┼────────────┼────────────┼────────────┼─────────────┼──────────────┼─────────────┼─────────────┤"
    );
    for (idx, c) in &cals {
        let row_norm = |i: usize| c.matrix[i].iter().map(|x| x * x).sum::<f64>().sqrt();
        let (rx, ry, rz) = (row_norm(0), row_norm(1), row_norm(2));

        // "Diagonal dominance" for a 3×6 matrix is awkward — pair the
        // 3 XYZ rows with the 3 "best-correlated" raw channels.
        let mut diag_mag = 0.0;
        let mut off_mag = 0.0;
        for i in 0..3 {
            // Take the two largest-magnitude entries in this row as
            // the "primary" channels for XYZ[i]; the rest are off-diag.
            let mut row: Vec<(usize, f64)> = c.matrix[i]
                .iter()
                .enumerate()
                .map(|(j, v)| (j, v.abs()))
                .collect();
            row.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            diag_mag += row[0].1 + row[1].1;
            off_mag += row[2..].iter().map(|(_, v)| *v).sum::<f64>();
        }

        println!(
            "│  {} │ {:#04x} │ {:>4} │ {:>10.4} │ {:>10.4} │ {:>10.4} │ {:>11.4} │ {:>12.4} │ {:.2} {:.2} {:.2} │ {:>+.2} {:>+.2} {:>+.2} │",
            idx,
            c.v1,
            c.v2,
            rx,
            ry,
            rz,
            diag_mag,
            off_mag,
            c.gain[0],
            c.gain[1],
            c.gain[2],
            c.offset[0],
            c.offset[1],
            c.offset[2],
        );
    }
    println!(
        "└─────┴──────┴──────┴────────────┴────────────┴────────────┴─────────────┴──────────────┴─────────────┴─────────────┘"
    );

    // Duplicate detection — two calibrations with identical matrices
    // are effectively the same preset (firmware may only ship N unique
    // calibrations and pad the rest).
    println!("\nDuplicate detection (matrices compared element-wise, tolerance 1e-9):");
    let mut group_of: Vec<usize> = (0..cals.len()).collect();
    for i in 0..cals.len() {
        for j in 0..i {
            if matrices_equal(&cals[i].1.matrix, &cals[j].1.matrix)
                && cals[i].1.gain == cals[j].1.gain
                && cals[i].1.offset == cals[j].1.offset
            {
                group_of[i] = group_of[j];
                break;
            }
        }
    }
    let mut shown: Vec<usize> = Vec::new();
    for (i, (idx, _)) in cals.iter().enumerate() {
        if !shown.contains(&group_of[i]) {
            let members: Vec<u8> = cals
                .iter()
                .enumerate()
                .filter(|(j, _)| group_of[*j] == group_of[i])
                .map(|(_, (id, _))| *id)
                .collect();
            if members.len() > 1 {
                println!("  group: indices {members:?} share the same calibration");
            } else {
                println!("  index {idx} is unique");
            }
            shown.push(group_of[i]);
        }
    }

    // Full per-calibration matrix dump.
    println!("\n--- Full matrices ---");
    for (idx, c) in &cals {
        println!(
            "\ncal index {idx}  v1={:#04x} v2={} v3={:#04x} v4={:02x?}",
            c.v1, c.v2, c.v3, c.v4
        );
        for i in 0..3 {
            let label = ["X", "Y", "Z"][i];
            print!("  {label}:");
            for j in 0..6 {
                print!(" {:>+10.5}", c.matrix[i][j]);
            }
            println!();
        }
        println!(
            "  gain:   {:.4} {:.4} {:.4}",
            c.gain[0], c.gain[1], c.gain[2]
        );
        println!(
            "  offset: {:+.4} {:+.4} {:+.4}",
            c.offset[0], c.offset[1], c.offset[2]
        );
    }

    if !measure {
        println!(
            "\n(re-run with --measure to also take one sample and show how each cal reads it)"
        );
        return Ok(());
    }

    // Measurement comparison: one raw sample, applied through every cal.
    println!("\n--- Measurement comparison ---");
    println!("Place the puck on a known white target (e.g. a moderate-brightness");
    println!("D65 patch from prism-tune). Measurement in {PREP_SECS} s.");
    for s in (1..=PREP_SECS).rev() {
        eprintln!("  starting in {s}s...");
        std::thread::sleep(Duration::from_secs(1));
    }
    // Use cal 0's setup to take the raw sample — the raw 6-channel
    // counts don't depend on the cal matrix, only on the setup
    // parameters (integration time, channel routing). Argyll uses the
    // same setup regardless of which cal you apply.
    let (idx0, ref cal0) = cals[0];
    let setup0 = device.get_setup(cal0)?;
    eprintln!(
        "Using setup from cal {idx0} (v1={:#04x}, integ={} ms) for the raw measurement.",
        setup0.s1, cal0.v2
    );
    let raw = device.measure_raw(&setup0)?;
    println!("\nRaw 6-channel counts: {:?}", raw.0);

    println!();
    println!(
        "┌─────┬───────────┬───────────┬───────────┬──────────┬──────────┬─────────────────────┐"
    );
    println!(
        "│ idx │     X     │     Y     │     Z     │     x    │     y    │ Δu'v' from D65       │"
    );
    println!(
        "├─────┼───────────┼───────────┼───────────┼──────────┼──────────┼─────────────────────┤"
    );
    for (idx, c) in &cals {
        let xyz = raw_to_xyz(&raw, &setup0, c);
        let (cx, cy) = xyz.chromaticity().unwrap_or((0.0, 0.0));
        let duv = duv_from_d65(cx, cy);
        println!(
            "│  {} │ {:>9.3} │ {:>9.3} │ {:>9.3} │ {:>8.4} │ {:>8.4} │ {:>19.5} │",
            idx, xyz.x, xyz.y, xyz.z, cx, cy, duv,
        );
    }
    println!(
        "└─────┴───────────┴───────────┴───────────┴──────────┴──────────┴─────────────────────┘"
    );
    println!("\nIf the target is true D65 at moderate Y, the cal index with the smallest");
    println!("Δu'v' is the best match for this display's spectral response.");

    Ok(())
}

fn matrices_equal(a: &[[f64; 6]; 3], b: &[[f64; 6]; 3]) -> bool {
    for i in 0..3 {
        for j in 0..6 {
            if (a[i][j] - b[i][j]).abs() > 1e-9 {
                return false;
            }
        }
    }
    true
}

fn duv_from_d65(x: f64, y: f64) -> f64 {
    const D65: (f64, f64) = (0.3127, 0.3290);
    let (up, vp) = xy_to_uv_prime((x, y));
    let (d65_up, d65_vp) = xy_to_uv_prime(D65);
    ((up - d65_up).powi(2) + (vp - d65_vp).powi(2)).sqrt()
}

fn xy_to_uv_prime(xy: (f64, f64)) -> (f64, f64) {
    let (x, y) = xy;
    let denom = -2.0 * x + 12.0 * y + 3.0;
    if denom.abs() < 1e-9 {
        return (0.0, 0.0);
    }
    (4.0 * x / denom, 9.0 * y / denom)
}
