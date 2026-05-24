//! Minimal proof-of-life: open the Spyder, read device info, dump raw
//! bytes for a couple of opcodes.

use std::time::Duration;
use tristim_driver::{Colorimeter, Opcode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut device = Colorimeter::open_any()?;
    println!("opened device, USB PID = 0x{:04x}", device.pid());
    println!(
        "(family: {})",
        if device.is_spyder_2024() {
            "Spyder 2024"
        } else {
            "SpyderX2"
        }
    );

    // 0xC2 — combined info (HW version + serial + extended capabilities)
    println!("\n--- 0xC2 (get device info) ---");
    let raw = device.command(Opcode::GetInfo, &[], 0x25, false, Duration::from_secs(5))?;
    println!("raw 37-byte reply: {}", hex(&raw));
    let info = device.get_info()?;
    println!(
        "hw version:     {}.{:02}",
        info.hw_version.0, info.hw_version.1
    );
    println!("serial:         {:?}", info.serial);
    println!("high-level cmds:{}", info.high_level_commands);
    if let Some(mx) = info.max_display_type {
        println!("max display:    {}", mx);
    }
    if let Some(mask) = info.display_type_mask {
        println!("display mask:   0x{:04x}", mask);
    }

    // 0xF6 — get calibration matrix for native index 0 (108-byte reply, checksummed)
    println!("\n--- 0xF6 (get calibration matrix, index 0) ---");
    match device.command(
        Opcode::GetCalibration,
        &[0],
        0x6C,
        true,
        Duration::from_secs(5),
    ) {
        Ok(raw) => println!("raw 108-byte reply: {}", hex(&raw)),
        Err(e) => println!("0xF6 failed: {e}"),
    }

    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(" ")
}
