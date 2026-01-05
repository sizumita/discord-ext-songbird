# Suggested commands
- `uv sync --all-extras --dev --no-install-project` (install dev deps)
- `uvx maturin develop` (build/install extension into venv)
- `just build` (stub gen + build wheels)
- `just fmt` (cargo fmt + ruff format)
- `just check` (cargo clippy + ruff check + uvx ty check)
- `python examples/basic.py` (run example; set `DISCORD_BOT_TOKEN`)
- Useful local commands: `rg` for fast search, `ls`, `git status` as needed.