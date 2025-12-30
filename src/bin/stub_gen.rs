use pyo3_stub_gen::Result;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().filter_or("RUST_LOG", "info")).init();
    let mut stub = discord_ext_songbird_native::stub_info()?;

    stub.modules.remove("discord");
    stub.modules.remove("discord.ext");
    stub.modules.remove("discord.ext.songbird");
    stub.generate()?;
    Ok(())
}
