# Receive (Voice Input)

This document describes the **current** voice receive API in `discord-ext-songbird`.
The public Python surface is re-exported from `discord.ext.songbird.receive`, which is backed by the
native module `discord.ext.songbird.native.receive`.

## Overview

- Voice receive is done by registering a **sink** with `SongbirdClient.listen()`.
- Two sinks are available from Python: `BufferSink` and `StreamSink`.
- Incoming audio is buffered as **VoiceTick** snapshots and consumed with `async for`.

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

async for tick in sink:
    key = receive.VoiceKey.User(user_id)
    pcm = tick.get(key)
    if pcm is not None:
        # pcm is a pyarrow.Int16Array (16-bit PCM)
        handle_pcm(pcm)
    elif tick.is_silent(key):
        # key was present but silent for this tick
        pass
```

## StreamSink (Streaming)

`StreamSink` provides a stream-based API using a broadcast channel. Unlike `BufferSink`,
it is designed for concurrent consumers and supports retaining ticks when no streams are active.

```python
from discord.ext import songbird
from discord.ext.songbird import receive

vc = await channel.connect(cls=songbird.SongbirdClient)
sink = receive.StreamSink(retain=True, retain_secs=15, max_concurrent=50)
vc.listen(sink)

async with sink.stream() as stream:
    async for tick in stream:
        pcm = tick.get(receive.VoiceKey.User(user_id))
        if pcm is not None:
            handle_pcm(pcm)
```

## How It Works (Technical)

At runtime, Songbird emits voice events (e.g., `VoiceTick`, `SpeakingStateUpdate`) from the voice
driver. The receive layer attaches a sink-specific event handler and converts raw tick data into
Python-facing structures.

Key points:

- **Tick cadence**: one tick represents ~20 ms of audio (≈ 50 ticks/sec).
- **Source mapping**: `SpeakingStateUpdate` is used to map SSRC → user ID.
- **Tick conversion**:
  - speaking entries with `decoded_voice` are converted to `pyarrow.Int16Array` PCM
  - entries without `decoded_voice` (or in the silent list) are marked silent
- **Delivery**:
  - `BufferSink` stores `VoiceTick` in a queue and is consumed destructively
  - `StreamSink` pushes `VoiceTick` through a broadcast channel for concurrent consumers

### Data Flow (Simplified)

```
Discord Voice -> Songbird driver
                     |
                     v
      SpeakingStateUpdate / VoiceTick events
                     |
                     v
            Receive Sink Handler
            - SSRC → User ID map
            - Build VoiceTick
                     |
         +-----------+-----------+
         |                       |
         v                       v
     BufferSink              StreamSink
   (VecDeque queue)     (broadcast channel)
         |                       |
         v                       v
   async for tick          async with stream
   async for pcm           async for tick/pcm
```

## Data Model

### VoiceKey

Identifies the source of audio.

- `VoiceKey.User(user_id)` for known user IDs
- `VoiceKey.Unknown(ssrc)` when only SSRC is known

### VoiceTick

A snapshot for a single tick.

- `VoiceTick.get(key)` → `Optional[pyarrow.Int16Array]`
  - Returns PCM only when the key is speaking in this tick.
  - Returns `None` if the key is silent or not present in this tick.
- `VoiceTick.is_silent(key)` → `bool`
  - `True` if the key is marked silent in this tick.
- `VoiceTick.all_keys()` → `set[VoiceKey]`
  - All keys present in this tick (speaking + silent).
- `VoiceTick.speaking_keys()` → `set[VoiceKey]`
  - Keys with PCM data in this tick.
- `VoiceTick.silent_keys()` → `set[VoiceKey]`
  - Keys marked silent in this tick.

## BufferSink Behavior

`BufferSink` accumulates `VoiceTick` entries in an internal queue.

- **Register**: `vc.listen(sink)` subscribes to Songbird `VoiceTick` and `SpeakingStateUpdate` events.
- **Stop**: `sink.stop()` stops further buffering (the sink remains registered).
- **Consume**:
  - `async for tick in sink:` yields `VoiceTick` entries.
  - `async for pcm in sink[VoiceKey.User(user_id)]:` yields `pyarrow.Int16Array | None` for a specific key.
- **Consumption is destructive**: ticks are popped from the queue and cannot be read again.
- **No waiting**: iteration ends when the queue is empty; it does not block for new ticks.
- **Buffer limit**:
  - `max_duration_secs` caps the buffer window in seconds (keyword-only).
  - `drop_oldest` is keyword-only.
  - `drop_oldest` controls whether old ticks are discarded when full.
  - Implementation detail: it is converted to a tick count based on **20 ms per tick** (50 ticks/sec),
    so the initial capacity is `max_duration_secs * 50`.

## StreamSink Behavior

`StreamSink` pushes ticks through a broadcast channel and is consumed via a stream handle.

- **Register**: `vc.listen(sink)` subscribes to `VoiceTick` and `SpeakingStateUpdate` (and disconnect) events.
- **Stream handle**: `async with sink.stream()` acquires a permit for one consumer.
- **Concurrency**: `max_concurrent` limits simultaneous stream handles.
- **Retention**:
  - `retain=False` drops ticks when no streams are active (default).
  - `retain=True` keeps ticks in the broadcast buffer (up to `retain_secs`).
  - Implementation detail: the buffer size is `retain_secs * 50` ticks (20 ms per tick).

### Receive Processing (High-Level)

- `SpeakingStateUpdate` events populate the SSRC → User ID map.
- On each `VoiceTick`:
  - Entries with `decoded_voice` become **speaking** PCM data.
  - Entries without `decoded_voice` are marked **silent** (no PCM stored).
  - SSRCs in the `silent` list are also marked **silent**.

## API Surface (Excerpt)

```python
from discord.ext.songbird import receive

sink = receive.BufferSink(max_duration_secs: int | None = None, drop_oldest: bool = True)
stream_sink = receive.StreamSink(retain: bool = False, retain_secs: int = 15, max_concurrent: int = 50)
sink.stop() -> None

vc.listen(sink) -> None  # SongbirdClient

async for tick in sink: ...
async for pcm in sink[receive.VoiceKey.User(user_id)]: ...
async with stream_sink.stream() as stream: ...
```

## Limitations & Notes

- **Custom Python sinks are not supported**: `SinkBase` is exposed but cannot be subclassed from Python today.
- **Distinguish silent vs missing**: use `is_silent()`; `get()` alone cannot tell them apart.
- **Unbounded buffering**: omit `max_duration_secs` only if you can tolerate growth.
- **pyarrow dependency**: PCM is returned as `pyarrow.Int16Array`.
