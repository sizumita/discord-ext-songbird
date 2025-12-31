fmt:
    cargo fmt
    uvx ruff format
check:
    cargo clippy
    uvx ruff check
    uvx ty check
build:
    cargo run --bin stub_gen
    uvx maturin build
