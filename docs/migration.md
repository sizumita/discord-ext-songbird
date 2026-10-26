# Migrating from 0.5 to 0.6

0.6 is a rewrite. The architecture changed so that a client can never leak (no Python object is
held by Rust), receive and send are bounded, and audio can be streamed in and out with low latency.
Source compatibility with 0.5 was not kept; this page maps every 0.5 API to its replacement.

## Packaging

- Version 0.6.0, still `requires-python >= 3.14` (CPython 3.14 and free-threaded 3.14t).
- `discord.py>=2.7.1,<3` is now a declared runtime dependency.
- `pyarrow`, `arro3-*`, `ty`, `audioop-lts` and `typing-extensions` are no longer installed. Install
  the extras `discord-ext-songbird[arrow]` or `[arro3]` if you use the Arrow helpers.
- The build backend is maturin only; the PyPy classifier is gone.

## The client

| 0.5 | 0.6 |
|---|---|
| `SongbirdImpl` (native base class) | removed: `SongbirdClient` is a pure-Python `discord.VoiceProtocol` composing a native connection |
| overriding `update_hook` | no effect; op 4 is sent by `SongbirdClient` itself |
| `update_server` / `update_state` | internal (`on_voice_server_update` / `on_voice_state_update`) |
| connect timeout → `PyJoinError`, client left registered | `ConnectTimeout` (a `TimeoutError`) or `ConnectFailed`; **any** failure, incl. cancellation, unregisters the client |
| `deafen()` / `mute()` before connect sent op 4 with no channel | the flags travel in the single join op 4 |
| `disconnect(force=...)` ignored `force`; cleanup skipped on errors | honours `force`, always calls `cleanup()`, idempotent |
| `move_to()` returned before the move completed | waits until connected on the new channel; `move_to(None)` disconnects |
| `SongbirdImpl.stop()` | `stop_all()` (also closes live streams) |
| `deafen()`/`mute()`/`is_deaf()`/`is_mute()` | `await set_self_deaf()` / `await set_self_mute()` and the `self_deaf` / `self_mute` properties |
| bot user id taken from `application_id` | `client.user.id` |
| — | new: `reconnect()`, `wait_connected()`, `status`, `stats()`, `VoiceConfig` |

Configuration is passed with a factory:

```python
vc = await channel.connect(cls=SongbirdClient.with_config(VoiceConfig(receive=False)))
vc = await songbird.connect(channel, config=VoiceConfig.low_latency())
```

`set_self_mute()`/`set_self_deaf()` never raise when not connected: a value set before `connect()`
wins over `connect()`'s arguments, and a value set while connecting is sent as soon as the
connection is up. While self-muted, live streams hold their position instead of being consumed.

## Receive

Removed: `listen(sink)`, `SinkBase`, `BufferSink`, `StreamSink`, `Stream`, `VoiceKey`,
`VoiceTick`, `PyAsyncIterator`, and the per-tick Arrow `RecordBatch` (`key_kind`, `key_id`,
`speaking`, `pcm` at 48 kHz stereo).

| 0.5 | 0.6 |
|---|---|
| `vc.listen(BufferSink(...))` + polling | `vc.listen_user(user, fmt)` / `vc.listen_mixed(fmt)` / `vc.listen(fmt, user=)` |
| `StreamSink` per user | `vc.listen_users(fmt)` yields one stream per user |
| `async for tick in stream` (ended when empty) | `async for chunk in stream` — waits while empty; ends only on close/disconnect |
| `speaking_keys()` on a batch | `chunk.speaking`, `chunk.speakers`, and `vc.events()` (`SpeakingStarted`/`Stopped`) |
| `RecordBatch` with `pcm` | `memoryview(chunk)` / `np.frombuffer(chunk, "<i2")`; Arrow: `pyarrow.array(chunk)`, `stream.read_batch()` |
| unbounded buffers, `retain` | bounded `max_queue_ms`, `overflow=` policy, `dropped_ms`, `skip_to_live()` |

Chunks come in the format you ask for (default: Discord's native 48 kHz stereo PCM16, as in 0.5;
or `PCM16_16K`, `"pcm16_24k"`, µ-law, …), are continuous and silence-filled, and only subscribed
users are decoded. Receive uses songbird's `Decrypt` mode
plus our own decoder, so DAVE audio is undistorted and packet-loss concealment is exactly 20 ms. The
jitter buffer defaults to 60 ms (it was 100 ms); `VoiceConfig.low_latency()` gives 40 ms.
`receive=False` disables decryption entirely.

## Inputs

