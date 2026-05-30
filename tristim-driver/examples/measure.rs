//! Take one XYZ measurement. Hold the colorimeter against any patch on screen
//! and watch the numbers.

use tristim_driver::{Colorimeter, Spyder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut device = Spyder::open_any()?;
    println!(
        "Spyder {} (HW {}.{:02}, SN {})",
        if device.is_spyder_2024() {
            "2024"
        } else {
            "X2"
        },
        device.info().firmware.0,
        device.info().firmware.1,
        device.info().serial,
    );

    println!("Taking one measurement (cal index 0)... hold the puck against a bright patch.");
    let (xyz, raw, cal, setup) = device.measure_xyz(0)?;

    println!("\nRaw 6-channel sensor counts: {:?}", raw.0);
    println!("Black-cal (subtracted before matrix): {:?}", setup.s5);
    println!("XYZ: X = {:.4}  Y = {:.4}  Z = {:.4}", xyz.x, xyz.y, xyz.z);
    if let Some((x, y)) = xyz.chromaticity() {
        println!("CIE 1931 xy chromaticity: ({:.4}, {:.4})", x, y);
    }
    println!("(Y is luminance — typical SDR display white ≈ 80–350 cd/m², HDR can hit 1000+)");

    println!("\nCalibration matrix (3x6, XYZ rows × 6 sensor channels):");
    for i in 0..3 {
        let label = ["X", "Y", "Z"][i];
        let row: Vec<String> = cal.matrix[i]
            .iter()
            .map(|v| format!("{:+.4e}", v))
            .collect();
        println!("  {} = [{}]", label, row.join(", "));
    }
    println!("Gain:   {:?}", cal.gain);
    println!("Offset: {:?}", cal.offset);

    Ok(())
}
