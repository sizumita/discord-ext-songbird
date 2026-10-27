//! Generates `py-src/discord/ext/songbird/native/__init__.pyi` (`cargo run --bin stub_gen`).
//!
//! Only the flat `native` module is written; the pure-Python parent packages are never touched.

use pyo3_stub_gen::Result;

fn main() -> Result<()> {
    let mut stub = discord_ext_songbird_native::stub_info()?;
    stub.modules
        .retain(|name, _| name == "discord.ext.songbird.native");
    stub.generate()?;
    Ok(())
}
