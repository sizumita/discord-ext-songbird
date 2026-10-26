# Sending audio

Everything here hangs off a connected `SongbirdClient` (`vc`). Several outputs can be active at
once; songbird mixes them.

| You have | Use |
|---|---|
| AI speech / any realtime PCM (bytes or base64) | `vc.open_playback(fmt)` → `PlaybackStream` |
| An encoded stream arriving over time (mp3/ogg/wav/…) | `vc.open_encoded_stream()` → `EncodedStream`, or `vc.queue.enqueue_stream(reader)` |
| 20 ms Opus packets arriving over time | `vc.open_opus_stream()` → `OpusPacketStream` |
| A file or bytes in memory | `AudioFile`, `RawPcm`, `OpusPackets` with `vc.play()` / `vc.enqueue()` |

## `PlaybackStream`: realtime PCM

```python
from discord.ext.songbird import GPT_LIVE, PlaybackFull

async with vc.open_playback(GPT_LIVE) as speaker:      # PCM16LE 24 kHz mono
    speaker.write_nowait(pcm_bytes)                    # any size; partial frames are carried
    speaker.write_base64_nowait(b64_delta, tag=item_id, at_ms=start_ms)
    await speaker.write(more_pcm)                      # waits only while the buffer is full
    await speaker.drain(timeout=5)                     # flush: wait until everything has played
```

- **Starts paused.** Nothing is sent (no RTP, no speaking indicator) until the first accepted write;
  the track resumes within one 20 ms tick. After `idle_ms` (250) without audio the stream goes idle,
  emits `PlaybackIdle`, and auto-pauses again.
- **Formats.** `write*` take data in the stream's format. `str` is base64. Typed buffers must match
  the encoding (`'h'` arrays for pcm16le, `'f'` for f32le, or raw bytes): writing a float32 numpy
  array to a pcm16le stream raises `InvalidAudio` instead of playing noise.
- **Backpressure.** The buffer holds `max_buffer_ms` (60 s). `write_nowait` raises
  `PlaybackFull(needed_ms, free_ms)` with nothing consumed; `await write()` waits for space.
  In a task that reads a provider socket, always use the `*_nowait` variants.
- **Underrun** plays silence; positions keep counting.

### Provider timestamps: gap filling

Pass the provider's timestamp of each delta (GPT-Live `start_ms`) as `at_ms`. The first timed write
anchors the provider timeline to the wall clock; a later delta that starts after a gap gets that
much silence inserted (up to `max_gap_ms`, default `min(2000, max_buffer_ms // 2)`), so the model's
own pauses are preserved. Late or overlapping deltas are appended immediately — speech is never
trimmed — and a gap longer than `max_gap_ms` is treated as a discontinuity. The anchor is wall-clock
based, so a track that was idle-paused does not accumulate stale silence.

### Barge-in: `clear()`

```python
result = speaker.clear()                 # discard everything queued, now; any thread
for item in result.items:                # open items and items heard in the last output_latency_ms
    truncate(item.tag, item.heard_ms)    # ≤ written_ms by construction
```

- Capacity is released immediately; the cut is audible at the next pull (≤ 40 ms plus network).
- **`reject_late=True`** (default) drops later writes for every open or interrupted tag: late
  deltas of a cancelled OpenAI Realtime item never play.
- **`reject_until_gap_ms=N`** (timed providers without item ids, e.g. GPT-Live): later *timed*
  writes contiguous with the cleared audio are dropped until one starts at least `N` ms after it.
- **`hold_ms`** drops *all* writes for a while — a fallback for untimed writes only.

Items (`tag=`) are open from their first write until `end_item(tag)` (call it on the provider's
"audio done" event), `clear()`, or the end of the turn (idle). `item(tag)` returns an
`ItemProgress(tag, written_ms, played_ms, heard_ms, complete)`; `complete` becomes true once
`end_item` was called and everything written has played.

### Positions

| Property | Meaning |
|---|---|
| `written_ms` | accepted real audio (gap silence and rejected writes excluded) |
| `played_ms` | real audio handed to the Discord encoder |
| `heard_ms` | estimate: `played_ms` minus the not-yet-elapsed part of `output_latency_ms` (40) |
| `position_ms` | playhead including silence |
| `buffered_ms` | queued, including inserted gap silence |

