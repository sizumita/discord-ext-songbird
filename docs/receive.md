# Receive (Voice Input)

This document describes the voice receive API in `discord-ext-songbird`.
The public Python surface is re-exported from `discord.ext.songbird.receive`,
which is backed by the native module `discord.ext.songbird.native.receive`.

## Overview

- Voice receive is done by registering a sink with `SongbirdClient.listen()`.
- Two sinks are available from Python: `BufferSink` and `StreamSink`.
- Incoming audio is exposed as Arrow `RecordBatch` snapshots and consumed with `async for`.
- Per-key convenience iterators are still available when only one source is needed.

## Quick Start

```python
import asyncio
import discord
from discord.ext import songbird
from discord.ext.songbird import receive

# ... obtain a voice channel and connect
vc = await channel.connect(cls=songbird.SongbirdClient)

sink = receive.BufferSink(max_duration_secs=5)
vc.listen(sink)  # non-async

# collect some audio, then stop
await asyncio.sleep(5)
sink.stop()

async for batch in sink:
    # batch is a pyarrow.RecordBatch
    # columns: key_kind, key_id, speaking, pcm
    handle_batch(batch)
```

For single-source consumers:

```python
key = receive.VoiceKey.User(user_id)
async for pcm in sink[key]:
    if pcm is not None:
        # pcm is a pyarrow.Int16Array slice backed by the tick's shared PCM buffer
        handle_pcm(pcm)
```

## StreamSink (Streaming)

`StreamSink` provides a stream-based API using a broadcast channel. It supports
concurrent consumers via permits and can optionally retain recent ticks when no
streams are active.

```python
from discord.ext import songbird
from discord.ext.songbird import receive

vc = await channel.connect(cls=songbird.SongbirdClient)
sink = receive.StreamSink(retain=True, retain_secs=15, max_concurrent=50)
vc.listen(sink)

async with sink.stream() as stream:
    async for batch in stream:
        handle_batch(batch)
```

## Arrow Batch Schema

Each receive tick is represented as one Arrow `RecordBatch`:

| column | type | meaning |
| --- | --- | --- |
| `key_kind` | `uint8` | `0` for user IDs, `1` for unknown SSRCs |
| `key_id` | `uint64` | Discord user ID or SSRC value |
| `speaking` | `bool` | `True` when this row has PCM for the tick |
| `pcm` | `list<int16>` | Interleaved 16-bit PCM samples; silent rows use an empty list |

This keeps all PCM for a tick in a single Arrow values buffer. The per-key
helpers return zero-copy slices from that shared buffer where possible.

## How It Works (Technical)

At runtime, Songbird emits voice events from the voice driver. The receive layer
maintains a connection-level SSRC to user ID map and converts raw tick data into
a compact columnar Arrow batch.

Key points:

- One tick represents about 20 ms of audio, or roughly 50 ticks/sec.
- `SpeakingStateUpdate` maps SSRC values to Discord user IDs at the voice
  connection level, so later sinks can reuse mappings observed before `listen()`.
- Entries with `decoded_voice` append their PCM into the shared `pcm` values buffer.
- Entries without `decoded_voice`, and SSRCs in the silent set, become silent rows.
- `BufferSink` stores batches in a queue and is consumed destructively.
- `StreamSink` broadcasts `Arc`-shared batches for concurrent consumers.

### Data Flow

```text
Discord Voice -> Songbird driver
                     |
                     v
      SpeakingStateUpdate / ClientDisconnect
                     |
                     v
        Connection VoiceIdentityMap
                     |
                     v
             VoiceTick events
                     |
                     v
            Receive Sink Handler
            - Build VoiceTickBatch
            - Canonical Arrow RecordBatch
                     |
         +-----------+-----------+
         |                       |
         v                       v
     BufferSink              StreamSink
   (VecDeque queue)     (broadcast channel)
         |                       |
         v                       v
   async for batch        async with stream
   async for pcm          async for batch/pcm
```

## BufferSink Behavior

`BufferSink` accumulates Arrow `RecordBatch` entries in an internal queue.

- `vc.listen(sink)` subscribes to Songbird receive events.
- `sink.stop()` stops further buffering; it does not unregister the sink.
- `async for batch in sink:` yields `pyarrow.RecordBatch` snapshots.
- `async for pcm in sink[VoiceKey.User(user_id)]:` yields `pyarrow.Int16Array | None`.
- Consumption is destructive: entries are popped from the queue and cannot be read again.
- Iteration ends when the queue is empty; it does not wait for new ticks.
- `max_duration_secs` caps the buffer window and must be greater than zero when set.
- `drop_oldest` controls whether old ticks are discarded when full.

## StreamSink Behavior

`StreamSink` pushes Arrow `RecordBatch` entries through a broadcast channel.

- `async with sink.stream()` acquires a permit for one consumer.
- `max_concurrent` limits simultaneous stream handles and must be greater than zero.
- `retain_secs` controls broadcast buffer capacity and must be greater than zero.
- `retain=False` drops ticks when no streams are active.
- `retain=True` keeps ticks in the broadcast buffer up to `retain_secs`.

## API Surface (Excerpt)

```python
from discord.ext.songbird import receive

sink = receive.BufferSink(max_duration_secs: int | None = None, drop_oldest: bool = True)
stream_sink = receive.StreamSink(retain: bool = False, retain_secs: int = 15, max_concurrent: int = 50)
sink.stop() -> None

vc.listen(sink) -> None  # SongbirdClient

async for batch in sink: ...
async for pcm in sink[receive.VoiceKey.User(user_id)]: ...
async with stream_sink.stream() as stream:
    async for batch in stream: ...
```

## Limitations & Notes

- Custom Python sinks are not supported; `SinkBase` is exposed but cannot be subclassed from Python today.
- `key_kind` values are stable: `0` is user ID, `1` is unknown SSRC.
- `VoiceKey.Unknown(ssrc)` is used only while Songbird has not yet observed a
  `SpeakingStateUpdate` with a user ID for that SSRC. Discord voice state data
  does not expose SSRCs, so the receive layer does not guess from channel
  membership.
- Silent and missing keys are distinct in batch form: silent keys have rows with `speaking=False`; missing keys have no row.
- Omit `max_duration_secs` only if you can tolerate unbounded buffering.
- PCM is returned through Arrow and requires the package's Arrow dependencies.
