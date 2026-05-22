//! Analyze one or more sweep CSVs produced by `tristim sweep`.
//!
//! Computes per-panel summary metrics (peak Y, white point + Δuv from D65,
//! gamma fit, primary chromaticities, gamut area) and a cross-panel
//! comparison when multiple files are given.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

/// CIE 1931 chromaticity of D65 (sRGB reference white).
const D65_X: f64 = 0.3127;
const D65_Y: f64 = 0.3290;

/// One row of a sweep CSV — matches the writer in `cmd_sweep`.
/// (X and Z fields aren't used in the current analyses, but kept for
/// parity with the CSV columns so future metrics can use them.)
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct Row {
    name: String,
    r_in: f64,
    g_in: f64,
    b_in: f64,
    big_x: f64,
    big_y: f64,
    big_z: f64,
    x: f64,
    y: f64,
}

/// All useful numbers about one panel sweep.
#[derive(Debug, Clone)]
struct PanelMetrics {
    path: PathBuf,
    peak_y: f64,
    black_y: f64,
    white_xy: (f64, f64),
    /// Distance from D65 in u'v' (CIE 1976 UCS). Roughly perceptually uniform —
    /// >0.005 is visible, >0.015 is obvious, >0.030 is severe.
    white_delta_uv: f64,
    cct_k: Option<f64>,
    gamma: f64,
    /// Goodness-of-fit of the gamma model. 1.0 = perfect.
    gamma_r2: f64,
    red:   PrimaryStats,
    green: PrimaryStats,
    blue:  PrimaryStats,
    /// sRGB triangle area in xy is ~0.1121; ratio > 1 means wider gamut.
    gamut_area_ratio: f64,
    /// Y(R+G+B) compared to Y(white). 1.0 = perfectly additive.
    additivity: f64,
}

#[derive(Debug, Clone)]
struct PrimaryStats {
    xy: (f64, f64),
    y: f64,
}

const SRGB_TRIANGLE_AREA: f64 = 0.1121; // computed from sRGB primaries below

