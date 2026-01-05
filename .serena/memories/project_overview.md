# Project overview
- Purpose: High-performance voice backend for discord.py powered by Songbird, implemented in Rust with a Python API via PyO3/maturin.
- Tech stack: Rust (core backend), Python (discord.py integration), PyO3 + maturin for bindings/build, tokio/songbird for voice.
- Structure:
  - `src/`: Rust backend, voice connection/receive logic; stub generation in `src/bin/stub_gen.rs`.
  - `py-src/discord/ext/songbird/`: Python API layer, `.pyi` stubs, `py.typed`, native stubs in `native/`.
  - `examples/`: runnable bot examples.
  - Build/config: `Cargo.toml`, `pyproject.toml`, `build.rs`, `justfile`.
- Artifacts: Build outputs in `target/` (do not commit).