| 0.5 | 0.6 |
|---|---|
| `AudioInput(pyarrow_array)` | `AudioFile.from_arrow(array)` (UInt8, or a one-value Binary array) |
| `AudioInput(bytes)` / a file | `AudioFile(buffer)`, `AudioFile.from_path(path)`, `await load_audio_file(path)` — probed eagerly; no `hint` (symphonia ignores it) |
| `RawPCMInput(float32_array, sample_rate=…, channels=…)` | `RawPcm.from_samples(array_or_buffer, sample_rate, channels)` — any rate 8–192 kHz, resampled once |
| raw PCM at 8/16/24/48 kHz | `RawPcm(buffer, AudioFormat(...))` (any encoding) or `RawPcm.from_arrow()` |
| `StreamInput(reader)` played now | `stream = vc.open_encoded_stream(); await stream.feed_from(reader)` |
| `vc.enqueue(Track(StreamInput(reader)))` | `vc.queue.enqueue_stream(reader)` (fed and decoded only once current) |
| `OpusPacketInput(frames)` | `OpusPackets(packets)` / `OpusPackets.from_arrow()` |
| `OpusPacketStreamInput(max_packets)` + `send`/`close` | `vc.open_opus_stream(max_packets=)` → `send`/`send_nowait`/`aclose` (underrun sends silence; never blocks the mixer) |
| `InputBase`, the `Compose` protocol | removed |
| — | new: `vc.open_playback(fmt)` → `PlaybackStream` for realtime PCM written as it arrives |

## Tracks

| 0.5 | 0.6 |
|---|---|
| `Track(source).volume(v).loops(n)` builder | `vc.play(source, volume=v, loops=n, paused=False)` / `vc.enqueue(...)` |
| `await vc.play(track)` returning an eager `Future` | `vc.play(...)` is **synchronous** and returns a `TrackHandle` |
| `TrackHandle.play()` | `resume()` |
| `seek(timedelta)` | `await seek(seconds)` |
| `enable_loop()` / `disable_loop()` / `loop_for(n)` | `set_loops(None)` / `set_loops(0)` / `set_loops(n)` |
| — | new: `wait_ended()`, `info()`, `ended`, `end_reason`, `set_volume`, `id`, `uuid`, equality |

At most 32 tracks play at once per connection (`LimitExceeded`).

## Queue

`Queue` became `vc.queue: TrackQueue`. Queued tracks are no longer parked inside songbird, and the
queue holds at most 10 000 entries.

| 0.5 `Queue` | 0.6 `TrackQueue` |
|---|---|
| `current()` | `current` (property) |
| `tracks()` (current first) | `[q.current, *q]` — `len()`, iteration and indexing cover pending entries only (index 0 = next up) |
| `__getitem__` returned `None` past the end | raises `IndexError` (iteration terminates) |
| `__len__` (current + pending) | pending entries only |
| `is_empty()` | `q.current is None and not q` |
| `stop()` | `stop()` (same: clear, then skip) |
| `dequeue(i)` | `dequeue(i)` (pending index; the entry ends with `"stopped"`) |
| `skip()`, `pause()`, `resume()` | unchanged |
| `Track(...).volume(v).pause()` before enqueueing | `enqueue(src, volume=v, paused=True, loops=…)`; pending entries also accept `set_volume`/`set_loops`/`pause`/`resume` |

Advancing is not gapless: expect 20–60 ms between entries.

## Errors

| 0.5 | 0.6 |
|---|---|
| `PySongbirdError` | `SongbirdError` |
| `PyJoinError` | `ConnectFailed` (timeouts: `ConnectTimeout`, also a `TimeoutError`) |
| `PyControlError` | `TrackError` |
| `PyPlayerError` | removed |

The old names remain importable for one release and emit a `DeprecationWarning`. New:
`ConnectTimeout`, `NotConnected`, `ConnectionClosed`, `StreamClosed`, `StreamOverflow`,
`PlaybackFull`, `ReceiveDisabled`, `InvalidAudio`, `LimitExceeded`, `SourceError`.

## Module layout and logging

- `discord.ext.songbird.native` is a single flat, internal module; the submodules `model`, `player`,
  `receive` and `error` are gone. Import everything from `discord.ext.songbird`.
- The hand-written top-level `__init__.pyi` was removed: the Python layer is typed directly.
- Logger names changed from `discord_ext_songbird_native.*` to `discord.ext.songbird.*`
  (`discord.ext.songbird.native.*` for this library's Rust code, `discord.ext.songbird.songbird.*`
  for songbird). Use `songbird.set_log_levels()` for Rust-side filtering.

## Examples

`basic.py`, `send_stream.py` and `receive_stream.py` were replaced by `echo.py` (receive +
playback streaming), `websocket_bridge.py` (a generic raw-PCM WebSocket bridge), `record.py` and
`music.py`. Each one guards `bot.run(...)` with `if __name__ == "__main__":`, and the bridge takes
its `url` as an argument so it can be tested against a local server.
