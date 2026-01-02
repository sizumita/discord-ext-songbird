# Receive (Voice Input)

This document describes the **current** voice receive API in `discord-ext-songbird`.
The public Python surface is re-exported from `discord.ext.songbird.receive`, which is backed by the
native module `discord.ext.songbird.native.receive`.

## Overview

- Voice receive is done by registering a **sink** with `SongbirdClient.listen()`.
- The only sink available from Python today is `BufferSink`.
- Incoming audio is buffered as **VoiceTick** snapshots and consumed with `async for`.

## Quick Start

```python
import asyncio
import discord
from discord.ext import songbird
from discord.ext.songbird import receive

# ... obtain a voice channel and connect
vc = await channel.connect(cls=songbird.SongbirdClient)

sink = receive.BufferSink(max_in_seconds=5)
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
  - `max_in_seconds` caps the queue length.
  - Implementation detail: the limit is treated as a **tick count**, and initial capacity is
    `max_in_seconds * 50`.

### Receive Processing (High-Level)

- `SpeakingStateUpdate` events populate the SSRC → User ID map.
- On each `VoiceTick`:
  - Entries with `decoded_voice` become **speaking** PCM data.
  - Entries without `decoded_voice` are marked **silent** (no PCM stored).
  - SSRCs in the `silent` list are also marked **silent**.

## API Surface (Excerpt)

```python
from discord.ext.songbird import receive

sink = receive.BufferSink(max_in_seconds: int | None = None)
sink.stop() -> None

vc.listen(sink) -> None  # SongbirdClient

async for tick in sink: ...
async for pcm in sink[receive.VoiceKey.User(user_id)]: ...
```

## Limitations & Notes

- **Custom Python sinks are not supported**: `SinkBase` is exposed but cannot be subclassed from Python today.
- **Distinguish silent vs missing**: use `is_silent()`; `get()` alone cannot tell them apart.
- **Unbounded buffering**: omit `max_in_seconds` only if you can tolerate growth.
- **pyarrow dependency**: PCM is returned as `pyarrow.Int16Array`.
