fmt:
    cargo fmt
    uv run --no-sync ruff format
    uv run --no-sync ruff check --fix
check:
    PYO3_PYTHON="$PWD/.venv/bin/python" cargo clippy
    uv run --no-sync ruff check
    uv run --no-sync ty check
build:
    PYO3_PYTHON="$PWD/.venv/bin/python" cargo run --bin stub_gen
    PYO3_PYTHON="$PWD/.venv/bin/python" uv run --no-sync maturin build
dev:
    PYO3_PYTHON="$PWD/.venv/bin/python" cargo run --bin stub_gen
    PYO3_PYTHON="$PWD/.venv/bin/python" uv run --no-sync maturin develop
