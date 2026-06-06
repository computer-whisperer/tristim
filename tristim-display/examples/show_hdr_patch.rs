//! Show an HDR (PQ + BT.2020) patch at a target luminance, then exit.
//!
//! Declares a parametric `wp_color_management_v1` description on an fp16
//! surface and writes the PQ code value for `--nits`. Fails up front if the
//! compositor rejects the description (or has no color management at all).
//!
//! Usage:
//!   cargo run -p tristim-display --example show_hdr_patch -- --output DP-4 --nits 100 --secs 5
//!   cargo run -p tristim-display --example show_hdr_patch -- --output DP-4 --nits 400 --window 0.04

use std::time::Duration;
use tristim_display::{
    BufferFormat, DescriptionRequest, Mastering, ParametricDescription, PatchSurface, pq,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let output = arg_value(&args, "--output").unwrap_or_else(|| "DP-1".to_string());
    let nits: f64 = arg_value(&args, "--nits")
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100.0);
    let secs: u64 = arg_value(&args, "--secs")
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let window: Option<f64> = arg_value(&args, "--window")
        .as_deref()
        .and_then(|s| s.parse().ok());

    let peak = nits.max(400.0);
    let mut params = ParametricDescription::named("st2084_pq", "bt2020");
    params.mastering = Some(Mastering {
        luminance_nits: Some((0.0005, peak)),
        max_cll_nits: Some(peak),
        max_fall_nits: Some(peak / 2.0),
        ..Default::default()
    });
    let desc = DescriptionRequest::parametric(params);

    let code = pq::nits_to_pq(nits);
    println!(
        "opening PQ/BT.2020 patch on '{}': {} cd/m² (PQ code {:.4}) for {}s",
        output, nits, code, secs
    );

    let mut patch = PatchSurface::open(&output, BufferFormat::Xbgr16161616f, Some(desc))?;
    if let Some(fraction) = window {
        // Centered bright window on black, e.g. 0.04 for a 4%-APL peak patch.
        patch.set_window_fraction(fraction)?;
    }
    patch.set_code_values([code, code, code])?;
    std::thread::sleep(Duration::from_secs(secs));
    drop(patch);
    Ok(())
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).cloned()
}
