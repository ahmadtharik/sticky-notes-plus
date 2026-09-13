slint::include_modules!();
use slint::ComponentHandle;

/// Spawns `n` small note windows (default 20) to measure Slint's
/// per-floated-window footprint — the MS-Sticky-Notes workload shape.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let n: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    // Keep the component handles alive for the whole run — dropping one
    // destroys its window.
    let mut windows = Vec::new();
    for _ in 0..n {
        let ui = SlintProbe::new()?;
        ui.show()?;
        windows.push(ui);
    }
    slint::run_event_loop()?;
    Ok(())
}