pub fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("USAGE: tristim analyze FILE.csv [FILE.csv ...]");
        eprintln!();
        eprintln!("  One file:  detailed per-panel report");
        eprintln!("  Many:      side-by-side table + cross-panel white-point deltas");
        return Ok(());
    }

    let mut all_metrics = Vec::new();
    for arg in args {
        let path = PathBuf::from(arg);
        let rows = read_sweep(&path)?;
        let m = compute_metrics(&path, &rows)?;
        all_metrics.push(m);
    }

    if all_metrics.len() == 1 {
        print_detailed(&all_metrics[0]);
    } else {
        print_summary_table(&all_metrics);
        println!();
        print_white_point_matrix(&all_metrics);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// CSV reading
// ---------------------------------------------------------------------------

fn read_sweep(path: &Path) -> Result<Vec<Row>, Box<dyn Error>> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut rows = Vec::new();
    let mut lines = text.lines();
    let header = lines.next().ok_or("empty file")?;
    // Quick sanity check on header.
    if !header.starts_with("name,") || !header.contains(",X,Y,Z,") {
        return Err(format!("unrecognized header in {}: {header}", path.display()).into());
    }
    for (lineno, line) in lines.enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() < 15 {
            return Err(format!(
                "{}: line {} has only {} columns",
                path.display(),
                lineno + 2,
                parts.len()
            )
            .into());
        }
        rows.push(Row {
            name: parts[0].to_string(),
            r_in: parts[1].parse()?,
            g_in: parts[2].parse()?,
            b_in: parts[3].parse()?,
            // raw0..raw5 at parts[4..10] — we don't need them for analysis
            big_x: parts[10].parse()?,
            big_y: parts[11].parse()?,
            big_z: parts[12].parse()?,
            x: parts[13].parse()?,
            y: parts[14].parse()?,
        });
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Metric computation
// ---------------------------------------------------------------------------

fn compute_metrics(path: &Path, rows: &[Row]) -> Result<PanelMetrics, Box<dyn Error>> {
    let lookup = |name: &str| -> Option<&Row> { rows.iter().find(|r| r.name == name) };

    let white = lookup("grey_1000").ok_or("missing grey_1000 patch")?;
    let black = lookup("grey_000").ok_or("missing grey_000 patch")?;
    let red   = lookup("red_1000").ok_or("missing red_1000 patch")?;
    let green = lookup("grn_1000").ok_or("missing grn_1000 patch")?;
    let blue  = lookup("blu_1000").ok_or("missing blu_1000 patch")?;

    let peak_y = white.big_y;
    let black_y = black.big_y;
    let white_xy = (white.x, white.y);
    let white_delta_uv = delta_uv_from_d65(white_xy);
    let cct_k = cct_mccamy(white_xy);

    let (gamma, gamma_r2) = fit_gamma(rows, peak_y);

    let prim = |row: &Row| PrimaryStats {
        xy: (row.x, row.y),
        y: row.big_y,
    };
    let red_p = prim(red);
    let green_p = prim(green);
    let blue_p = prim(blue);

    let gamut_area = triangle_area_xy(red_p.xy, green_p.xy, blue_p.xy);
    let gamut_area_ratio = gamut_area / SRGB_TRIANGLE_AREA;

    let additivity = (red_p.y + green_p.y + blue_p.y) / peak_y;

    Ok(PanelMetrics {
        path: path.to_path_buf(),
        peak_y,
        black_y,
        white_xy,
        white_delta_uv,
        cct_k,
        gamma,
        gamma_r2,
        red: red_p,
        green: green_p,
        blue: blue_p,
        gamut_area_ratio,
        additivity,
    })
}

/// Linear least-squares fit of `log(Y_rel) = γ · log(V)` over greyscale
/// patches with V ∈ [0.15, 0.95] (excluding very dark, where measurement
/// noise dominates, and the extreme top where panels often clip).
///
/// Returns `(γ, R²)`.
fn fit_gamma(rows: &[Row], peak_y: f64) -> (f64, f64) {
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for row in rows {
        // Only grayscale patches.
        if !row.name.starts_with("grey_") {
            continue;
        }
        if row.r_in != row.g_in || row.g_in != row.b_in {
            continue;
        }
        let v = row.r_in;
        if !(0.15..=0.95).contains(&v) {
            continue;
        }
        if row.big_y <= 0.0 || peak_y <= 0.0 {
            continue;
        }
        let y_rel = row.big_y / peak_y;
        if y_rel <= 0.0 {
            continue;
        }
        xs.push(v.ln());
        ys.push(y_rel.ln());
    }
    if xs.len() < 3 {
        return (f64::NAN, f64::NAN);
    }
    // Slope through origin? No, fit y = γx + c with both.
    let n = xs.len() as f64;
    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for i in 0..xs.len() {
        let dx = xs[i] - mean_x;
        sxx += dx * dx;
        sxy += dx * (ys[i] - mean_y);
    }
    let gamma = sxy / sxx;
    let intercept = mean_y - gamma * mean_x;
    // R²
    let mut ss_res = 0.0;
    let mut ss_tot = 0.0;
    for i in 0..xs.len() {
        let pred = gamma * xs[i] + intercept;
        ss_res += (ys[i] - pred).powi(2);
        ss_tot += (ys[i] - mean_y).powi(2);
    }
    let r2 = if ss_tot > 0.0 { 1.0 - ss_res / ss_tot } else { f64::NAN };
    (gamma, r2)
}

/// Triangle area in the CIE 1931 xy plane.
fn triangle_area_xy(p1: (f64, f64), p2: (f64, f64), p3: (f64, f64)) -> f64 {
    0.5 * (p1.0 * (p2.1 - p3.1) + p2.0 * (p3.1 - p1.1) + p3.0 * (p1.1 - p2.1)).abs()
}

/// xy → (u', v') in CIE 1976 UCS, which is roughly perceptually uniform
/// (Δu'v' distances are comparable across the diagram).
fn xy_to_uv_prime(xy: (f64, f64)) -> (f64, f64) {
    let (x, y) = xy;
    let denom = -2.0 * x + 12.0 * y + 3.0;
    if denom.abs() < 1e-9 {
        return (f64::NAN, f64::NAN);
    }
    let u = 4.0 * x / denom;
    let v = 9.0 * y / denom;
    (u, v)
}

fn delta_uv_from_d65(white_xy: (f64, f64)) -> f64 {
    let (u, v) = xy_to_uv_prime(white_xy);
    let (ud, vd) = xy_to_uv_prime((D65_X, D65_Y));
    ((u - ud).powi(2) + (v - vd).powi(2)).sqrt()
}

/// McCamy's CCT approximation. Valid roughly within 3000–25000 K and only
/// near the Planckian locus — returns None when the white point is far off
/// the locus (large Δuv) because the answer would be meaningless.
fn cct_mccamy(white_xy: (f64, f64)) -> Option<f64> {
    let (x, y) = white_xy;
    let denom = 0.1858 - y;
    if denom.abs() < 1e-6 {
        return None;
    }
    let n = (x - 0.3320) / denom;
    let cct = 437.0 * n.powi(3) + 3601.0 * n.powi(2) + 6831.0 * n + 5517.0;
    if !cct.is_finite() || cct < 1000.0 || cct > 50000.0 {
        return None;
    }
    Some(cct)
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

fn print_detailed(m: &PanelMetrics) {
    let name = m.path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    println!("{}", name);
    println!();
    println!("  Peak white Y      {:7.2} cd/m²", m.peak_y);
    println!("  Black Y           {:7.3} cd/m²", m.black_y);
    let contrast = if m.black_y > 1e-6 { m.peak_y / m.black_y } else { f64::INFINITY };
    println!("  Static contrast   {:>6.0}:1", contrast);
    println!(
        "  White point       xy = ({:.4}, {:.4})",
        m.white_xy.0, m.white_xy.1
    );
    println!(
        "                    Δuv from D65 = {:.4}   {}",
        m.white_delta_uv,
        delta_uv_descriptor(m.white_delta_uv)
    );
    match m.cct_k {
        Some(cct) => println!("                    CCT ≈ {:.0} K (rough — McCamy)", cct),
        None => println!("                    CCT not meaningful (too far off Planckian locus)"),
    }
    println!(
        "  Gamma (fit V=0.15..0.95)   γ = {:.3}    R² = {:.4}   {}",
        m.gamma,
        m.gamma_r2,
        gamma_descriptor(m.gamma)
    );
    println!();
    println!("  Primaries (xy + Y at full intensity):");
    print_primary("R", &m.red);
    print_primary("G", &m.green);
    print_primary("B", &m.blue);
    println!(
        "  Gamut triangle area  {:.4}    (sRGB = {:.4}, ratio: {:.2})",
        m.gamut_area_ratio * SRGB_TRIANGLE_AREA,
        SRGB_TRIANGLE_AREA,
        m.gamut_area_ratio
    );
    println!(
        "  Channel additivity   Y(R+G+B)/Y(white) = {:.3}   {}",
        m.additivity,
        if (m.additivity - 1.0).abs() < 0.03 {
            "(good)"
        } else if (m.additivity - 1.0).abs() < 0.06 {
            "(ok)"
        } else {
            "(suspect — nonlinearity or stray light)"
        }
    );
}

fn print_primary(label: &str, p: &PrimaryStats) {
    println!(
        "    {}   ({:.4}, {:.4})    Y = {:6.2}",
        label, p.xy.0, p.xy.1, p.y
    );
}

fn print_summary_table(metrics: &[PanelMetrics]) {
    println!(
        "{:38} {:>8} {:>8} {:>7} {:>17} {:>8} {:>6} {:>5}",
        "file", "peak Y", "black Y", "contr", "white xy", "Δuv D65", "γ", "gamut"
    );
    println!("{}", "-".repeat(106));
    for m in metrics {
        let name = m.path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let contrast = if m.black_y > 1e-6 { m.peak_y / m.black_y } else { 0.0 };
        println!(
            "{:38} {:>8.2} {:>8.3} {:>5.0}:1 ({:.3}, {:.3}) {:>8.4} {:>6.2} {:>5.2}",
            name,
            m.peak_y,
            m.black_y,
            contrast,
            m.white_xy.0,
            m.white_xy.1,
            m.white_delta_uv,
            m.gamma,
            m.gamut_area_ratio,
        );
    }
}

fn print_white_point_matrix(metrics: &[PanelMetrics]) {
    println!("Cross-panel white-point distance (Δu'v' in CIE 1976 UCS):");
    println!("  > 0.005 perceptible side-by-side");
    println!("  > 0.015 obviously different");
    println!("  > 0.030 dramatically mismatched");
    println!();
    let shorten = |m: &PanelMetrics| -> String {
        m.path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    let names: Vec<String> = metrics.iter().map(shorten).collect();
    let max_name = names.iter().map(|s| s.len()).max().unwrap_or(0).max(10);

    print!("{:width$} ", "", width = max_name);
    for name in &names {
        print!("{:>10} ", abbrev(name, 10));
    }
    println!();

    for (i, mi) in metrics.iter().enumerate() {
        print!("{:width$} ", names[i], width = max_name);
        let (ui, vi) = xy_to_uv_prime(mi.white_xy);
        for mj in metrics {
            let (uj, vj) = xy_to_uv_prime(mj.white_xy);
            let duv = ((ui - uj).powi(2) + (vi - vj).powi(2)).sqrt();
            if duv < 1e-6 {
                print!("{:>10} ", "—");
            } else {
                print!("{:>10.4} ", duv);
            }
        }
        println!();
    }
}

fn abbrev(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}

fn delta_uv_descriptor(duv: f64) -> &'static str {
    if duv < 0.003 {
        "(on-target)"
    } else if duv < 0.008 {
        "(close to D65)"
    } else if duv < 0.015 {
        "(visibly off D65)"
    } else if duv < 0.030 {
        "(obviously off D65)"
    } else {
        "(dramatically off D65)"
    }
}

fn gamma_descriptor(g: f64) -> &'static str {
    if (g - 2.2).abs() < 0.05 {
        "(sRGB / BT.709 nominal)"
    } else if g > 2.2 && g < 2.5 {
        "(steeper than sRGB — mids darker than ideal)"
    } else if g < 2.2 && g > 1.8 {
        "(flatter than sRGB — mids brighter than ideal)"
    } else {
        "(unusual)"
    }
}
