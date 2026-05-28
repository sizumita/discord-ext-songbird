# Send (Voice Output)

This document describes the voice send API in `discord-ext-songbird`.
The public Python surface is re-exported from `discord.ext.songbird.player`,
which is backed by the native module `discord.ext.songbird.native.player`.

## Overview

- Voice output is done by connecting with `SongbirdClient` and playing `Track` values.
- Tracks are created from native input types such as `RawPCMInput`, `AudioInput`, and `StreamInput`.
- `SongbirdClient.play()` starts a track immediately and returns a `TrackHandle`.
- `SongbirdClient.enqueue()` adds a track to the call queue and returns a `TrackHandle`.
- Playback can be controlled through `TrackHandle` and the call `Queue`.

## Quick Start

```python
import discord
import pyarrow as pa
from discord.ext import songbird
from discord.ext.songbird import player

# ... obtain a voice channel and connect
vc = await channel.connect(cls=songbird.SongbirdClient)

samples = pa.array([0.0, 0.1, 0.0, -0.1], type=pa.float32())
source = player.RawPCMInput(samples, sample_rate=48000, channels=2)
track = player.Track(source).volume(0.8)

handle = await vc.play(track)
handle.pause()
handle.play()
```

For queued playback:

```python
track = player.Track(source).pause()
handle = await vc.enqueue(track)

# The track was queued as paused. Start it when ready.
handle.play()
```

## Input Types

Native input types are exported from `discord.ext.songbird.player`.

| input | source | intended use |
| --- | --- | --- |
| `RawPCMInput` | `pyarrow.Float32Array` | In-memory interleaved float32 PCM |
| `AudioInput` | `pyarrow.Array` | In-memory encoded audio payload |
| `StreamInput` | `asyncio.StreamReader` | Live or long-running encoded streams |
| `OpusPacketInput` | `pyarrow.BinaryArray` or `pyarrow.LargeBinaryArray` | In-memory 20 ms Opus frames |
| `OpusPacketStreamInput` | `await send(packet)` | Live 20 ms Opus packet streams |

`AudioInput` and `StreamInput` do not take a codec argument. Songbird and
Symphonia detect supported encoded formats from the payload or stream.

Use `player.supported_codecs()` to inspect the codecs and formats enabled in
the native build.

## Raw PCM

`RawPCMInput` accepts interleaved float32 PCM samples. The defaults are
48 kHz stereo, which matches Discord voice output.

```python
import pyarrow as pa
from discord.ext.songbird import player

samples = pa.array([0.0, 0.1, 0.0, -0.1], type=pa.float32())
source = player.RawPCMInput(samples, sample_rate=48000, channels=2)
track = player.Track(source)
await vc.play(track)
```

## Encoded Audio Arrays

`AudioInput` accepts an encoded audio payload stored in a primitive Arrow
array. The payload format is detected by Songbird/Symphonia.

```python
import pyarrow as pa
from discord.ext.songbird import player

payload = pa.array(encoded_bytes, type=pa.uint8())
source = player.AudioInput(payload)
await vc.enqueue(player.Track(source))
```

## Stream Input

`StreamInput` wraps an `asyncio.StreamReader`. The player reads from the stream
as the track is consumed, so this is the preferred API for radio streams,
network responses, and other live encoded sources.

```python
import asyncio
from discord.ext.songbird import player

buffer = asyncio.StreamReader()
source = player.StreamInput(buffer)
handle = await vc.enqueue(player.Track(source))

handle.play()
```

Feed bytes with `buffer.feed_data(chunk)` and signal end of input with
`buffer.feed_eof()` when the stream is complete.

## Opus Packet Input

`OpusPacketInput` and `OpusPacketStreamInput` are for pre-encoded Opus. Each
packet must be one non-empty 20 ms Opus frame containing 960 samples at 48 kHz.

For finite packets:

```python
import pyarrow as pa
from discord.ext.songbird import player

frames = pa.array([opus_frame_0, opus_frame_1], type=pa.binary())
source = player.OpusPacketInput(frames)
await vc.play(player.Track(source))
```

For live packets:

```python
from discord.ext.songbird import player

source = player.OpusPacketStreamInput(max_packets=128)
handle = await vc.play(player.Track(source))

await source.send(opus_frame)
await source.close()
```

When an Opus packet input is the only active track and volume is `1.0`,
Songbird can send frames without decoding and re-encoding them. Do not change
volume if you need passthrough behavior.

## Track And Playback Control

`Track` is a builder for playback configuration. Its mutating methods return
the same track object so calls can be chained before playback starts.

```python
track = player.Track(source).volume(0.5).pause()
handle = await vc.enqueue(track)
handle.play()
```

Common control methods:

- `Track.play()` marks the initial track state as playing.
- `Track.pause()` marks the initial track state as paused.
- `Track.stop()` marks the initial track state as stopped.
- `Track.volume(value)` sets the initial volume multiplier.
- `TrackHandle.play()` resumes playback.
- `TrackHandle.pause()` pauses playback.
- `TrackHandle.stop()` stops playback.
- `TrackHandle.seek(position)` seeks when the underlying source supports it.
- `TrackHandle.enable_loop()`, `disable_loop()`, and `loop_for(times)` control looping.

## Queue Behavior

Use `SongbirdClient.queue()` to inspect or control the active call queue.

```python
queue = vc.queue()

current = queue.current()
queued = queue.tracks()

queue.pause()
queue.resume()
queue.skip()
queue.stop()
```

Key points:

- `await vc.play(track)` starts a track immediately through Songbird's call handle.
- `await vc.enqueue(track)` appends a track to the playback queue.
- `vc.stop()` stops playback immediately.
- `queue.current()` returns the current `TrackHandle`, if any.
- `queue.dequeue(index)` removes a queued track by zero-based index.
- `len(queue)` returns the queue length and `queue[index]` returns a handle or `None`.

## Data Flow

```text
Python input source
        |
        v
  player.InputBase
        |
        v
  player.Track
        |
        v
SongbirdClient.play/enqueue
        |
        v
 Songbird call / queue
        |
        v
 Discord voice output
```

## API Surface (Excerpt)

```python
from discord.ext.songbird import player

source = player.RawPCMInput(samples, sample_rate=48000, channels=2)
source = player.AudioInput(payload)
source = player.StreamInput(stream_reader)
source = player.OpusPacketInput(frames)
source = player.OpusPacketStreamInput(max_packets=128)

track = player.Track(source).volume(1.0).play()

handle = await vc.play(track)
handle = await vc.enqueue(track)

vc.stop() -> None
queue = vc.queue()
```

## Limitations & Notes

- Custom Python input subclasses are not supported; use the native input types.
- Live and streaming inputs are consumed as playback reads them; create a fresh input and track for replay.
- `StreamInput` expects `read()` to return bytes-like data; non-bytes values are rejected.
- Call `feed_eof()` on `asyncio.StreamReader` sources when no more data will arrive.
- `OpusPacketStreamInput.close()` signals EOF to the player and prevents further sends.
- Opus packet arrays must not contain nulls.
- Published wheels are built with the full Symphonia codec/format set enabled.
