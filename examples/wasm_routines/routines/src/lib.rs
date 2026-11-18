//! Example `extern` functions for `examples/wasm_routines`.

use serde::Serialize;

#[derive(Serialize)]
struct Ladder {
    pitch: f64,
    widths: Vec<f64>,
}

/// Splits `total` into `count` equal slots and tapers the width within each.
fn ladder(count: i64, total: f64) -> Result<Ladder, String> {
    if count <= 0 {
        return Err(format!("count must be positive, found {count}"));
    }
    let pitch = (total / count as f64).floor();
    let widths = (0..count)
        .map(|i| (pitch * (0.3 + 0.4 * i as f64 / count as f64)).floor())
        .collect();
    Ok(Ladder { pitch, widths })
}

argon_guest::export!(ladder);