`stats()` adds `underrun_ms`, `gap_filled_ms`, `rejected_ms`, `discarded_ms`, `contended_ticks`,
`clears` and `tags_evicted`. Network and listener jitter buffers (≈ 40–100 ms) lie downstream of
`played_ms` and cannot be observed.

### Other controls

- `prebuffer_ms`: hold the start of each turn until this much is buffered (or the same time has
  passed), smoothing bursty providers. `drain()`, `end_item()` and `aclose(drain=True)` release it.
- `await wait_idle()`: the end of the current AI turn (GPT-Live has no output-done event).
- `pause()`/`resume()`, `volume` (0.0–4.0, 5 ms ramp), `close()`, `await aclose(drain=True)`.
- `when_disconnected="discard"` drops buffered audio while the voice link is down instead of
  playing it after reconnect.
- While the client is self-muted the stream holds its position: nothing is consumed or counted as
  played, and `drain()` keeps waiting.

## Files, PCM and Opus in memory

```python
from discord.ext.songbird import AudioFile, RawPcm, OpusPackets, load_audio_file

song = await load_audio_file("song.mp3")              # read + probe in a worker thread
song = AudioFile.from_path("song.mp3")                # blocking variant (releases the GIL)
clip = AudioFile(wav_bytes)                           # any bytes-like object
pcm = RawPcm(pcm16_bytes, "pcm16_48k_stereo")          # 8/16/24/48 kHz in any stream encoding
pcm = RawPcm.from_samples(float32_array, 44_100, 2)    # any rate 8–192 kHz, resampled once
opus = OpusPackets(packets)                            # 20 ms / 48 kHz packets, passthrough-capable

handle = vc.play(song, volume=0.8, loops=0)            # synchronous; mixed with everything else
end = await handle.wait_ended()                        # TrackEnd(reason, error)
```

Sources are immutable and reusable: each play gets its own cursor over the same memory. The
container is detected from content (`supported_codecs()` lists what this build can decode); there is
no `hint` parameter. At most 32 memory/file tracks play at once per connection.

### `TrackHandle`

`pause()`, `resume()`, `stop()`, `set_volume(v)`, `set_loops(n | None)`, `await seek(seconds)`,
`await info()`, `await wait_ended()`, plus `id`, `uuid`, `kind`, `ended`, `end_reason`. Handles
compare equal when they refer to the same track. `wait_ended()` resolves on every termination path:
natural end (`"finished"`), `stop()` / `stop_all()` (`"stopped"`), a decode error (`"error"`), or the
connection closing (`"disconnected"`).

songbird processes track commands only while its mixer is live (something is playing); `seek()`
and `info()` raise `TimeoutError` while it is parked.

## The queue

```python
q = vc.queue
first = vc.enqueue(AudioFile.from_path("a.mp3"))       # == q.enqueue(...)
q.enqueue(AudioFile.from_path("b.ogg"), volume=0.5, loops=1, paused=False)
q.enqueue_stream(response_body_iterator)                # fetched and decoded only once current

q.current        # the playing entry (TrackHandle) or None
len(q), q[0]     # pending entries only: index 0 is next up
q.skip(); q.pause(); q.resume(); q.clear(); q.stop(); q.dequeue(i)
```

Only the current entry is handed to songbird, so a long queue costs no mixer time. Pending entries
are fully controllable (`pause`, `set_volume`, `set_loops` apply when they start; `stop()` removes
them). Advancing is not gapless: expect 20–60 ms between entries. At most 10 000 entries.

## Push streams

```python
async with vc.open_encoded_stream() as stream:   # decoded on a dedicated thread, not the mixer
    await stream.feed_from(http_response.content) # an asyncio.StreamReader or async iterable of bytes

async with vc.open_opus_stream(max_packets=250) as opus:
    await opus.send(packet)                       # 20 ms packets; underrun sends Opus silence
```

Both start paused and resume on the first audio; an Opus stream pauses again after 250 ms without
packets, so an idle stream sends no RTP.

## Limits

16 live sources (playbacks + encoded + Opus streams) and 32 playing memory tracks per connection;
16 encoded-stream decode threads per process. Every size is checked before anything is allocated
(`LimitExceeded`).

Note that an open live source keeps songbird's mixer live even while auto-paused: open a playback
per AI session, not per bot.
