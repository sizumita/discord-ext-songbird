# Repository Guidelines

This repository hosts a hybrid Rust/Python, general-purpose voice backend for `discord.py` (audio in and
out of voice channels), built with PyO3 and `maturin`. It stays provider-neutral: no service-specific
APIs, presets, defaults, examples or tests. The one exception is `examples/lfm2_audio/`, model-specific
example bots outside the library (`docs/deviations/lfm2-audio-examples.md`).
The design contract is `docs/design.md`; deviations are recorded in `docs/deviations/*.md`.

## Project Structure & Module Organization
- `crates/sbcore/`: the pyo3-free core crate (all audio, queue, connection and songbird-facing logic:
  `conn/`, `rx/`, `tx/`, `tracks/`, `dsp/`, …). It must never depend on pyo3. Test hooks live in
  `crates/sbcore/src/testing/` (feature `test-hooks`).
- `src/`: the binding crate (`discord_ext_songbird_native`, a cdylib + rlib): pyclasses and conversions
  only (`src/py/*.rs`, `src/errors.rs`, `src/convert.rs`, `src/logging.rs`). Pyclass fields hold only
  `Arc<sbcore::…>`, songbird handles or `Copy` data. Stub generation lives in `src/bin/stub_gen.rs`.
- `py-src/discord/ext/songbird/`: the Python API layer (`client.py`, `_pump.py`, `_log.py`,
  `config.py`, `formats.py`, `streams.py`, `playback.py`, `tracks.py`, `errors.py`) and the
  `py.typed` marker.
- `py-src/discord/ext/songbird/native/`: **auto-generated** by `cargo run --bin stub_gen` (via
  `just stubs`/`just dev`); do not edit manually.
- `tests/`: pytest suite (`tests/conftest.py`, `tests/fakes/native_fake.py` — a pure-Python fake of
  the native module).
- `examples/`: runnable bots (`echo.py`, `websocket_bridge.py`, `record.py`, `music.py`), plus
  `lfm2_audio/` (`stt.py`, `tts.py`, `voice_chat.py` on LFM2.5-Audio-1.5B-JP; they need `liquid-audio`,
  imported lazily so the modules import and test without it).
- `scripts/`: `check_invariants.py` (structural invariants), `smoke.py` (release smoke test).
- Build metadata: `Cargo.toml` (workspace), `crates/sbcore/Cargo.toml`, `pyproject.toml`, `build.rs`,
  `justfile`. Build outputs go to `target/` and should not be committed.

## Build, Test, and Development Commands
- `uv sync --all-extras --dev --no-install-project`: install dev dependencies.
- `uv run --no-sync maturin develop`: build and install the extension into `.venv` (export
  `PYO3_PYTHON="$PWD/.venv/bin/python"` first; the justfile does this).
- `just dev`: regenerate stubs, then `uv run --no-sync maturin develop`.
- `just dev-test`: the same with `--features test-hooks` (`native._testing`, needed by the real-module tests).
- `just build`: regenerate stubs, then `uv run --no-sync maturin build --release`.
- `just fmt`: `cargo fmt --all` + `ruff format` + `ruff check --fix`.
- `just check`: rustfmt check, `cargo clippy` (all features and `codec-minimal`, `-D warnings`),
  `scripts/check_invariants.py`, `ruff check`, `ruff format --diff`, `ty check`. ty finds the package only
  through the editable install's `.pth`, so `check` first runs `maturin develop` when this checkout is not
  installed yet (an existing build, e.g. with test hooks, is kept).
- Native build requirements: Rust stable plus libopus via pkg-config, or CMake (libopus_sys then builds its
  bundled Opus). The fake test run needs neither.
- `just stub-drift`: regenerate stubs from scratch and fail on any change or untracked file.
- `python examples/record.py`: run a sample bot after setting `DISCORD_BOT_TOKEN` (the WebSocket bridge
  also needs `BRIDGE_URL`; quick starts read `CHANNEL_ID`).

## Coding Style & Naming Conventions
- Rust: `rustfmt` defaults via `cargo fmt`; `clippy` with `-D warnings`. sbcore denies `unwrap`, `expect`,
  `panic`, `todo`, `unimplemented` and slice indexing outside tests; never hold a lock across `.await`.
- Python: 4-space indent, line length 120, double quotes per Ruff config. Use `ruff format` and `ruff check`.
- Naming: snake_case for functions/modules, PascalCase for types/classes (e.g., `SongbirdClient`).
- Docstrings: use NumPy-style sections (`Parameters`, `Returns`, `Notes`, `Examples`) for pyclass/pymethods
  and public Python APIs.
- Never block the asyncio loop thread or the songbird mixer thread; keep every buffer bounded (§10 of the design).

## Testing Guidelines
- Rust: `cargo test -p sbcore --all-features` (no libpython needed; unit tests live next to the code in
  `crates/sbcore/src/**`). Also run `cargo test -p sbcore --no-default-features --features codec-minimal`.
- Python: `pytest` under `tests/` with `test_*.py` naming (pytest-asyncio, `asyncio_mode = "auto"`).
  `SONGBIRD_TEST_NATIVE=fake` runs the suite on `tests/fakes/native_fake.py` without building or
  installing anything (`just test-fake`; `tests/conftest.py` puts this checkout's `py-src` on discord's
  package path); `SONGBIRD_TEST_NATIVE=real` requires the extension built with
  `--features test-hooks` (`just test-py`). Mark tests that need the Rust implementation
  `@pytest.mark.real_only` and fake-only behaviour `@pytest.mark.fake_only`.
- Detached connections (`native._testing.detached_connection`) run without Discord: inject ticks and
  driver events, drive time with `Harness.step(ms)`, and pull playback with `MixerHarness`.
- `scripts/check_invariants.py` must pass (`tests/test_check_invariants.py` tests its rules; extend both
  when adding a rule); `scripts/smoke.py` checks an installed release build.

## Commit & Pull Request Guidelines
Git history shows short, descriptive subjects and often an emoji prefix followed by a colon (e.g., `:sparkles: Refactor ...`). Follow that style when possible, keep commits focused, and avoid generated artifacts. PRs should include a clear summary, steps to verify (commands run), and call out any API or behavior changes; update docs or stubs when APIs move.

## Security & Configuration Tips
Keep tokens out of code and use environment variables (e.g., `DISCORD_BOT_TOKEN`, `CHANNEL_ID`,
`BRIDGE_URL`). Do not commit built artifacts like
`discord_ext_songbird_native*.so` / `native.*.so` or anything in `target/`. Release wheels never
enable `test-hooks`.
