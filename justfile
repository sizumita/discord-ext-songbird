fmt:
    cargo fmt
    uvx ruff format
check:
    cargo clippy
    uvx ruff check py-src
build:
    uvx maturin develop
