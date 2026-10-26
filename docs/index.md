# discord-ext-songbird

A general-purpose voice backend for [discord.py](https://github.com/Rapptz/discord.py) 2.7 built
on [songbird](https://github.com/serenity-rs/songbird) (Rust): audio in and out of Discord voice
channels with low latency, for music bots, recorders, voice tools and bridges to other audio
systems:

- **receive** continuous, silence-filled, real-time-paced per-user or mixed streams in the format
  you need (8/16/24/48 kHz, mono/stereo, PCM16 / f32 / µ-law / A-law, 10–1000 ms chunks);
- **send** realtime PCM with backpressure, wall-clock gap filling from source timestamps, and
  interruption (`clear()`) that is silent within 40 ms and reports how much was played;
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
- `disconnect()` always cleans up, is idempotent and never raises for gateway errors. Once it has
  started, nothing the library does in the background (a rejoin, a flag flush) sends another join.
- When an established connection ends terminally (`lost`, `rejoin_failed`, `session_invalid`, …)
  a best-effort leave is sent too, so the bot does not stay listed in the channel.
- `move_to(channel)` waits until the move completes; `reconnect()` forces a fresh voice session
  (the recovery for stalled receive or DAVE; `VoiceConfig.auto_recover` does it automatically).
- `await set_self_mute(True)` stops sending audio; live streams hold their position while muted.
  Flags set while not connected are sent with one op 4 as soon as the connection is up.
- `status` (`ConnectionStatus`), `is_connected()`, `wait_connected()`, `ssrc`, `timeline_ms` and
  `stats()` describe the connection.

Streams, playbacks, events and tracks can be created in every state except `"closed"` (so a pipeline
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

## Audio formats

Every receive stream and playback has one `AudioFormat` (sample rate, channels, encoding). The
default of every receive stream and playback is **`PCM16_48K_STEREO`, Discord's native format**
(48 kHz, 2 channels, 16-bit little-endian PCM): no resampling or channel conversion happens in
either direction. `AudioFormat(sample_rate, channels, encoding="pcm16le")` has no default rate or
channel count. Pass
any other format (an `AudioFormat`, a preset such as `PCM16_16K`, or a string such as
`"pcm16_24k"`) and the library converts in Rust: receive streams are resampled, down- or
up-mixed and re-encoded before they reach Python, and playback input is converted to 48 kHz stereo
on the mixer side.

```python
async with vc.listen_mixed() as mic:                         # 48 kHz stereo PCM16
    ...
async with vc.listen_user(member, songbird.PCM16_16K, chunk_ms=100) as mic:  # 16 kHz mono, 100 ms chunks
    ...
```

## Streaming to external services

Speech recognisers, realtime speech services and other audio systems usually take a stream of raw
audio chunks and send audio back. The same building blocks cover all of them; nothing in the
library is specific to one service:

1. **Choose the format and chunk size the service expects** when you open the streams: 16 or
   24 kHz mono PCM16 is common for speech services, with chunks of 20–100 ms.
   `listen_mixed(..., policy="floor")` gives a dominant-speaker mix for consumers that expect one
   speaker at a time; `listen_user` follows a single member.
2. **Forward `bytes(chunk)`** for every chunk (or hand the chunk itself to anything that accepts a
   buffer). Streams are continuous and
   real-time paced, so the service receives silence while nobody speaks. Call `skip_to_live()`
   once the service is ready (and again after it reconnects) to drop what queued up meanwhile.
   If the protocol wants text-encoded audio, encode it yourself: `AudioChunk` supports the buffer
   protocol, so `base64.b64encode(chunk)` works without an extra copy.
3. **Write the returned audio to a `PlaybackStream`** opened in the service's output format, with
   `write_nowait()` from the task that reads the connection (treat `PlaybackFull` as an overrun).
   If the service timestamps its audio, pass `at_ms=` so gaps are filled with silence.
4. **Call `clear()` on interruption** (for example when the service reports that the user started
   talking): queued audio is discarded and playback is silent within 40 ms.
5. **Use tags and `segment()` to know how much was played.** Write each segment of returned audio
   with `tag=<segment id>`, call `end_segment(tag)` when it is fully written, and read
   `segment(tag).played_ms` / `heard_ms` (or the `segments` of the `ClearResult` returned by
   `clear()`) to tell the service how much of a segment the listeners actually heard. Use a new
   tag for every segment: a tag interrupted by `clear()` stays rejected (dropped writes show up in
   `stats().rejected_ms`).
6. **Expect either side to end first.** When the voice connection closes, receive streams still
   deliver what was queued and then end, while writes to the playback raise `StreamClosed`; when the
   service closes the socket, sends raise its library's closed-connection error. Handle both, and
   stop the other direction when one ends.

```python
import asyncio

import websockets
from websockets.asyncio.client import connect as ws_connect
from discord.ext.songbird import PCM16_16K, PlaybackFull, StreamClosed

async with (
    ws_connect(url) as ws,
    vc.listen_mixed(PCM16_16K, chunk_ms=40, policy="floor") as mic,
    vc.open_playback(PCM16_16K) as speaker,
):
    mic.skip_to_live()

    async def uplink() -> None:
        try:
            async for chunk in mic:  # ends after the voice connection closes
                await ws.send(bytes(chunk))
        except websockets.ConnectionClosed:
            pass  # the service closed the socket

    async def downlink() -> None:
        try:
            async for message in ws:  # ends when the service closes the socket
                if message == "clear":
                    speaker.clear()
                elif isinstance(message, bytes):
                    try:
                        speaker.write_nowait(message)
                    except PlaybackFull:
                        pass  # overrun: drop rather than block the reader
        except (websockets.ConnectionClosed, StreamClosed):
            pass  # the socket dropped, or the voice connection closed

    tasks = [asyncio.create_task(uplink()), asyncio.create_task(downlink())]
    await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)  # one side ended: stop both
    for t in tasks:
        t.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
```

[`examples/websocket_bridge.py`](../examples/websocket_bridge.py) is a complete, tested version of
this loop (raw PCM in binary frames, a `"clear"` text frame as the interruption signal).
[`VoiceConfig.low_latency()`](#voiceconfig) trims the receive jitter buffer for interactive use.

## asyncio rules

1. Every stream, playback and event stream is an async context manager. Every `close()` is
   synchronous, idempotent and safe from any thread; dropping an unclosed object closes it.
2. Awaiting is bound to the event loop that ran `connect()`; awaiting from another loop raises
   `RuntimeError` before anything is done, even when no wait would be needed. The synchronous methods (`read_nowait`, `write_nowait`, `clear`, `close`,
   properties) work from any thread.
3. In a task that reads audio from a network connection, use `write_nowait` and treat
   `PlaybackFull` as an overrun; never `await write()` there, or backpressure delays handling of
   interruptions.
4. Call `skip_to_live()` right after a downstream consumer becomes ready and again after it
   reconnects: Discord streams stay open while the consumer reconnects.
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
| `InvalidAudio` | bad format, buffer format or Opus packet, or a bad tag (a `ValueError`); a `str` passed as audio raises `TypeError` |
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
