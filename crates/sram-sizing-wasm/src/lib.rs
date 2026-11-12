//! The SRAM sizer as a WebAssembly module, which `examples/sram` calls as
//! `sizing::sram`. Build it with
//! `cargo build --release -p argon-sram-sizing-wasm --target wasm32-unknown-unknown`.

use argon_sram_sizing::{SramLayoutParams, SramParams, unsupported_periphery};

/// Sizes an SRAM with the given organization for the Argon layout.
fn sram(
    wmask_granularity: i32,
    mux_ratio: i32,
    num_words: i32,
    data_width: i32,
) -> Result<SramLayoutParams, String> {
    let sized = SramParams::new(wmask_granularity, mux_ratio, num_words, data_width)
        .map_err(|error| error.to_string())?
        .size();
    if sized.periphery.is_none() {
        return Err(unsupported_periphery(mux_ratio));
    }
    Ok(sized)
}

argon_guest::export!(sram);
