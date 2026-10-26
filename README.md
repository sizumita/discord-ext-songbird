# discord-ext-songbird

[![GitHub License](https://img.shields.io/github/license/sizumita/discord-ext-songbird)](LICENSE)
[![GitHub Release](https://img.shields.io/github/v/release/sizumita/discord-ext-songbird)](https://github.com/sizumita/discord-ext-songbird/releases)
[![PyPI - Version](https://img.shields.io/pypi/v/discord-ext-songbird)](https://pypi.org/project/discord-ext-songbird/)
[![PyPI - Python Version](https://img.shields.io/pypi/pyversions/discord-ext-songbird)](https://pypi.org/project/discord-ext-songbird/)

A voice backend for discord.py 2.7, powered by [songbird](https://github.com/serenity-rs/songbird)
and written in Rust — built for realtime voice AI (GPT-Live, OpenAI Realtime, Gemini Live), music
and recording bots.

## Highlights

- **`SongbirdClient`**, a `discord.VoiceProtocol`: `await channel.connect(cls=SongbirdClient)`.
- **Receive** continuous, silence-filled, real-time-paced per-user or mixed streams in the format
  your model wants (8–48 kHz, mono/stereo, PCM16 / f32 / µ-law / A-law, 10–1000 ms chunks), with
  speaking events, timestamps and drop accounting. Chunks are zero-copy buffers with
  `to_base64()` done in Rust.
- **Send** realtime PCM with backpressure, base64 input, wall-clock gap filling from provider
  timestamps, and barge-in (`clear()`) that is silent within 40 ms and reports what was heard for
  truncation.
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

    # …and echo the channel back, 20 ms at a time.
    async with vc.listen_mixed(songbird.GPT_LIVE) as mic, vc.open_playback(songbird.GPT_LIVE) as out:
        async for chunk in mic:
            if chunk.speaking:
                out.write_nowait(chunk)


client.run(os.environ["DISCORD_BOT_TOKEN"])
```

A complete GPT-Live bridge is about 20 lines of user code; see [`examples/gpt_live.py`](examples/gpt_live.py).

## Documentation

- [Overview: connecting, configuration, asyncio rules, logging, errors](docs/index.md)
- [Sending audio](docs/send.md) — `PlaybackStream`, tracks, the queue, sources
- [Receiving audio](docs/receive.md) — streams, chunks, events, recording
- [Migrating from 0.5](docs/migration.md)

## Examples

| File | What it shows |
|---|---|
| [`examples/gpt_live.py`](examples/gpt_live.py) | GPT-Live bridge: floor-policy uplink, gap-filled playback, local barge-in |
| [`examples/openai_realtime.py`](examples/openai_realtime.py) | OpenAI Realtime bridge: item tags, `clear()` + exact truncation |
| [`examples/gemini_live.py`](examples/gemini_live.py) | Gemini Live bridge: 16 kHz in / 24 kHz out, `interrupted`, `goAway` reconnects |
| [`examples/record.py`](examples/record.py) | One timeline-aligned WAV per user plus a speaking-event log |
| [`examples/music.py`](examples/music.py) | Music bot on the track queue, including queued HTTP streams |

Set `DISCORD_BOT_TOKEN` (and the provider's API key) and run e.g. `python examples/record.py`.

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
