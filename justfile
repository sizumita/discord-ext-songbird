fmt:
    cargo fmt
    uvx ruff format
    uvx ruff check --fix
check:
    cargo clippy
    uvx ruff check
    uvx ty check
build:
    cargo run --bin stub_gen
    uvx maturin build
dev:
    cargo run --bin stub_gen
    uvx maturin develop
