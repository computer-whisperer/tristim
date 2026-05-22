//! Show a solid-color patch on a chosen output for N seconds, then exit.
//!
//! Usage:
//!   cargo run -p tristim-display --example show_patch -- --list
//!   cargo run -p tristim-display --example show_patch -- --output DP-1 --color 1,0.5,0 --secs 5

use tristim_display::{list_outputs, PatchSurface};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--list" || a == "-l") {
        for out in list_outputs()? {
            println!(
                "{:12}  {} {:?}  {}",
                out.name,
                out.make,
                out.size.unwrap_or((-1, -1)),
                out.description
            );
        }
        return Ok(());
    }

    let output = arg_value(&args, "--output").unwrap_or_else(|| "DP-1".to_string());
    let color_str = arg_value(&args, "--color").unwrap_or_else(|| "1,1,1".to_string());
    let secs: u64 = arg_value(&args, "--secs")
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);

    let parts: Vec<f64> = color_str
        .split(',')
        .map(|s| s.trim().parse::<f64>().expect("color components must be numbers"))
        .collect();
    if parts.len() != 3 {
        return Err("--color must be R,G,B with three components in 0..=1".into());
    }
    let rgb = [parts[0], parts[1], parts[2]];

    println!("opening patch on output '{}' with color {:?} for {}s", output, rgb, secs);
    let mut patch = PatchSurface::open(&output)?;
    patch.set_color(rgb)?;
    std::thread::sleep(Duration::from_secs(secs));
    drop(patch);
    Ok(())
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).cloned()
}
