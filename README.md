# discord-ext-songbird

[![GitHub License](https://img.shields.io/github/license/sizumita/discord-ext-songbird)](LICENSE)
[![GitHub Release](https://img.shields.io/github/v/release/sizumita/discord-ext-songbird)](https://github.com/sizumita/discord-ext-songbird/releases)
[![PyPI - Version](https://img.shields.io/pypi/v/discord-ext-songbird)](https://pypi.org/project/discord-ext-songbird/)
[![PyPI - Python Version](https://img.shields.io/pypi/pyversions/discord-ext-songbird)](https://pypi.org/project/discord-ext-songbird/)

High-performance voice backend for discord.py, powered by Songbird and written in Rust.

discord-ext-songbird provides a native `VoiceProtocol` implementation for discord.py, exposing
Songbird's Rust audio pipeline through PyO3.

## Highlights

- Drop-in `VoiceProtocol` via `SongbirdClient`
- Low-latency playback backed by Songbird
- Voice receive APIs (`BufferSink`, `StreamSink`)
- Native input types for raw PCM, encoded audio, and streaming
- PyO3/maturin extension with CPython 3.14 and free-threaded CPython 3.14 support
- Beta release series (API may evolve)

## Installation

```bash
pip install discord-ext-songbird
```

```bash
uv add discord-ext-songbird
```

## Quickstart

```python
import pyarrow as pa
import discord
from discord.ext import songbird
from discord.ext.songbird import player

client = discord.Client(intents=discord.Intents.default())

@client.event
async def on_ready():
    channel = client.get_channel(int(CHANNEL_ID))
    if isinstance(channel, discord.VoiceChannel):
        vc = await channel.connect(cls=songbird.SongbirdClient)

        samples = pa.array([0.0, 0.1, 0.0, -0.1], type=pa.float32())
        source = player.input.RawPCMInput(samples, sample_rate=48000, channels=2)
        track = player.Track(source).volume(0.8)
        await vc.play(track)

client.run(DISCORD_BOT_TOKEN)
```

## Features

### Playback

Use `SongbirdClient` as your voice client. Playback is driven by `Track` and `TrackHandle`.

```python
from discord.ext import songbird
from discord.ext.songbird import player

vc = await channel.connect(cls=songbird.SongbirdClient)
track = player.Track(source)
handle = await vc.play(track)
handle.pause()
```

### Inputs

Native input types live under `discord.ext.songbird.player.input`.

- `RawPCMInput`: `pyarrow.Float32Array` PCM input
- `AudioInput`: encoded audio in a `pyarrow.Array`
- `StreamInput`: `asyncio.StreamReader`

`AudioInput` and `StreamInput` no longer take a codec argument. Songbird 0.6
detects encoded stream formats internally, so `SupportedCodec` has been removed
from the Python API.

```python
import asyncio
from discord.ext.songbird import player

buffer = asyncio.StreamReader()
source = player.input.StreamInput(buffer)
track = player.Track(source)
```

### Voice receive

Receive decoded PCM via `BufferSink` or `StreamSink`.

```python
from discord.ext.songbird import receive

sink = receive.BufferSink(max_duration_secs=5)
vc.listen(sink)

async for tick in sink:
    pcm = tick.get(receive.VoiceKey.User(user_id))
    if pcm is not None:
        handle_pcm(pcm)
```

`VoiceTick.get()` returns `pyarrow.Int16Array` (PCM).

## Examples

See `examples/` for runnable bots:

- `examples/basic.py` (PCM playback)
- `examples/send_stream.py` (stream input)
- `examples/receive_stream.py` (voice receive)

Set `DISCORD_BOT_TOKEN` and `CHANNEL_ID` before running the examples.

## Requirements

- Python 3.14+
- `discord.py[voice]`
- `pyarrow`

Published CPython wheels are split by ABI. The release workflow builds regular
`cp314` wheels and free-threaded `cp314t` wheels for the supported platforms, so
a normal Python 3.14 environment installs the regular wheel while Python 3.14t
installs the free-threaded wheel.

## Development

```bash
uv sync --all-extras --dev --no-install-project --python 3.14t
uv run maturin develop
```

The default local environment uses free-threaded CPython 3.14 (`3.14t`).
Release wheels are built separately for the normal and free-threaded ABIs
(`cp314` and `cp314t`). To check the normal 3.14 build locally, point PyO3 at
a GIL-enabled interpreter:

```bash
PYO3_PYTHON=/path/to/python3.14 cargo check
```

```bash
just build
just fmt
just check
```

## Contributing

Issues and pull requests are welcome. Please keep changes focused and avoid committing build artifacts.

## License

MIT. See `LICENSE`.

## Acknowledgements

- Songbird (Rust voice library)
- discord.py (Python Discord API wrapper)
- PyO3 (Rust bindings for Python)
