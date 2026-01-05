# Task completion checklist
- Run `just fmt` after code changes affecting Rust/Python formatting.
- Run `just check` for lint/type checks (no dedicated test suite).
- Validate behavior with examples in `examples/` when relevant.
- Update stubs/docs when APIs change.
- Avoid committing build artifacts (`target/`, `backend.so`, etc.).