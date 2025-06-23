# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is discord-ext-songbird, a Python library that replaces the voice backend of discord.py with Songbird (a Rust-based voice library). It's a hybrid Python/Rust project using PyO3 for Python bindings.

## Development Commands

### Build and Install
```bash
# Activate virtual environment
source venv/bin/activate

# Build and install the Rust extension locally
maturin develop
```

### Code Quality
```bash
# Run Rust formatter
cargo fmt

# Run Python formatter and linter
ruff format py-src examples
ruff check py-src examples

# Type checking
pyright
```

### Running Examples
```bash
# Set DISCORD_BOT_TOKEN environment variable first
export DISCORD_BOT_TOKEN="your_token_here"

# Run basic example
python examples/basic.py

# Other examples
python examples/custom_config.py
python examples/receive.py
python examples/track_handling.py
```

## Architecture

### Rust Backend (`src/`)
- **lib.rs**: PyO3 module definition, exports all Python-accessible classes
- **client.rs**: Core `SongbirdBackend` implementation managing voice connections
- **source.rs**: Audio source abstractions (AudioSource, SourceComposed)
- **source/raw.rs**: RawBufferSource for raw audio data
- **track.rs**: Track management and playback control
- **queue.rs**: Queue handler for managing audio tracks
- **player.rs**: Player handler for controlling playback
- **config/**: Configuration builders for crypto and decode modes
- **connection.rs**: Voice connection management

### Python Frontend (`py-src/discord/ext/songbird/`)
- **client.py**: `SongbirdClient` - main interface extending discord.VoiceProtocol
- **track.py**: Python wrapper for track objects
- **__init__.py**: Public API exports
- **backend.pyi**: Type stubs for Rust backend

### Key Design Patterns
1. **VoiceProtocol Integration**: SongbirdClient implements discord.py's VoiceProtocol interface
2. **Async Bridge**: Uses pyo3-async-runtimes to bridge Python async/await with Tokio
3. **Builder Pattern**: ConfigBuilder allows customizing voice connection settings
4. **Queue System**: Tracks are enqueued and played sequentially through QueueHandler

## Important Notes

- The project uses maturin for building Python wheels from Rust code
- Requires Python 3.10+ and supports up to Python 3.13
- Voice receiving functionality is still work in progress
- Examples require a valid Discord bot token
- The backend.so file is generated during build and should not be committed
- venv dir is venv
