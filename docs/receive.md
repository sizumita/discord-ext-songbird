# Receiving audio

Receive streams are **continuous**: one chunk per `chunk_ms` of real time from subscription until
close, whether or not anyone speaks. Silence is synthesised for quiet users, before the first packet,
during reconnects and when Discord stops sending audio — so a realtime model's uplink stays paced
like a microphone.

```python
from discord.ext.songbird import GPT_LIVE

async with vc.listen_mixed(GPT_LIVE, chunk_ms=40) as mic:   # PCM16LE 24 kHz mono, 40 ms chunks
    mic.skip_to_live()                                       # start fresh (e.g. after a handshake)
    async for chunk in mic:
        await ws.send(chunk.to_base64())                     # base64 encoded in Rust
```

## Streams

| Method | Yields |
|---|---|
| `vc.listen_user(user, fmt)` | one user's audio; silence while they are quiet or absent |
| `vc.listen_mixed(fmt, include=, exclude=, ignore_bots=True, policy="mix")` | the channel mixed |
| `vc.listen(fmt, user=None)` | `listen_user` if `user` is given, else `listen_mixed` |
| `vc.listen_users(fmt)` | a router yielding one `AudioStream` per user as they first speak |

Common options: `chunk_ms` (10–1000, default 20), `max_queue_ms` (default 2000) and `overflow`.

- **Formats**: 8/16/24/48 kHz, mono or stereo, `pcm16le`, `f32le`, `mulaw`/`alaw` (8 kHz mono).
  Presets: `GPT_LIVE`, `OPENAI_REALTIME` (24 kHz), `GEMINI_LIVE_INPUT` (16 kHz), `PCM16_48K`, …
  Each user's Opus is decoded once and resampled once per format, however many streams use it.
- **Mixing** sums the selected users and runs a limiter (−1 dBFS); it never divides by the number
  of speakers, so the level does not jump when someone joins. `policy="floor"` lets the first
  speaker hold the floor until silent for `floor_release_ms` — for single-caller models.
  `set_include()` / `set_exclude()` change a mixed stream's selection at the next chunk boundary.
- **`ignore_bots`** uses the member cache; a bot's first few hundred ms may be mixed before its
  classification arrives.
- **Users** can be ints or anything with an `id` (`Member`, `User`, `Object`).

### Reading

```python
chunk = await stream.read(timeout=1.0)    # or: async for chunk in stream
chunk = stream.read_nowait()              # any thread; None if nothing is queued
ms = stream.skip_to_live()                # drop the backlog; the next chunk has start_ms == 0
```

Iteration ends after `close()` or when the connection closes, once queued chunks are drained.
Cancelling a pending read never loses a chunk.

### Overflow

Each stream is bounded by `max_queue_ms` (and every stream of a connection together by
`VoiceConfig.max_receive_mb`). When a consumer falls behind:

| `overflow=` | Behaviour |
|---|---|
| `"drop_oldest"` (default) | evict the oldest chunk |
| `"drop_newest"` | discard the incoming chunk |
| `"error"` | close the stream: the queued chunks are still delivered, then `StreamOverflow` is raised once (also by `async for`) |

A chunk's `dropped_ms` is the audio dropped immediately before it, so a bridge can send
compensating silence and keep the provider's timeline aligned.

## `AudioChunk`

A chunk exposes its payload through the **buffer protocol** without copying:

```python
memoryview(chunk)                      # format 'h' (pcm16le), 'f' (f32le) or 'B' (G.711), read-only
np.frombuffer(chunk, "<i2")            # zero-copy
wave_file.writeframes(chunk)
bytes(chunk); chunk.to_bytes(); chunk.to_base64()
```

| Attribute | Meaning |
|---|---|
| `seq`, `start_ms`, `end_ms` | the stream's own clock (0 = first chunk; `seq` jumps over dropped chunks) |
| `timeline_ms` | the connection's receive timeline (shared by every stream and event) |
| `captured_at` | `time.monotonic()` of the tick that produced the first sample |
| `speaking`, `speakers`, `voiced_ms` | voiced audio in this chunk and who contributed |
| `gap_ms` | synthetic silence (no Discord audio flowed: link down, reconnect, stall) |
| `concealed_ms` | packet-loss concealment |
| `dropped_ms` | audio dropped by overflow immediately before this chunk |
| `rms_dbfs`, `peak` | levels |
| `format`, `frames`, `duration_ms`, `user_id` | shape and source (`user_id` is `None` when mixed) |

`timeline_ms − start_ms` is the stream's `origin_timeline_ms`. A GPT-Live bridge that appends every
chunk can map transcript `start_ms` values back to `chunk.speakers`.

### Arrow (optional)

With `pyarrow` (or `arro3-core`) installed:

```python
arr = pyarrow.array(chunk)             # zero-copy Int16/Float32/UInt8 array over the chunk
batch = stream.read_batch(50)          # up to 50 chunks as one record batch (user_id, seq, …, pcm)
table = batch.to_pyarrow()
```

## Events

```python
from discord.ext.songbird import VoiceEvent

async with vc.events(kinds=[VoiceEvent.SpeakingStarted, VoiceEvent.UserLeft]) as events:
    async for ev in events:
        match ev:
            case VoiceEvent.SpeakingStarted(user_id=uid, timeline_ms=t):
                ...
            case VoiceEvent.UserLeft(user_id=uid):
                ...
```

| Event | Fields |
|---|---|
| `Connected`, `Reconnected` | `channel_id`, `ssrc`, `timeline_ms` / `gap_ms` |
| `Disconnected` | `reason`, `code`, `will_retry` — `will_retry=False` is always the last event |
| `UserMapped`, `UserLeft` | `user_id` (+ `ssrc`) — "mapped" means first transmitted |
| `SpeakingStarted`, `SpeakingStopped` | `user_id`, `timeline_ms`, `captured_at` / `duration_ms` |
| `PlaybackStarted`, `PlaybackIdle`, `PlaybackEnded` | `playback_id` (+ `played_ms` / `reason`) |
| `TrackEnded` | `track_id`, `reason` |
| `ReceiveStalled` | `since_ms` |
| `Lagged` | `dropped`: events dropped because this stream fell behind (bounded, drop-oldest) |

Speaking is detected from packet activity (Opus silence frames do not count) with a hangover of
`speaking_hangover_ms`; set `VoiceConfig.voice_threshold_dbfs` to also require a minimum level.
Joins are not observable through the voice connection; use discord.py's `on_voice_state_update`.

## Recording

`examples/record.py` writes one timeline-aligned WAV per user:

```python
async with vc.listen_users(PCM16_48K, chunk_ms=200, max_queue_ms=10_000) as users:
    async for stream in users:          # one stream per user, as they first speak
        tg.create_task(record_user(stream, out, origin=vc.timeline_ms))
```

Each per-user stream starts at its user's first speech; pad the file with
`chunk.timeline_ms − origin` of silence so every file starts at the same instant.

## Latency

Typical one-way latency to a chunk: songbird's jitter buffer (`playout_buffer_ms`, 60 ms; 40 ms with
`VoiceConfig.low_latency()`) + up to one tick + ≤ 4.3 ms of resampling + (`chunk_ms` − 20 ms) +
about 0.2 ms to wake the event loop.

## Receive off

`VoiceConfig(receive=False)` turns decryption and decoding off entirely (`listen_*` raise
`ReceiveDisabled`) — the cheapest setting for music bots.
