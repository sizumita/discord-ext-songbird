set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# pyo3's build script must see the project interpreter, not the system python.
export PYO3_PYTHON := justfile_directory() / ".venv" / "bin" / "python"

stub_dir := "py-src/discord/ext/songbird/native"

# Install the dev dependencies (no build).
sync:
    uv sync --all-extras --dev --no-install-project

# Format Rust and Python.
fmt:
    cargo fmt --all
    uv run --no-sync ruff format
    uv run --no-sync ruff check --fix

# Lints and type checks (what CI runs, minus the tests).
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-features --all-targets -- -D warnings
    cargo clippy --workspace --no-default-features --features codec-minimal --all-targets -- -D warnings
    uv run --no-sync python scripts/check_invariants.py
    uv run --no-sync ruff check
    uv run --no-sync ruff format --diff
    uv run --no-sync ty check

# Regenerate the native stubs from scratch (stub_gen writes only the flat native module).
stubs:
    rm -rf {{stub_dir}}
    cargo run --bin stub_gen

# Fail if the committed stubs differ from freshly generated ones (incl. deleted/untracked files).
stub-drift: stubs
    test -z "$(git status --porcelain --untracked-files=all -- {{stub_dir}})"

# Build release wheels.
build: stubs
    uv run --no-sync maturin build --release

# Install the extension into .venv (editable).
dev: stubs
    uv run --no-sync maturin develop

# Install the extension with native._testing (needed by the real-module pytest run).
dev-test: stubs
    uv run --no-sync maturin develop --features test-hooks

# Rust unit tests (no libpython needed).
test-rust:
    cargo test -p sbcore --all-features
    cargo test -p sbcore --no-default-features --features codec-minimal

# Python tests against the pure-Python fake of the native module (no build needed).
test-fake:
    SONGBIRD_TEST_NATIVE=fake uv run --no-sync pytest

# Python tests against the real module (builds it with test hooks first).
test-py: dev-test
    SONGBIRD_TEST_NATIVE=real uv run --no-sync pytest

# Everything.
test: test-rust test-fake test-py

# Smoke-test the installed package as release.yml does.
smoke:
    uv run --no-sync python scripts/smoke.py
