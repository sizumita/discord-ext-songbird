# discord-ext-songbird

[![GitHub License](https://img.shields.io/github/license/sizumita/discord-ext-songbird)](LICENSE)
[![GitHub Release](https://img.shields.io/github/v/release/sizumita/discord-ext-songbird)](https://github.com/sizumita/discord-ext-songbird/releases)
[![PyPI - Version](https://img.shields.io/pypi/v/discord-ext-songbird)](https://pypi.org/project/discord-ext-songbird/)
[![PyPI - Python Version](https://img.shields.io/pypi/pyversions/discord-ext-songbird)](https://pypi.org/project/discord-ext-songbird/)

A general-purpose voice backend for discord.py 2.7, powered by
[songbird](https://github.com/serenity-rs/songbird) and written in Rust: low-latency audio in and
out of Discord voice channels for music bots, recorders, voice tools and bridges to other audio
systems.

## Highlights

- **`SongbirdClient`**, a `discord.VoiceProtocol`: `await channel.connect(cls=SongbirdClient)`.
- **Receive** continuous, silence-filled, real-time-paced per-user or mixed streams in the format
  you need (8–48 kHz, mono/stereo, PCM16 / f32 / µ-law / A-law, 10–1000 ms chunks; Discord's
  native 48 kHz stereo PCM16 by default), with speaking events, timestamps and drop accounting.
  Chunks are zero-copy buffers (`bytes(chunk)`, `memoryview(chunk)`, NumPy, Arrow).
- **Send** realtime PCM with backpressure, wall-clock gap filling from source timestamps, and
  interruption (`clear()`) that is silent within 40 ms and reports how much of each tagged segment
  was actually played.
- Files, raw PCM and Opus packets with a sequential queue; push-based encoded and Opus streams.
- **Leak-free and bounded by construction**: Rust never holds a Python object, no Rust thread ever
  enters the interpreter, and every buffer has a validated cap.
- CPython 3.14 and free-threaded 3.14t wheels.

## Installation

```bash
pip install discord-ext-songbird
# or
uv add discord-ext-songbird
```

`pyarrow` integration is optional: `pip install "discord-ext-songbird[arrow]"`.

## Quick start

```python
import os

import discord
from discord.ext import songbird

client = discord.Client(intents=discord.Intents.default())


@client.event
async def on_ready() -> None:
    channel = client.get_channel(int(os.environ["CHANNEL_ID"]))
    assert isinstance(channel, discord.VoiceChannel)
    vc = await songbird.connect(channel, config=songbird.VoiceConfig.low_latency())

    # Play a file…
    vc.play(await songbird.load_audio_file("hello.mp3"))

    # …and echo the channel back, 20 ms at a time (48 kHz stereo PCM16 by default).
    async with vc.listen_mixed() as mic, vc.open_playback() as out:
        async for chunk in mic:
            if chunk.speaking:
                out.write_nowait(chunk)


client.run(os.environ["DISCORD_BOT_TOKEN"])
```

Streaming to an external service (a speech recogniser, a realtime speech service, another voice
system) uses the same two calls with the format and chunk size that service expects; see
[Streaming to external services](docs/index.md#streaming-to-external-services) and
[`examples/websocket_bridge.py`](examples/websocket_bridge.py).

## Documentation

- [Overview: connecting, configuration, asyncio rules, logging, errors](docs/index.md)
- [Sending audio](docs/send.md) — `PlaybackStream`, tracks, the queue, sources
- [Receiving audio](docs/receive.md) — streams, chunks, events, recording
- [Migrating from 0.5](docs/migration.md)

## Examples

| File | What it shows |
|---|---|
| [`examples/echo.py`](examples/echo.py) | Receive + playback streaming: a member's voice played back with a delay |
| [`examples/websocket_bridge.py`](examples/websocket_bridge.py) | Generic bridge: raw PCM chunks out as binary WebSocket frames, returned audio played back, `"clear"` interrupts |
| [`examples/record.py`](examples/record.py) | One timeline-aligned WAV per user plus a speaking-event log |
| [`examples/music.py`](examples/music.py) | Music bot on the track queue, including queued HTTP streams |

Set `DISCORD_BOT_TOKEN` (plus `BRIDGE_URL` for the bridge) and run e.g. `python examples/record.py`.

## Development

Requirements: Rust (stable), [uv](https://docs.astral.sh/uv/), [just](https://github.com/casey/just).

```bash
uv sync --all-extras --dev --no-install-project   # dev dependencies
just dev                                         # stubs + uv run --no-sync maturin develop
just test-fake                                   # Python tests on the pure-Python fake (no build)
just test                                        # Rust tests + fake + real-module tests (test hooks)
just fmt && just check                           # format; clippy, invariants, ruff, ty
```

The workspace has two crates: `crates/sbcore` (all logic; no pyo3 dependency, tested with
`cargo test -p sbcore`) and the binding crate at the root (`discord_ext_songbird_native`, built
by maturin into `discord.ext.songbird.native`). The Python layer lives in
`py-src/discord/ext/songbird/`. See [`docs/design.md`](docs/design.md) for the design.

## License

MIT. See [LICENSE](LICENSE).
