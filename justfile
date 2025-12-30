fmt:
    cargo fmt
    uvx ruff format
check:
    cargo clippy
    uvx ruff check py-src
build:
    cargo run --bin stub_gen
    uvx maturin develop
