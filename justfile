fmt:
    cargo fmt
    uvx ruff check --fix
check:
    cargo clippy
    uvx ruff check
build:
    uvx maturin develop
