# Repository Guidelines

This repository hosts a hybrid Rust/Python voice backend for `discord.py`, built with PyO3 and `maturin`.

## Project Structure & Module Organization
- `src/`: Rust backend (PyO3 module, voice connection logic, queue/track handling). Stub generation lives in `src/bin/stub_gen.rs`.
- `py-src/discord/ext/songbird/`: Python API layer, type stubs (`.pyi`), and `py.typed` marker; native stubs live under `native/`.
- `examples/`: runnable bot examples (`basic.py`, `track_handling.py`, `receive.py`).
- Build metadata: `Cargo.toml`, `pyproject.toml`, `build.rs`, `justfile`. Build outputs go to `target/` and should not be committed.

## Build, Test, and Development Commands
- `uv sync --all-extras --dev --no-install-project`: install dev dependencies.
- `uvx maturin develop`: build and install the Rust extension into the active virtualenv.
- `just build`: runs stub generation then builds wheels (`cargo run --bin stub_gen`, `uvx maturin build`).
- `just fmt`: `cargo fmt` + `ruff format` for Python.
- `just check`: `cargo clippy` + `ruff check` + `uvx ty check`.
- `python examples/basic.py`: run a sample bot after setting `DISCORD_BOT_TOKEN`.

## Coding Style & Naming Conventions
- Rust: `rustfmt` defaults via `cargo fmt`; `clippy` for linting.
- Python: 4-space indent, line length 120, double quotes per Ruff config. Use `ruff format` and `ruff check`.
- Naming: snake_case for functions/modules, PascalCase for types/classes (e.g., `SongbirdClient`).
- Docstrings: use NumPy-style sections (`Parameters`, `Returns`, `Notes`, `Examples`) for pyclass/pymethods.

## Tooling Preference (Serena)
When analyzing or editing code, use Serena tools as the default approach. Prefer the JetBrains-backed
symbol tools (e.g., `jet_brains_find_symbol`, `jet_brains_get_symbols_overview`,
`jet_brains_find_referencing_symbols`) whenever possible to navigate and edit code precisely.
Only fall back to plain text search when necessary.

## Testing Guidelines
There is no dedicated automated test suite today. Use `just check` for lint/type checks and validate behavior with `examples/`. If adding tests, place Python tests under `tests/` with `test_*.py` naming, and Rust unit tests in the corresponding `src/` modules.

## Commit & Pull Request Guidelines
Git history shows short, descriptive subjects and often an emoji prefix followed by a colon (e.g., `:sparkles: Refactor ...`). Follow that style when possible, keep commits focused, and avoid generated artifacts. PRs should include a clear summary, steps to verify (commands run), and call out any API or behavior changes; update docs or stubs when APIs move.

## Security & Configuration Tips
Keep tokens out of code and use environment variables (e.g., `DISCORD_BOT_TOKEN`). Do not commit built artifacts like `backend.so` or anything in `target/`.
