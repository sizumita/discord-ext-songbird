# discord-ext-songbird

A voice backend for [discord.py](https://github.com/Rapptz/discord.py) 2.7 built on
[songbird](https://github.com/serenity-rs/songbird) (Rust). It is designed for realtime voice AI —
GPT-Live, OpenAI Realtime, Gemini Live — as much as for music and recording bots:

- **receive** continuous, silence-filled, real-time-paced per-user or mixed streams in the format a
  model wants (8/16/24/48 kHz, mono/stereo, PCM16 / f32 / µ-law / A-law, 10–1000 ms chunks);
- **send** realtime PCM with backpressure, base64 input, wall-clock gap filling from provider
  timestamps, and barge-in (`clear()`) that is silent within 40 ms and reports what was heard;
- play files, raw PCM and Opus packets, with a sequential queue;
- bounded memory everywhere, no Python object held by Rust, and no Rust thread that ever enters the
  interpreter — identical behaviour on CPython 3.14 and free-threaded 3.14t.

Guides:

- [Sending audio](send.md): playback streams, tracks, the queue, sources.
- [Receiving audio](receive.md): streams, chunks, events, recording.
- [Migrating from 0.5](migration.md).

## Installation

```bash
pip install discord-ext-songbird          # or: uv add discord-ext-songbird
pip install "discord-ext-songbird[arrow]" # optional: pyarrow for the Arrow helpers
```

Wheels are published for CPython 3.14 and 3.14t (Linux glibc/musl x86_64/aarch64, macOS arm64,
Windows x64/x86). `discord.py>=2.7.1,<3` is installed as a dependency.

## Connecting

`SongbirdClient` is a `discord.VoiceProtocol`; pass it to `connect`:

```python
from discord.ext import songbird

vc = await channel.connect(cls=songbird.SongbirdClient)
# or with settings:
vc = await songbird.connect(channel, config=songbird.VoiceConfig.low_latency())
vc = await channel.connect(cls=songbird.SongbirdClient.with_config(receive=False))
```

- `connect()` sends **one** voice-state update (op 4) carrying `self_mute`/`self_deaf` and waits for
  the media connection. A timeout raises `ConnectTimeout` (a `TimeoutError`); a terminal failure
  raises `ConnectFailed`. On *any* failure — including cancellation — the connection is torn down,
  a best-effort leave is sent and the client is unregistered, so retrying never hits "Already
  connected".
- `disconnect()` always cleans up, is idempotent and never raises for gateway errors.
- `move_to(channel)` waits until the move completes; `reconnect()` forces a fresh voice session
  (the recovery for stalled receive or DAVE; `VoiceConfig.auto_recover` does it automatically).
- `await set_self_mute(True)` stops sending audio; live streams hold their position while muted.
  Flags set while not connected are sent with one op 4 as soon as the connection is up.
- `status` (`ConnectionStatus`), `is_connected()`, `wait_connected()`, `ssrc`, `timeline_ms` and
  `stats()` describe the connection.

Streams, playbacks, events and tracks can be created in every state except `"closed"` (so a bridge
can be wired up while the connection is still being established). Before `connect()` they raise
`NotConnected`; after the connection closed they raise `ConnectionClosed`.

### `VoiceConfig`

| Field | Default | Range / meaning |
|---|---|---|
| `receive` | `True` | `False` disables receive decoding entirely; `listen_*` raise `ReceiveDisabled` |
| `playout_buffer_ms` | 60 | songbird jitter buffer, 20–200, multiple of 20 (`low_latency()`: 40) |
| `playout_spike_ms` | 60 | 0–200, multiple of 20 |
| `decode_state_timeout_s` | 60.0 | idle SSRC decoders are pruned after this |
| `speaking_hangover_ms` | 200 | silence before `SpeakingStopped` |
| `voice_threshold_dbfs` | `None` | `None`: speech = packet activity; else an RMS gate (−100–0) |
| `max_decoded_users` | 64 | 1–256 concurrently decoded speakers |
| `max_receive_mb` | 64 | 1–1024: the sum of every receive queue's bound |
| `stall_timeout_ms` | 2000 | receive watchdog, 500–30 000 |
| `mix_mode` | `"stereo"` | `"mono"` halves the Opus encoding cost (`low_latency()`) |
| `bitrate` | `None` | 6000–510 000 bps; `None` = songbird default (128 kbps) |
| `driver_timeout_s`, `driver_retries` | 10.0, 5 | per transport attempt; retries 1–10, never infinite |
| `rejoin_attempts` | 3 | gateway-level rejoins with `reconnect=True`, 0–10 |
| `auto_recover` | `True` | rejoin after a ≥ 5 s receive stall |

Invalid values raise `LimitExceeded` (a `ValueError`) when the config is constructed.

## A realtime voice bridge in 20 lines

```python
import asyncio, json
from websockets.asyncio.client import connect as ws_connect
from discord.ext import songbird
from discord.ext.songbird import GPT_LIVE, PlaybackFull

async def bridge(vc: songbird.SongbirdClient, url: str, headers: dict[str, str]) -> None:
    async with (
        ws_connect(url, additional_headers=headers) as ws,
        vc.listen_mixed(GPT_LIVE, chunk_ms=40, policy="floor") as mic,
        vc.open_playback(GPT_LIVE) as speaker,
    ):
        await ws.send(json.dumps({"type": "session.start", "session": {"audio": {"format": GPT_LIVE.openai_format}}}))
        mic.skip_to_live()

        async def uplink() -> None:
            async for chunk in mic:  # one chunk every 40 ms, silence included
                await ws.send(json.dumps({"type": "session.input_audio.append", "audio": chunk.to_base64()}))

        async with asyncio.TaskGroup() as tg:
            up = tg.create_task(uplink())
            async for raw in ws:
                ev = json.loads(raw)
                if ev["type"] == "session.output_audio.delta":
                    try:
                        speaker.write_base64_nowait(ev["delta"], at_ms=ev.get("start_ms"))
                    except PlaybackFull:
                        pass  # overrun: drop rather than block the reader
            up.cancel()
```

Complete, tested bridges live in `examples/` (`gpt_live.py`, `openai_realtime.py`,
`gemini_live.py`); every bridge function takes `url=` and `api_key=`.

## asyncio rules

1. Every stream, playback and event stream is an async context manager. Every `close()` is
   synchronous, idempotent and safe from any thread; dropping an unclosed object closes it.
2. Awaiting is bound to the event loop that ran `connect()`; awaiting from another loop raises
   `RuntimeError`. The synchronous methods (`read_nowait`, `write_nowait`, `write_base64_nowait`,
   `clear`, `close`, properties) work from any thread.
3. In the task that reads a provider WebSocket, use `write_*_nowait` and treat `PlaybackFull` as an
   overrun; never `await write()` there, or backpressure delays handling of interruptions.
4. Call `skip_to_live()` right after starting a provider session and again after a provider
   reconnect: Discord streams stay open while the AI session reconnects.
5. Cancelling a pending `__anext__` or `write()` never loses or duplicates audio.

## How it works

Rust never calls Python. Every native method is synchronous; readiness is signalled by a one-byte
"doorbell" on a socket that one asyncio task per connection awaits, and log records are pulled by a
daemon thread (`songbird-log`). All Rust state lives in a pyo3-free core crate, so nothing songbird,
tokio or the mixer can reach holds a Python object — dropping a client without `disconnect()` still
tears everything down. See `docs/design.md` for the full design.

## Logging

Rust records are forwarded to `logging`: this library under `discord.ext.songbird.native.*`,
songbird under `discord.ext.songbird.songbird.*`, the Python layer under `discord.ext.songbird`.
Filtering happens in Rust before a record is formatted:

```python
songbird.set_log_levels(library="DEBUG", songbird="INFO")
```

Tokens are never logged; session ids are logged as lengths and endpoints as hosts.

## Errors

| Exception | Raised when |
|---|---|
| `NotConnected` | a connection method is used before `connect()` |
| `ConnectFailed(reason, code)` / `ConnectTimeout(timeout)` | connecting failed / timed out (`ConnectTimeout` is a `TimeoutError`) |
| `ConnectionClosed(reason)` | a connection method is used after close |
| `StreamClosed(reason)` / `StreamOverflow` | reading or writing a closed stream / receive overflow under `overflow="error"` (raised once) |
| `PlaybackFull(needed_ms, free_ms)` | a `*_nowait` write found no space (nothing consumed) |
| `ReceiveDisabled` | `listen_*` with `VoiceConfig(receive=False)` |
| `InvalidAudio` | bad format, base64, buffer type or Opus packet (a `ValueError`) |
| `LimitExceeded(what, value, minimum, maximum)` | a value is out of range or a cap was reached (a `ValueError`) |
| `SourceError` | unrecognised encoded audio |
| `TrackError` | a track operation failed |

All derive from `SongbirdError`. Background failures (a track that failed to decode, a lost
connection) are reported as state — `TrackEnded`, `Disconnected`, `close_reason` — never as
exceptions in unrelated tasks.

## Runtime

Two internal tokio runtimes run songbird's tasks and the receive DSP. Size them before the first
connection with `songbird.configure_runtime(net_workers=2, rx_workers=1)` (1–8 each).
`supported_codecs()` lists the codecs and containers compiled into the installed build.
