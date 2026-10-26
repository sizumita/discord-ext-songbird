# discord-ext-songbird 0.6.0: final design

| | |
|---|---|
| Status | **Accepted.** This document is the implementation contract for branch `redesign/core`. |
| Supersedes | The 0.5.0 architecture (`SongbirdImpl`, sinks, `Track` builder). |
| Basis | Design A ("lifecycle-first"). Python pulls data and Rust holds no Python objects. Grafts from B (pyo3-free core crate, streaming ergonomics), C (wall-clock gap filling, separate receive runtime, connect atomicity, `reconnect()`) and D (`OwnedDriver`, drop-fired completion, `TrackControl`). Every blocking review finding on the candidate designs is resolved, and so is every second-round finding on this document; see §15.3, §15.4 and the "Decision" notes. |
| Verified against | songbird 0.6.1 (incl. `driver/scheduler/{idle,live,task}.rs`, `driver/tasks/{mod,disposal,events}.rs`, `mixer/mod.rs`, `udp_rx/playout_buffer.rs`, `id.rs`, `input/{audiostream,live_input}.rs`), symphonia-core 0.5.5 (`probe.rs`, `formats.rs`, `io/media_source_stream.rs`), smallvec 1.16.4, rubato 0.16.2, pyo3 0.29.3, pyo3-arrow 0.19.0, arrow-buffer 59.3.1, tokio 1.54.1 (sources in `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`) and discord.py `abc.py` / `state.py` / `voice_client.py` / `guild.py` at 2.7.0a5363 (the git-main build cached by uv, which precedes the 2.7.1 PyPI release; re-check in phase 0, §12.7). |

Notation:

- **L*n*** refers to a lock in §5.3.
- **T*n*** refers to a teardown step in §4.7.
- **W*n*** refers to an implementation workstream in §12.
- **"mixer thread"** means songbird's shared live-mixer `std::thread`, which serves up to 16 calls.
- **"tick"** means 20 ms.

**Decision (2026-10-27): general-purpose and provider-neutral.** Per user direction, the library is a
general-purpose Discord voice I/O library. The goal was only that it be *easy to connect* to realtime
speech services, not that it be built for them. Compatibility with such services therefore comes from
generic capabilities alone: format conversion, chunked async receive streams, and low-latency playback
with `clear()`, tags and positions. There are no provider-specific APIs, format presets, defaults,
helpers, examples or tests. Concretely, this revision removed the provider format presets and the
provider session-format helper, removed every base64 helper (users encode audio themselves when a
protocol needs text; `AudioChunk` supports the buffer protocol), made Discord's native format
(`PCM16_48K_STEREO`, 48 kHz stereo PCM16LE) the default of every receive stream and of
`open_playback`, and replaced the provider bridge examples with a generic echo bot and a generic raw-PCM
WebSocket bridge (§9.11–§9.13). Earlier text in `docs/deviations/` is kept as history.

---

## 1. Goals and non-goals

### 1.1 Goals

| # | Goal | How it is verified |
|---|---|---|
| G1 | **Leak-free by construction.** All audio, queue, connection and songbird-facing logic lives in `crates/sbcore`, a crate with **no pyo3 dependency**, so no Rust state reachable from songbird, tokio or the mixer can name a Python type. In the binding crate, pyclass fields hold only `Arc<sbcore::…>`, songbird handles or `Copy` data. No native class needs `__traverse__`/`__clear__`. | Compile-time: `sbcore/Cargo.toml` has no pyo3-family dependency. `scripts/check_invariants.py` checks the binding crate. Leak tests use `weakref` + `gc` + `_debug_live_counts()` (our `LiveCounter`s, including a sentinel per songbird `Input`) + songbird `Scheduler::live_tasks()`. `total_tasks()` is not used: songbird 0.6.1 never decrements it when a parked mixer is culled (§13.4). |
| G2 | **No Rust-created thread ever attaches to the interpreter.** That covers tokio workers, songbird mixer/UDP/WS/events threads, the disposal thread and decode threads. Only threads that Python created call into Rust while attached. Rust signals Python with a 1-byte doorbell on a socket. Log records are pulled by a Python daemon thread. | `check_invariants.py` forbids `Python::attach`, `try_attach`, `with_gil`. sbcore cannot call pyo3 at all. |
| G3 | **Provably bounded memory.** Every queue has a validated, capped, lazily grown bound. Each connection has a receive budget. No user number directly sizes an allocation. | §10. Bounds tests run in a subprocess and assert the exit code is never 134. A one-hour soak runs without a consumer. |
| G4 | **No blocking where it hurts.** The asyncio loop never waits on a lock that is held across I/O or Python. The mixer thread uses atomics and `try_lock` only: no allocation on the PCM path, no syscalls, no `Notify`, no flume/`TrackHandle` sends, no logging. | Lock inventory (§5.3). CI greps. A counting-allocator test on the mixer pull path. Debug `Role` assertions. |
| G5 | **Generic realtime audio streaming.** **Receive:** continuous, silence-filled, real-time-paced per-user, mixed and per-user-router streams with rate, channel and encoding conversion. Formats: 8/16/24/48 kHz, mono or stereo, PCM16LE / f32LE / µ-law / A-law; the default is Discord's native 48 kHz stereo PCM16LE (no conversion). Chunks of 10–1000 ms. Timestamps, speaking events and drop accounting. **Send:** raw writes of any size (including odd byte counts), bounded buffering with backpressure, underrun plays as silence, `clear()` takes effect within ≤ 40 ms, per-tag (application-defined segment) played/heard positions, wall-clock gap filling from source-timeline timestamps (`at_ms`), an idle signal, optional prebuffering. | Rust pipeline unit tests. pytest through test hooks, with no Discord connection. |
| G6 | **Easy to connect to external audio services, without service-specific code.** Bridging Discord voice to a realtime speech service, a recogniser or any other audio system takes a few lines of user code built only from generic pieces: choose the format and `chunk_ms` the service expects, forward `bytes(chunk)` (or the buffer itself), write returned audio with `write_nowait`, call `clear()` on interruption, and use tags + `item()` to learn how much was played. Async iterators, no callbacks. The library ships no provider presets, protocol helpers or text encodings. | `examples/websocket_bridge.py` (§9.12, a generic raw-PCM WebSocket bridge) runs in CI against a local fake binary echo WebSocket server (its bridge function takes `url`), and `examples/echo.py` (§9.11) runs against a detached connection. Every example is imported in CI (§13.3). |
| G7 | Identical behaviour on CPython 3.14 and 3.14t (`gil_used = false`). | CI matrix. `sys._is_gil_enabled()` stays `False` after import. |
| G8 | **Correct discord.py 2.7 `VoiceProtocol` behaviour.** One op 4 per intent, carrying the right flags. Connect failure of any kind, including cancellation, leaves nothing registered. A timeout raises a `TimeoutError` subclass. `move_to` awaits completion. | Fake-gateway tests (§13.3). |
| G9 | Everything except the UDP/WS transport is testable without Discord. | The `test-hooks` cargo feature and pyo3-free `cargo test -p sbcore`. |
| G10 | Keep 0.5's good ideas: feature-gated codecs plus `supported_codecs()`, Opus passthrough input, optional Arrow, stub generation, cp314/cp314t wheels, structured logging. | §2, §7, §8.13. |

### 1.2 Non-goals

- Video, Go Live, Stage moderation, DM/group calls. Only guild voice is supported.
- Shipping clients, presets or helpers for particular external services (speech recognisers, realtime speech services, …). Protocol adapters belong in user code; `examples/websocket_bridge.py` shows a generic one.
- A drop-in `discord.VoiceClient`. There is no `play(discord.AudioSource, after=)`, and no `latency` property (songbird exposes no heartbeat RTT).
- Thread-blocking receive APIs. Receive is asyncio-based. `read_nowait()` lets any thread poll.
- Compensating for network and listener jitter. `played_ms` means "handed to the Discord encoder". `heard_ms` is an estimate based on `output_latency_ms`.
- Echo cancellation, ML VAD, diarization. Speaking state comes from packet activity plus a hangover, with an optional RMS threshold.
- Stream sample rates other than 8/16/24/48 kHz. Files at any rate still play through songbird's decoder, and in-memory PCM at any rate from 8 to 192 kHz is accepted by `RawPcm.from_samples`, which resamples it once at construction (§7.9). 12 kHz is dropped for streams because it is rarely used.
- `os.fork()` after import. The child gets a disabled library (`os.register_at_fork`).
- songbird's `Call`, `Songbird` manager, `Shard`, `builtin-queue`, serenity/twilight. Patching songbird, including opting out of DAVE.
- abi3/abi3t wheels and PyPy.
- Source compatibility with 0.5.0 (§14).

---

## 2. Dependency matrix (exact versions and rationale, features)

### 2.1 Workspace layout

```
Cargo.toml                 [workspace] members = [".", "crates/sbcore"]; root package = binding cdylib+rlib
crates/sbcore/             pyo3-free core: songbird, DSP, queues, state machines (all Rust logic)
src/                       binding crate: #[pymodule] native, pyclasses, conversions (the only pyo3 code)
```

**Decision (graft B).** Design A enforced "no Python in Rust state" with a grep. The two-crate split turns that rule into a compile-time guarantee for every type songbird, tokio or the mixer can reach. It also lets `cargo test -p sbcore` run without libpython, including in manylinux containers.

### 2.2 Rust: `crates/sbcore` (edition 2024, `rust-version = "1.88"`, CI on stable 1.99.1)

| Crate | Version | Features | Rationale |
|---|---|---|---|
| `songbird` | `=0.6.1` | `default-features = false`, `["driver", "receive", "gateway", "tws", "rustls"]` | We use `Driver` directly and never `Call`. `gateway` is required **only** because `driver/tasks/message/udp_rx.rs` imports `dashmap`, which is gated on that feature. Without it the crate does not compile. `builtin-queue` is dropped (§7.10). DAVE (davey) comes in through `driver`. |
| `symphonia` | `=0.5.5` | `default-features = false`, plus codec/format features forwarded from ours | Feature unification populates `songbird::input::codecs::{get_probe, get_codec_registry}`. Trait impls use `songbird::input::core` (a re-export of symphonia-core 0.5.5), so trait versions always match. |
| `rubato` | `=0.16.2` | default | The same copy songbird links. `SincFixedIn<f32>` is used for tick resampling (§5, §7.4, §8.7). |
| `tokio` | `=1.54.1` | `["rt-multi-thread", "sync", "time", "macros"]` | The two owned runtimes (§5.1), `mpsc`, `Notify`, timers. |
| `parking_lot` | `=0.12.5` | — | Leaf locks with `try_lock`, no poisoning. Already in songbird's graph. |
| `bytes` | `=1.12.1` | — | `RtpData.packet: bytes::Bytes`. Cloning is a refcount bump, with no copy. |
| `bytemuck` | `=1.25.2` | — | Casts between `[i16]`/`[f32]` and `[u8]` on borrowed slices only. **Never** `cast_slice_box` across alignments (§7.4). |
| `async-trait` | `=0.1.92` | — | songbird's `EventHandler` is `#[async_trait]`. |
| `log` | `=0.4.34` | `std` | Logging facade. songbird logs through `tracing` with its `log` feature, so records reach our `log::Log` when no tracing subscriber is installed. |
| `nonmax` | `=0.5.5` | — | `LoopState::Finite(NonMaxU32)` and `TrackHandle::loop_for(NonMaxU32)`. songbird does not re-export `nonmax`. |
| `smallvec` | `=1.16.4` | — | Per-tick SSRC lists (≤ 8 inline) and gateway action lists. Latest 1.x in the registry; satisfies parking_lot_core's `^1.6.1`. |
| `thiserror` | `=2.0.21` | — | `CoreError`. |
| `libc` | `=0.2.191` | `cfg(unix)` only | `send(…, MSG_DONTWAIT \| MSG_NOSIGNAL)` on Linux and `setsockopt(SO_NOSIGPIPE)` on Apple platforms for the doorbell (§5.4). |
| dev: `tokio` | `=1.54.1` | `+ ["test-util"]` | Paused-time tests. |

These come through songbird re-exports, which pins their versions to songbird's:

- `songbird::driver::opus` (opus2 0.4.0, static libopus_sys 0.3.3) for receive decoding and packet validation.
- `songbird::input::core` (symphonia-core 0.5.5).
- `songbird::packet` (discortp) for RTP parsing.
- `songbird::model::CloseCode` (serenity-voice-model 0.3.0).
- `TrackHandle::uuid()` returns `uuid::Uuid`. We use `.as_u128()` only, so there is no direct `uuid` dependency.

### 2.3 Rust: binding crate (root package `discord-ext-songbird` 0.6.0)

`[lib] name = "discord_ext_songbird_native"`, `crate-type = ["cdylib", "rlib"]` (stub_gen needs the rlib), `[[bin]] stub_gen`. Release profile: `lto = true`, `codegen-units = 1`.

| Crate | Version | Features | Rationale |
|---|---|---|---|
| `pyo3` | `=0.29.3` | default only. **No** `experimental-async`, **no** `extension-module` | Every native method is synchronous, so async pyo3 is unnecessary. The `extension-module` feature is deprecated in favour of `PYO3_BUILD_EXTENSION_MODULE`, which maturin sets. 0.30.x is excluded because pyo3-stub-gen 0.23.1 requires `<0.30`. |
| `pyo3-build-config` (build-dep) | `=0.29.3` | — | `add_extension_module_link_args()` in `build.rs` (`-undefined dynamic_lookup` on macOS). |
| `pyo3-stub-gen` | `=0.23.1` | `default-features = false`, `["infer_signature"]` | Stubs for `discord.ext.songbird.native`. The defaults would pull in numpy, either and ordered-float. |
| `sbcore` | path | forwards features | Core. |
| `pyo3-arrow` | `=0.19.0` | optional (`arrow`), `default-features = false` | `to_array_pycapsules` / `to_schema_pycapsule` for Arrow export and `PyArray` extraction for import. It forces numpy 0.29 and the pyo3 `chrono`/`chrono-tz`/`indexmap` features, which is why it is optional. |
| `arrow-array`, `arrow-buffer`, `arrow-schema` | `=59.3.1` | optional (`arrow`) | pyo3-arrow 0.19 requires arrow 59, not 60. Export uses `Buffer::from_custom_allocation(NonNull<u8>, usize, Arc<dyn Allocation>)`, verified in arrow-buffer 59.3.1 `buffer/immutable.rs:158`. |

**Deliberately dropped:**

| Dropped | Reason |
|---|---|
| `pyo3-async-runtimes` | `future_into_py` parks `Py<Future>`/`TaskLocals` inside tokio tasks, which become GC roots and defeat weakref/Drop teardown. It completes through `spawn_blocking` → `Python::attach` on a pool thread, which violates G2. It also returns eager `asyncio.Future`s, which broke `create_task` in 0.5. |
| `pyo3-log` | It attaches on whatever thread logs, including the mixer and UDP threads, and panics during finalisation. Replaced by the pull model in §11.2. |
| pyo3 `experimental-async` | Its waker attaches on the waking thread. |
| `dashmap` (direct), `arrow` (umbrella), `tokio-stream`, `async-stream`, `futures`, `pin-project-lite`, `env_logger`, `uuid` | No longer needed. |

### 2.4 Cargo features (binding crate; sbcore mirrors them)

```toml
[features]
default       = ["codec-full", "arrow"]
codec-full    = ["sbcore/codec-full"]      # symphonia/all + opt-simd (unchanged set)
codec-minimal = ["sbcore/codec-minimal"]   # codec-pcm + format-wav
codec-aac = ["sbcore/codec-aac"]           # … codec-{adpcm,alac,flac,mp1,mp2,mp3,pcm,vorbis}
format-wav = ["sbcore/format-wav"]         # … format-{aiff,caf,isomp4,mkv,ogg}
codec-simd = ["sbcore/codec-simd"]
arrow         = ["dep:pyo3-arrow", "dep:arrow-array", "dep:arrow-buffer", "dep:arrow-schema"]
test-hooks    = ["sbcore/test-hooks"]      # exposes native._testing; never in release wheels
```

`supported_codecs()` mirrors these flags via `cfg!`, as in 0.5. The sdist CI job builds with `--no-default-features --features codec-minimal`, which also proves the crate compiles without `arrow`.

**One native surface for every feature set.** The Arrow-related members (`AudioChunk.__arrow_c_array__`, `ReceiveSubscription.read_batch`, `ChunkBatch`, `AudioFile.from_arrow`, `RawPcm.from_arrow`, `OpusPackets.from_arrow`, and Arrow input to `RawPcm.from_samples`) are **always compiled**. Without the `arrow` feature their bodies raise `SongbirdError("built without the 'arrow' feature")`. The generated stubs, which are built with default features, therefore match every build, including sdist/codec-minimal builds.

### 2.5 Python (`requires-python = ">=3.14"`; cp314 and cp314t wheels)

| Kind | Package | Version | Note |
|---|---|---|---|
| runtime | `discord.py` | `>=2.7.1,<3` (PyPI; the git-main source pin is removed) | Imported by `client.py`, so it is a real runtime dependency (it was missing in 0.5). |
| extra `arrow` | `pyarrow` | `>=26.0.0` | Optional consumer of the capsules. |
| extra `arro3` | `arro3-core` | `>=0.9.1` | Optional consumer. |
| build backend | `maturin` | `>=1.15.0,<2` | hatchling is removed. The dev pin moves from the installed 1.10.2 to `==1.15.0`. |
| dev | `ruff` 0.17.2, `ty` 0.0.92, `pytest` 9.1.1, `pytest-asyncio` 1.4.1, `numpy` 2.5.4, `pyarrow` 26.0.0, `arro3-core` 0.9.1, `websockets` 17.3, `maturin` 1.15.0 | exact pins in `uv.lock` | `asyncio_mode = "auto"`. `websockets` is used only by `examples/websocket_bridge.py` and its test (a local fake binary echo server, §13.3). |

Removed from runtime dependencies: `ty`, `pyarrow`, `arro3-*`, `audioop-lts`, `typing-extensions`. The PyPy classifier is removed.

`[tool.maturin]` is unchanged: `module-name = "discord.ext.songbird.native"`, `python-source = "py-src"`. The justfile, README and AGENTS.md all switch to `uv run --no-sync maturin develop`.

---

## 3. Architecture overview (text diagram of components, threads, data flow)

### 3.1 Component diagram

```
══════════ PYTHON  (only Python-created threads ever attach) ═══════════════════════════════════════════
 discord.py ConnectionState._voice_clients[guild_id]
   └─► SongbirdClient(discord.VoiceProtocol)        [pure Python]
         ├─ _conn ───────────────► native.Connection    (frozen pyclass; one field: Arc<ConnShared>)
         ├─ _pump: _Pump ──task──► loop.sock_recv(rsock) ◄────────────── 1-byte doorbell ───────────────┐
         │      └─ weakref(SongbirdClient, callback → _conn.close("dropped"))                              │
         ├─ AudioStream ────► native.ReceiveSubscription ─Arc─► SubQueue ◄── push (ConnWorker) ─┐         │
         ├─ EventStream ────► native.EventSubscription ──Arc─► EventQueue ◄── emit ─────────────┤         │
         ├─ PlaybackStream ─► native.Playback ──────────Arc─► PcmQueue ◄── pull (mixer, try_lock)│        │
         └─ TrackHandle / TrackQueue ─► native.NativeTrack (Arc<TrackFlags> + songbird TrackHandle)        │
 "songbird-log" daemon thread (Python) ─► native._log_wait() [parks detached] ─► logging.getLogger(…)      │
══════════ binding crate (pyo3): pyclasses hold Arc<sbcore::…> or Copy data only ══════════════════════════│
══════════ sbcore (NO pyo3) ═══════════════════════════════════════════════════════════════════════════════│
 ConnShared ─┬─ L1  Mutex<Option<OwnedDriver>> ─────────────► songbird::Driver (exactly 1, never cloned)    │
             ├─ L2  Mutex<GatewayState>   (pure state machine; actions execute after unlock)              │
             ├─ Arc<WakeHub>{bits:[AtomicU64;4], pending:AtomicBool, Doorbell} ─────────────────────────────┘
             ├─ Arc<EventHub> · Arc<CtrlQueue> · Arc<TrackRegistry> · TrackQueue · OpRegistry
             └─ Arc<WorkerShared>{cmds, notify, shutdown} ◄─────────────── ConnWorker task (RX runtime)
 songbird Driver
   ├─ core task ─► events task ─► Dispatch{Weak<ConnShared>, RxSender} ──try_send──► mpsc<RxInput>(50) ─┐
   ├─ mixer: Idle task (parked) │ live std::thread (≤16 mixers) ─► LiveReader / PcmDecoder / Opus readers │
   └─ WS task · UDP-rx task (transport decrypt + DAVE + jitter buffer; DecodeMode::Decrypt)              │
 ConnWorker (1 tokio task per connection, RX runtime) ◄────────────────────────────────────────────────────┘
   ├─ RxPipeline: extract → opus decode → 960-frame normalise → speaking FSM → MixBus → converters
   │              → lanes (encode + chunk, Arc<ChunkData>) → SubQueue fan-out ; timeline + silence synthesis
   ├─ Housekeeper: playback wakes/events/auto-pause, track completion polling, TrackQueue advance
   └─ timers: gateway deadlines, RX stall watchdog (→ Dispatch re-registration, ReceiveStalled)
 Process globals: NET runtime · RX runtime · SCHEDULER (songbird Scheduler) · DISPOSER (DisposalThread)
                  · LOG_RING · REGISTRY: Mutex<Vec<Weak<ConnShared>>> · ENCODED_WORKERS (≤16 std threads)
```

### 3.2 Threads

§5.2 gives the full rules for each thread. In summary:

| Thread | Count | Runs |
|---|---|---|
| asyncio loop thread | 1 per bot | discord.py, the `_Pump` task, the Python wrappers and synchronous native calls |
| other Python threads | any | `read_nowait`, `write_nowait`, `clear`, `close` |
| Python daemon `songbird-log` | 1 | forwards Rust log records into `logging` |
| `songbird-net-*` | 2 tokio workers (NET runtime) | songbird's core, events, WS, UDP-rx and Idle tasks, plus `Dispatch::act` |
| `songbird-rx-*` | 1 tokio worker by default (RX runtime) | ConnWorkers, connect futures and track ops |
| songbird live mixer threads | managed by songbird | our `FormatReader`/`Decoder` implementations |
| songbird DisposalThread | 1, global | disposal work |
| `songbird-decode-N` | ≤ 16 per process | encoded-stream decoding |

### 3.3 Data flow and latency budget

**Receive (Discord user → consumer):**

1. songbird's UDP-rx task decrypts, then DAVE-decrypts, then passes packets through the jitter buffer. That buffer is `playout_buffer_ms`: 60 ms by default, 40 ms with `VoiceConfig.low_latency()`.
2. A `VoiceTick` fires every 20 ms (`udp_rx/mod.rs:86-111`).
3. `Dispatch::act` copies `(ssrc, Bytes, offsets)` handles into a `RawTick` and sends it with `try_send`.
4. The ConnWorker runs the pipeline:
   1. extract the Opus payload;
   2. decode it once per needed SSRC to 48 kHz stereo f32, with exact 20 ms packet-loss concealment;
   3. normalise to 960 frames;
   4. update speaking state;
   5. mix and limit;
   6. resample once per (source, rate, channels) with `SincFixedIn`, adding 1.3–4.3 ms;
   7. encode and chunk once per lane;
   8. push the shared `Arc<ChunkData>` into each subscriber's `SubQueue`.
5. `WakeHub::notify` writes one byte per tick at most.
6. `_Pump` wakes and resolves the futures waiting on it.
7. `AudioStream.__anext__` pops the chunk synchronously. An `AudioChunk` exposes its PCM through the buffer protocol without copying.

**Typical one-way latency:** 40–60 ms jitter buffer, plus ≤ 20 ms tick phase, plus ≤ 4.3 ms resampler, plus (`chunk_ms` − 20), plus about 0.2 ms wake-up.

**Send (producer → Discord):**

1. `PlaybackStream.write_nowait(data)` runs on the loop thread (or any thread). **Outside every lock** it parses PCM at the **input rate** into an owned buffer. It then takes L6w only to splice the ≤ 7-byte partial-frame carry, and L6 to apply wall-clock gap filling, tag the samples and append one segment: both are O(1) critical sections (§7.3).
2. Every tick the mixer thread calls `LiveReader::next_packet()`, which only checks atomics and returns an empty 960-frame-duration packet with no allocation.
3. songbird then calls `PcmDecoder::decode()`. That function takes `try_lock`, pops `rate/50` frames (zero-filling on underrun or contention), runs our `TickResampler` to exactly 960 frames at 48 kHz f32, applies gain, and fills a preallocated `AudioBuffer<f32>`.
4. songbird takes its no-resample `mix_over_ref` path, then Opus-encodes, DAVE-encrypts and sends.

**Added latency:** 0–20 ms waiting for the next pull, plus about 1.4 ms of resampling, plus ≤ 20 ms until send. **Interruption:** `clear()` empties the queue immediately. Audio is silent after at most one already-pulled tick plus one in-flight packet, i.e. ≤ 40 ms.

**Control:**

1. discord.py calls `on_voice_state_update` / `on_voice_server_update`.
2. These call `Connection.on_voice_state` / `on_voice_server`, which run `GatewayState::step` under L2 and enqueue the resulting action batch in step order.
3. L2 is released, and then a single executor runs the queued batches strictly in step order (§4.6): `driver.connect(info)` (spawned and awaited on RX), `Emit`, `Control`, `Teardown`.
4. Rust requests op 4 by pushing a `Control` and ringing `WAKE_CTRL`. The `_Pump` sends op 4 through discord.py. **Rust never calls Python.**

---

## 4. Ownership and lifecycle model

### 4.1 Invariants

| Id | Invariant | Enforcement |
|---|---|---|
| **I1** | **No Python object in Rust state.** sbcore cannot name pyo3 types at all. In the binding crate, no struct, enum or static field holds `Py<T>`, `Bound`, `PyObject`, `PyBuffer`, `PyUntypedBuffer` or `PyBackedBytes`. The **only** allowed exception is `static EXC_TYPES: [PyOnceLock<Py<PyType>>; N]`, the cached exception classes from `discord.ext.songbird.errors`: process-lifetime type objects that create no cycle with user objects. Python values exist only as locals inside `#[pymethods]`/`#[pyfunction]` bodies. Consequences: the reference pool is never used, no native class implements GC slots, and dropping anything on any thread never touches the interpreter. | Compile time (sbcore). `check_invariants.py` for the binding crate (AST/regex scan of field positions and statics). |
| **I2** | **No attach from Rust-created threads.** Rust → Python signalling is a non-blocking 1-byte `send` (§5.4). Logs are pulled (§11.2). | `check_invariants.py` forbids `Python::attach`, `try_attach`, `with_gil` in `src/`. |
| **I3** | **One owner per Driver.** `OwnedDriver` is a newtype with **no `Clone` and no `Deref`**. It lives in `ConnShared.driver: Mutex<Option<OwnedDriver>>` and is `take()`n exactly once, at teardown. This matters because `songbird::Driver: Clone`, and dropping any clone sends `Poison` (`driver/mod.rs:332-338`). | Type system. |
| **I4** | **No upward strong edges.** The layers are L3 `ConnShared` → L2 `EventHub`, `CtrlQueue`, `TrackRegistry`, `WorkerShared`, `PcmQueue`, `OpusQueue`, `ByteRing` → L1 `SubQueue`, `EventQueue` → L0 `WakeHub`. Strong `Arc`s point downward only. songbird handlers and tasks hold `Weak<ConnShared>`, so no `Arc` cycle can form. | Review + §4.3 table. |
| **I5** | **Closure is idempotent and non-blocking.** Every `close()` first does `if self.closing.swap(true, AcqRel) { return }`, **before taking any lock**. It then does O(1) work and notifies. It never waits for another thread. | Unit tests: double close, close from Drop, close from 8 threads concurrently. |
| **I6** | **Bounded fan-in per connection.** ≤ 256 wake ids, ≤ 64 receive subscriptions, ≤ 16 event streams, ≤ 16 live sources (playback/encoded/Opus streams), ≤ 32 playing memory/file tracks (`play()` plus the current queue entry; each is decoded on the shared mixer thread), ≤ 10 000 queue entries, ≤ 64 pending ops, ≤ 256 decoded SSRCs (configurable, default 64). Process-wide: ≤ 16 encoded-stream decode threads. | `limits.rs` → `LimitExceeded`. |
| **I7** | **Lock discipline.** No lock is held across `.await`, a Python call, a blocking syscall (including the doorbell write) or another blocking wait. sbcore public APIs never return lock guards. Any function that runs a caller-supplied closure under a lock is `pub(crate)`. **State machines compute actions under their lock and execute them after unlocking** (§4.6). | clippy `await_holding_lock = "deny"`. `check_invariants.py` rejects `pub fn … -> …Guard`. Code review. |
| **I8** | **Python buffers never overlap Rust locks.** The binding crate finishes every Python-calling extraction before calling into sbcore, and releases any `PyBuffer`/`PyUntypedBuffer` before that call. `PyUntypedBuffer::drop` calls `try_attach` (`buffer.rs:730`). For `bytes`, the binding borrows `&[u8]`. For `str`, it borrows `&str` (`PyString::to_str`). Both are plain borrows, released by a decref after the sbcore call returns. Any other buffer object is copied into an owned `Vec<u8>` and released first (§7.3). | Review. `convert.rs` is the only module that touches buffers. |

### 4.2 Python-side reference graph

Cycles may exist only here, where Python's cyclic GC collects them.

```
ConnectionState._voice_clients ─► SongbirdClient ─► _conn (native.Connection)
                                               ├─► _pump (_Pump) ─► _conn, rsock, loop, _waiters{wake_id: [Future]}
                                               │        └─► weakref.ref(SongbirdClient, _owner_gone)     # weak
                                               └─► _children: weakref.WeakSet[AudioStream|EventStream|PlaybackStream|…]
asyncio loop ─► _pump._task          (while awaiting sock_recv)
AudioStream ─► native.ReceiveSubscription, _pump     (user-owned; close optional, Drop closes)
_rejoin/_leave helper tasks ─► weakref only
```

- **`_Pump._owner_gone`** is a weakref callback. If the `SongbirdClient` is collected without `disconnect()`/`cleanup()`, the callback calls `_conn.close("dropped")`. That shuts the doorbell down, so `sock_recv` returns `b""`, the pump exits, and the last reference to `native.Connection` goes away.
- **The pump task's `finally:`** also calls `_conn.close("loop_closed")`, so a loop that dies (for example at the end of `asyncio.run`) always tears the connection down.
- **Python never holds a lock-protected Rust object across `await`** in a way that matters. Waits are pure: wait for a wake id, then pop synchronously.

### 4.3 Rust-side strong and weak edges

| Holder | Field / location | Target | Kind | Released when |
|---|---|---|---|---|
| `native.Connection` (pyclass, frozen, weakref) | `shared` | `ConnShared` | `Arc` (the only long-lived strong owner) | Python deallocation. Its `Drop` calls `shared.close(Dropped)` first. |
| `ConnShared` | `driver` | `OwnedDriver` | owned (`Option`) | T4 |
| `ConnShared` | `wake`, `events`, `ctrl`, `tracks`, `worker` | `WakeHub`, `EventHub`, `CtrlQueue`, `TrackRegistry`, `WorkerShared` | `Arc` | `ConnShared` drop |
| `ConnShared` | `sources: Mutex<Vec<Weak<dyn LiveSource>>>` | `PcmQueue` / `OpusQueue` / `ByteRing` | `Weak` | pruned on insert; closed at T5 |
| `ConnShared` | `tasks: Mutex<Vec<AbortHandle>>` | connect/op tasks | handle | aborted at T7. The ConnWorker is **not** in this list: it exits on the shutdown flag (T6). |
| `Dispatch` (inside songbird `GlobalEvents`) | `inner: Arc<DispatchInner{conn: Weak<ConnShared>, rx: Option<RxSender>, tracks: Weak<TrackRegistry>, counters}>` | — | `Weak` + channel sender | `remove_all_global_events()` (T2) or Poison (T4) |
| ConnWorker task | owns `RxPipeline`, `Housekeeper` | `Arc<SubQueue>`×n, `Weak<dyn LiveSource>`×n, `Arc<EventHub>`, `Weak<ConnShared>` | mixed | task exits on the shutdown flag (T6); every queue it feeds is already closed by then |
| connect / op tasks | captured | `Weak<ConnShared>` (upgraded per step), songbird `Connect` / `TrackHandle` futures | `Weak` | completion, timeout or abort |
| `ReceiveSubscription` (pyclass) | `sub: Subscription{queue: Arc<SubQueue>, worker: Weak<WorkerShared>, token: WakeToken}` | — | `Arc` + `Weak` | Python dealloc → `Drop` → `close()` |
| `EventSubscription` (pyclass) | `queue: Arc<EventQueue>`, `hub: Weak<EventHub>` | — | `Arc` + `Weak` | same |
| `Playback` (pyclass) | `q: Arc<PcmQueue>` | — | `Arc` | same; `Drop` → `q.close(Abort)` |
| `LiveReader` / `PcmDecoder` (inside songbird `Track`) | `p: Arc<dyn PcmProvider>`, `done: DoneGuard` | `PcmQueue` / `MemPcm`, `TrackFlags` | `Arc` | track dropped on the mixer thread, Idle task or DisposalThread |
| `PcmQueue` | `wake: WakeToken`, `track: OnceLock<Box<dyn TrackControl>>` | `WakeHub`, songbird `TrackHandle` (an `Arc` over a flume sender; it does not own the `Input`) | `Weak`, owned | last `Arc` |
| `NativeTrack` (pyclass) | `track: TrackRef{flags: Arc<TrackFlags>, handle: OnceLock<TrackHandle>}`, `conn: Weak<ConnShared>` | — | `Arc` + `Weak` | Python dealloc |
| `TrackRegistry` | `Mutex<Vec<Weak<TrackFlags>>>` + `HashMap<u128 /*uuid*/, Weak<TrackFlags>>` | — | `Weak` | pruned by the housekeeper |
| `AudioFile` / `RawPcm` / `OpusPackets` (pyclasses) | `Arc<EncodedBytes>` / `Arc<MemPcm>` / `Arc<OpusFrames>` | — | `Arc`, shared with live tracks | last of {Python object, tracks} |
| `REGISTRY` (static) | `Mutex<Vec<Weak<ConnShared>>>` | — | `Weak` | pruned on insert |

### 4.4 GC policy

- **Native classes declare no `__traverse__`/`__clear__`.** I1 makes them unnecessary. A native class that would need them violates I1 and is rejected by `check_invariants.py`, which forbids the string `__traverse__` in `src/`.
- **Python wrapper classes** (`AudioStream`, `PlaybackStream`, …) are ordinary GC objects. They hold downward references only: wrapper → native handle (+ `_pump`).
- **`AudioChunk` buffer exports** set `Py_buffer.obj = self`. That is CPython's own refcounting, not Rust state.
- **If a future change ever needs a stored `Py<T>`,** it must be a direct field of exactly one pyclass. That pyclass implements `__traverse__` (using `try_lock`, never blocking) and `__clear__`, and the field must never be reachable through an `Arc` shared with Rust threads. Such a change also needs a dedicated leak test.

### 4.5 Connection state machine (`sbcore::conn::gateway`, pure and table-tested)

**Phases:** `Idle`, `Joining`, `Connecting{gen}`, `Connected`, `AwaitingMove{deadline}`, `AwaitingRejoin{attempt, due}`, `Rejoining{attempt, deadline}`, `Closed(CloseReason)` (terminal).

**Public `ConnectionStatus.state`:**

| state | phases |
|---|---|
| `"idle"` | Idle |
| `"joining"` | Joining |
| `"connecting"` | Connecting |
| `"connected"` | Connected |
| `"reconnecting"` | AwaitingMove, AwaitingRejoin, Rejoining |
| `"closed"` | Closed |

Joining entered from a move is reported as `"joining"`.

**Inputs (`GwInput`):**

- `Join{channel, self_mute, self_deaf}`
- `Move{channel}` (from `request_move`, i.e. `move_to`)
- `BeginRejoin`
- `Leave`
- `SetFlags{self_mute, self_deaf}` (from `set_self_mute`/`set_self_deaf`)
- `VoiceState{session_id, channel: Option<u64>}`
- `VoiceServer{token, endpoint: Option<String>}`
- `ConnectResult{generation, result}`
- `DriverConnect{ssrc}`, `DriverReconnect{ssrc}`
- `DriverDisconnect{kind, reason}`
- `RxStalled{since}`
- `HandlersLost` (no heartbeat for 3 s, §8.8)
- `Deadline`
- `Close(reason)`

**Actions (`GwAction`):**

- `Connect{info: songbird::ConnectionInfo, generation}`
- `SetSendMute(bool)`
- `Emit(EventData)`
- `Control(Control)`
- `RxLink{up, ssrc}`
- `ReinstallDispatch`
- `Teardown(CloseReason)`
- `NotifyStatus`

**How songbird 0.6.1 actually reports transport events** (read in `driver/tasks/mod.rs:102-205,243-340` and `ws.rs:205-225,525-560`):

- **Resumable failures are invisible until they end.** WS IO errors and resumable close codes (`should_resume()`: 4015 and 4009) cause `CoreMessage::Reconnect` internally, and **no event fires**. Success fires `DriverReconnect`. Failure falls back to a full reconnect, which fires `DriverReconnect` or **`DriverDisconnect{kind: Reconnect, reason}`**, and that can arrive while we believe we are Connected.
- **Final close codes** produce `SignalWsClosure`, then `DriverDisconnect{kind: Runtime, reason: WsClosed(code)}`. Final codes include 4014, 4006 and 4017. This event is **quashed** when `ws_idx ≠ attempt_idx` (a stale attempt).
- **Our own `leave()`** produces `DriverDisconnect{Runtime, Requested}` only if a connection existed.

"Open phase" means every phase except Idle and Closed. A row that names a phase takes precedence over an `any …` row.

| From | Input | To / actions |
|---|---|---|
| Idle | `Join{c, m, d}` | Joining{target=c}. flags=(m, d). `SetSendMute(m)`. No Rust deadline: the Python `connect(timeout=)` governs. |
| Joining | `VoiceState(s, Some(c))` | session=s. If c ≠ target: target=c, server=None, `Control(ChannelMoved{c})`. If session and server are complete: Connecting{gen+1}, `Connect(info, gen)`. |
| Joining | `VoiceServer(t, Some(e))` | server=(t, normalize(e)). If complete: Connecting{gen+1}, `Connect`. |
| any open phase | `VoiceServer(_, None)` | server=None. A Connected/Connecting phase becomes Joining, with `RxLink{up:false}`. Discord sends a new update once it reallocates the server. |
| Joining (after a move) | `Deadline` (3 s after the channel change, no server update) | **Fallback:** reuse the previous endpoint/token with the new channel_id, giving Connecting{gen+1}. DAVE needs the new `channel_id`, so a new `ConnectionInfo` is always built. Never armed in Rejoining, where the op 4 leave has revoked the old token. |
| Connected | `Move{c}` | Joining{target=c, server=None}. `RxLink{up:false}`. Arms the 3 s server fallback. No `ChannelMoved` control: Python initiated the move and updates `.channel` itself. |
| Joining, Connecting | `Move{c}` | Joining{target=c, server=None}. The newer move supersedes the pending one; a `ConnectResult` from the abandoned attempt is stale, because the next `Connect` bumps the generation. Arms the 3 s fallback. |
| AwaitingMove, AwaitingRejoin, Rejoining | `Move{c}` | target=c, phase unchanged. The next merge (or `Rejoin` control) uses the new channel. |
| Connecting | `ConnectResult(g == gen, Ok)` | Connected. `Emit(Connected)`, `RxLink{up:true, ssrc}`, `NotifyStatus`. The ssrc comes from `DriverConnect` in either order. |
| Connecting | `ConnectResult(g ≠ gen)` or `Err(AttemptDiscarded)` | Ignored (superseded). |
| Connecting | `ConnectResult(g == gen, Err(e))` | If reconnect and attempt < `rejoin_attempts`: AwaitingRejoin{attempt+1, due=now+backoff}. Otherwise Closed(ConnectFailed). |
| Connecting | `DriverDisconnect{Connect, _}` | Ignored (the `ConnectResult` carries the error). |
| Connecting | `VoiceServer(t', Some(e'))`, changed | Connecting{gen+1}, `Connect(new info)`. The in-flight attempt's result is now stale. |
| Connected, Connecting | `VoiceState(s', Some(c'))`, c' ≠ channel (Connecting: c' ≠ target) | Joining{target=c', session=s', server=None}. `Control(ChannelMoved{c'})`, `RxLink{up:false}`. Arms the 3 s server fallback. |
| Connected, Connecting | `VoiceState(s', same channel)`, s' ≠ s | session=s'. Connecting{gen+1} with the known server (mirrors songbird `info.rs:59-97`). Equal session: no-op (that update was just a mute/deaf flag change). |
| Connected | `VoiceServer(t', Some(e'))`, changed | Connecting{gen+1}, `Connect(new info)`. |
| Connected | `DriverConnect{ssrc}` | ssrc stored. `RxLink{up:true, ssrc}` (`DriverConnect` may arrive after `ConnectResult`). |
| Connected | `DriverReconnect{ssrc}` | Stays Connected. `Emit(Reconnected{gap_ms})`, `RxLink{up:true, ssrc}`. |
| any open phase except Connected | `DriverConnect{ssrc}` / `DriverReconnect{ssrc}` | ssrc stored. No phase or link change. Connecting reaches Connected only through its own `ConnectResult`. In other phases the event comes from an attempt that is being replaced. |
| any open phase | `DriverDisconnect{Runtime, Requested}` | Ignored (our own `leave()`). |
| Connected | `DriverDisconnect{Runtime, WsClosed(4014)}` | **AwaitingMove{deadline = now+5 s}.** `RxLink{up:false}`, `Emit(Disconnected{code:4014, will_retry:true})`. **Never disconnects on its own:** 4014 means moved, kicked or channel deleted, and the gateway VOICE_STATE_UPDATE decides which. |
| AwaitingMove | `VoiceState(s, Some(c'))` | Joining{target=c'} (a move). If c' == channel and s is new, Connecting with a fresh session. |
| AwaitingMove | `VoiceState(_, None)` | Closed(Kicked). `Control(Closed)`. |
| AwaitingMove | `Deadline` | If reconnect: AwaitingRejoin{1, due=now}. Otherwise Closed(Lost(4014)). We assume the gateway session dropped, since there was no state update within the grace period. |
| AwaitingMove | `DriverDisconnect{_, _}` | Ignored (already waiting for the gateway). |
| Connected | `DriverDisconnect{Runtime, WsClosed(other final code), Io, TimedOut, ProtocolViolation, Internal}` | If reconnect: AwaitingRejoin{attempt+1}. Otherwise Closed(Lost(reason)). |
| Connected | `DriverDisconnect{Reconnect, _}` | If reconnect: AwaitingRejoin{attempt+1}. Otherwise Closed(Lost). |
| Joining, Connecting, AwaitingRejoin, Rejoining | `DriverDisconnect{Runtime \| Reconnect, _}` (other than `Requested`) | **No phase change.** `RxLink{up:false}`, `Emit(Disconnected{code, will_retry:true})`. This is typically the old voice WS closing with 4014 during a move or rejoin. songbird reports it whenever no newer attempt has started yet (`tasks/mod.rs:117-145` quashes it only for stale attempts). The pending gateway update or attempt decides what happens next. |
| AwaitingRejoin | `Deadline(due)` | Rejoining{attempt, deadline=now+15 s}, **server=None**. `Control(Rejoin{attempt, channel})`: the pump sends op 4 `None`, then op 4 `(channel, flags)`. |
| Rejoining | `VoiceState(_, None)` | **Ignored** (expected from the leave half of the rejoin). |
| Rejoining | `VoiceState` / `VoiceServer` with a channel | Merges like Joining, then Connecting{gen+1} (attempt carried). It requires a **fresh** `VoiceServer`, received after entering Rejoining. The leave revoked the old token, and a `Connect` with it would block songbird's runner inside `Connection::new` for up to `driver_timeout`. |
| Rejoining | `Deadline` | If attempt < `rejoin_attempts`: AwaitingRejoin{attempt+1, backoff}. Otherwise Closed(RejoinFailed). |
| any open phase | `HandlersLost` | `ReinstallDispatch` (rate-limited to once per 5 s, shared with `RxStalled`). No phase change. |
| Connected | `RxStalled{since}` | `ReinstallDispatch` (rate-limited to once per 5 s). If stalled ≥ 5 s, and `auto_recover` and reconnect are set: AwaitingRejoin{1, due=now} (the documented DAVE #310 / UDP-rx #315 recovery). |
| any open phase | `BeginRejoin` (from `vc.reconnect()`) | Rejoining{attempt=0, deadline=now+timeout}, **server=None**. Python then sends the op 4 pair itself. |
| any open phase | `SetFlags{m, d}` | flags=(m, d); later `Rejoin` controls carry them. `SetSendMute(m)`. No phase change. |
| any open phase except Rejoining | `VoiceState(_, None)` | Closed(Requested if `leave_requested` else LeftChannel). `Emit(Disconnected{will_retry:false})`, `Control(Closed)`. |
| any | `Leave` | `leave_requested = true` (the following VoiceState(None) completes the transition). |
| any | `Close(r)` | Closed(r), `Teardown(r)`. |
| any | any (phase, input) pair not listed above | **No-op:** no actions and no phase change, plus a rate-limited DEBUG log. Examples: `ConnectResult` outside Connecting, `Deadline` with nothing armed, `Join` outside Idle, `RxStalled` outside Connected. |
| Closed | anything | No-op (`apply` returns before taking L2 once `closing` is set). |

- **Rejoin backoff:** 1, 2, 4, 8, 16 s with ±20 % jitter.
- **`rejoin_attempts`:** default 3, range 0–10.
- **`ConnectionInfo`** is `{channel_id: ChannelId::from(target), endpoint, guild_id: GuildId::from(guild), session_id, token, user_id: UserId::from(user)}`. All fields are public (`info.rs:102-118`). songbird 0.6.1 ids are tuple structs (`ChannelId(pub NonZeroU64)`) with only `From<NonZeroU64>` and no `::new` (`id.rs:23-98`), so `target`, `guild` and `user` are `NonZeroU64`. A zero id is rejected with `ValueError` at the binding boundary. `user_id` is **`client.user.id`**, not `application_id`.
- **Endpoint normalisation** strips the `wss://` scheme and any trailing `/`.

### 4.6 The apply discipline

This prevents the defect-8-class self-deadlock, and it keeps actions in the order of the steps that produced them.

`apply()` runs concurrently on the loop thread (`VoiceState`, `VoiceServer`, `Join`, `Move`, `SetFlags`), on NET inside `Dispatch` (`DriverConnect`/`DriverDisconnect`) and on RX (`ConnectResult`, `Deadline`, `RxStalled`). Some actions are order-sensitive. songbird's runner reconnects whenever `conn.info != info` (`driver/tasks/mod.rs:66-90`), so a `Connect{gen 5}` that reached songbird after `Connect{gen 6}` would leave the Driver on stale info while the state machine believes gen 6 is connected. In the same way, `RxLink{up:false}` overtaking `RxLink{up:true}`, or `Emit(Disconnected)` overtaking `Emit(Connected)`, would corrupt the worker's link flag and the event order. Therefore every batch is **enqueued under L2 in step order** and **executed by one executor at a time, in that order** (flat combining):

```rust
pub(crate) fn apply(&self, input: GwInput) {
    if self.closing.load(Acquire) { return; }
    {
        let mut gw = self.gw.lock();                                   // L2
        let mut batch: SmallVec<[GwAction; 4]> = SmallVec::new();
        gw.step(input, self.clock.now(), &mut batch);                  // pure; no I/O
        if !batch.is_empty() {                                         // L2 → L2q: enqueue order == step order
            let seq = gw.next_action_seq();
            self.pending.lock().push_back(ActionBatch { seq, actions: batch });
        }
    }                                                                  // L2 released HERE
    self.run_pending();
}
fn run_pending(&self) {
    // Whoever wins `executing` drains every queued batch in order; losers return at once,
    // because their batch is already queued and the winner will run it.
    while self.executing.compare_exchange(false, true, AcqRel, Acquire).is_ok() {
        while let Some(b) = self.pending.lock().pop_front() {          // L2q held only for the pop
            for a in b.actions { self.execute(a, b.seq); }             // may take L1, emit, set ctrl, ring doorbell,
        }                                                              //   or call self.close() (takes L2: safe)
        self.executing.store(false, Release);
        if self.pending.lock().is_empty() { break; }                   // re-check: a batch queued after our last pop
    }
}
```

- **No lost batches.** A loser's push to L2q happens before its failed CAS. The winner re-checks L2q after clearing `executing`, and that re-check is ordered after the push by L2q. So either the winner sees the batch, or the loser's CAS happens after the winner's store and the loser executes the batch itself.
- **Re-entrancy.** If `execute` reaches `apply()` again on the same thread, the nested call only enqueues; the outer loop runs it next. `execute` and `close` never run while L2 or L2q is held. Because `parking_lot` mutexes are not re-entrant, this is the only safe shape.
- **After close.** Once `closing` is set, `execute` drops every action except `Teardown`.
- **Defence in depth.** `OwnedDriver` keeps `last_connect_gen` under L1 and ignores a `Connect{gen}` with `gen ≤ last_connect_gen`. `RxLink` carries the batch sequence number, and the worker ignores a `WorkerCmd::Link` older than the last one it applied.
- **Tests.** A unit test drives every terminal transition from inside a `Dispatch` callback and from a Python-thread call, and asserts that it completes. A stress test runs `apply()` from three threads with interleaved `VoiceServer` changes and asserts that the `Connect` generations reaching a fake driver, and the `RxLink` values reaching the worker, are monotonic and end in the state machine's final values.

### 4.7 Complete teardown: `ConnShared::close(reason)`

**Reachable from:**

- `SongbirdClient.disconnect()`;
- `VoiceProtocol.cleanup()`;
- `native.Connection` deallocation (`Drop`);
- the `_Pump` weakref callback, or its `finally` (loop closed / task cancelled);
- a terminal state-machine transition (kick, connect failure, rejoin exhausted);
- a `connect()` abort;
- `native._shutdown_all()` at `atexit`.

| Step | Action | Thread | Blocking? |
|---|---|---|---|
| T0 | `if self.closing.swap(true, AcqRel) { return }`. **No lock has been taken yet.** | caller | no |
| T1 | Lock L2, set phase = Closed(reason), copy `status`, unlock. | caller | µs |
| T2 | Lock L1, `take()` the `OwnedDriver`, unlock. Then outside every lock: `driver.teardown()`, which runs `let _g = runtime::net().enter();` (required: `Driver::send` may call `restart_inner` → `tokio::spawn`, `driver/mod.rs:267-274`) and then calls `remove_all_global_events()`. That drops every `Dispatch` clone on the events task. | caller | flume sends only |
| T3 | Inside `OwnedDriver::teardown`: `stop()` → `SetTrack(None)`. All tracks and inputs are dropped **inline** on the mixer thread or Idle task (`mixer/mod.rs:225`), which works while parked (`scheduler/task.rs:158-166`). Each reader's `Drop` sets its `DoneGuard`. | caller | flume send |
| T4 | Inside `OwnedDriver::teardown`: `leave()` (`Disconnect` → `DropConn`, so UDP-rx ends), then `drop(driver)` (`Poison` → core and events tasks exit, the mixer is culled, the WS task ends). Poison is FIFO behind any in-flight connect attempt, which takes ≤ `driver_timeout` (default 10 s, capped at 60 s). | caller | flume sends |
| T5 | Close every live source in `sources` (`PcmQueue::close(Abort)`, `OpusQueue::close`, `ByteRing::close`). Queued audio is freed immediately (swapped out under lock, dropped after unlock). Writers get `StreamClosed("connection_closed")`. Decode threads see EOF. | caller | µs |
| T6 | **Synchronously, on the caller thread** (nothing here depends on the ConnWorker running again): `worker.close_queues(ConnectionClosed)` closes every registered `SubQueue` and router queue (`known_queues: Vec<Weak<…>>`, registered at subscribe time). Then `events.close_all(Some(&Disconnected{will_retry:false}))`: under L5h it marks the hub closed (later `emit`s are dropped), appends the final event to every `EventQueue` and closes them, so `Disconnected` is always last. Finally `worker.shutdown()` sets the flag and notifies. The ConnWorker exits at its next poll. Its exit path is only best-effort cleanup (decoders, resamplers, lanes); everything observable is already closed. | caller | µs |
| T7 | Abort the **connect and op** tasks in `tasks`. Their owned state drops on an RX worker. The ConnWorker is **not** aborted. tokio drops an aborted idle task without polling it again, so an abort would skip its exit path, and T6 no longer needs that path. | caller | no |
| T8 | `tracks.finish_all(Disconnected)` resolves every track not yet finalised (T2 removed the `Dispatch`, so no `End` can arrive any more). Then `ops.fail_all()`, `ctrl.set_closed(reason)` (a sticky slot), and `wake.notify(WAKE_TRACKS)`. | caller | µs |
| T9 | `wake.notify(WAKE_CONN)`, then `wake.close()` (`shutdown(SHUT_WR)` on the doorbell). Python reads the remaining bytes and then `b""`. Remove this connection from `REGISTRY`: collect, release the lock, nothing else runs under it. **In a forked child** (`FORKED` set, or the creating pid ≠ `getpid()`), `close()` and `Drop` skip T2–T9 and leak the state. The doorbell socket is shared with the parent, and `shutdown()` would end the parent's live connection (§4.10). | caller | one syscall |
| T10 | Python `_Pump` on EOF: a final `take_ready()`, resolve every waiter (consumers re-check and see closed), then `SongbirdClient._finalize()`, which calls `cleanup()` and so `_remove_voice_client`. | loop | — |

Steps T0–T9 never block, never await and never touch Python, so they are safe in `Drop` on any thread and during interpreter finalisation.

### 4.8 The songbird Driver: creation, use, drop

**Creation.** In `ConnShared::new`, under `runtime::net().enter()`, because `Driver::new` and `Scheduler` call `tokio::spawn`:

```rust
let cfg = songbird::Config::default()
    .decode_mode(if s.receive { DecodeMode::Decrypt } else { DecodeMode::Pass })
    .playout_buffer_length(s.playout_packets)          // NonZeroU8 (playout_buffer_ms / 20)
    .playout_spike_length(s.spike_packets)              // u8
    .decode_state_timeout(s.decode_state_timeout)       // Duration
    .mix_mode(s.mix_mode)                               // MixMode::{Mono, Stereo}
    .use_softclip(s.softclip)
    .driver_timeout(Some(s.driver_timeout))             // Option<Duration>
    .driver_retry(Retry { retry_limit: Some(s.driver_retries), ..Retry::default() })  // Option<NonZeroU8>
    .preallocated_tracks(PREALLOCATED_TRACKS)           // u8 = MAX_LIVE_SOURCES_PER_CONN + MAX_PLAYING_TRACKS_PER_CONN = 48
    .disposer(runtime::disposer().clone())              // ONE DisposalThread per process
    .scheduler(runtime::scheduler().clone());           // our Scheduler (leak-test observable)
```

`preallocated_tracks` defaults to 1. songbird sizes the mixer's `tracks`/`track_handles` `Vec`s with it (`mixer/mod.rs:113-114,320-322`), so with the default, adding a second track would reallocate on the mixer thread. The I6 caps make 48 an upper bound, so no track addition allocates there.

**Retry mapping.** `Retry.retry_limit: Option<NonZeroU8>`, and `None` means **infinite** retries (`retry/mod.rs:45`, `is_none_or`). We therefore **never pass `None`**:

- `reconnect=True` → `Some(VoiceConfig.driver_retries)`. The value is validated in `1..=10`, default 5. A value of 0 is rejected with a message that explains why.
- `reconnect=False` → `Some(NonZeroU8::MIN)`, i.e. one retry, the minimum songbird can express.

The decode mode is fixed at construction, because songbird ignores a later switch from Decrypt to Decode (`udp_rx/mod.rs:75-81`).

**Dispatch registration.** Registered immediately, once per event, before any connect:

- `CoreEvent::{SpeakingStateUpdate, ClientDisconnect, DriverConnect, DriverReconnect, DriverDisconnect}`;
- `CoreEvent::VoiceTick`, only if `receive`;
- `Event::Track(TrackEvent::End)` and `Event::Track(TrackEvent::Error)`;
- `Event::Periodic(1 s, None)`, a **heartbeat**, on a separate `Heartbeat` handler that shares `DispatchInner` (§8.2). Global timed events fire from the `EventMessage::Tick` that both live mixers (`mixer/mod.rs:493`) and parked mixers (`scheduler/task.rs:175-179`, every 20 ms from the Idle task) send. Missing heartbeats therefore detect lost global handlers whether or not `receive` is on (§8.8).

**Use.** Every Driver call goes through `OwnedDriver` methods, which enter the NET runtime and do O(1) flume sends: `connect`, `leave`, `mute`, `play`, `stop`, `set_bitrate`, `remove_all_global_events`, `add_global_event`, `reinstall_dispatch`. `connect(info, generation)` returns a `'static` `Connect` future, or `None` when `generation ≤ last_connect_gen` (a stale batch, §4.6). The caller releases L1 and then spawns `timeout(connect_deadline, fut)` on RX, where `connect_deadline = min(120 s, driver_timeout × (retries + 1) + 30 s)`. Results are generation-checked.

**Drop.** Only in T2–T4, after which the Driver is never used again. That rules out `restart_inner` resurrecting a fresh core, and rules out songbird's "zombie retry after `leave()`" (`tasks/mod.rs:92-116`). `Driver::leave()` is called **only** in teardown.

### 4.9 Lifecycles of sub-objects

| Object | Created | Ends | On end |
|---|---|---|---|
| `ReceiveSubscription` | `conn.subscribe_*()`. `WorkerCmd::AddSub` is applied at the next lane boundary. | `close()`, Python dealloc, connection teardown, overflow with policy `error`, `UserLeft` (router streams with `close_on_leave`) | `SubQueue.closed = Some(reason)`, then `WorkerCmd::RemoveSub(id)`. The worker also drops any subscription whose queue `is_closed()` at every tick, so removal never depends on message delivery. Queued chunks stay readable until drained, then `StopAsyncIteration`. |
| `EventSubscription` | `conn.subscribe_events()` | `close()`, dealloc, teardown | Removed from `EventHub` on the next emit or by explicit unsubscribe. |
| `Playback` | `conn.open_playback()`: build `PcmQueue` (with `paused_idle = true`) + `LiveReader` + `PcmDecoder`, then `driver.play(Track::new(Input::Live(LiveInput::Parsed(..), None)).pause())`, then `q.attach_track(handle)`, then register with the Housekeeper. **Live sources start paused**: songbird counts frames, not content (`mix_len = MixedPcm(960)` for 960 zero frames, `mixer/mod.rs:529-566`), so an unpaused but unwritten playback would Opus-encode and send silence every 20 ms, hold the speaking state and disable Opus passthrough for other tracks. The first accepted write resumes the track (§7.3e, ≤ 1 tick). | `close()`/`aclose(drain)`, dealloc, `stop_all()`, teardown, reader panic | `closed` → `next_packet` returns `Err(IoError(UnexpectedEof))` once drained (immediately for Abort). songbird ends the track. The reader's `Drop` sets `DoneGuard` and `reader_gone`. |
| Memory sources (`AudioFile`, `RawPcm`, `OpusPackets`) | constructor (one copy) | last reference | `Arc` freed (may happen on the disposal thread: Rust-only memory). |
| `NativeTrack` | `play` / `enqueue` / `enqueue_stream` | — | `TrackFlags` resolves by **priority, not first writer** (§7.10). The `Dispatch` track-event handler (`finished`/`stopped`/`error`) is authoritative. `DoneGuard::drop` only sets a provisional `DROPPED` bit (plus `EOF_SEEN`/`DECODE_ERROR` if our reader observed them), and the housekeeper finalises a dropped-only track after a 2-tick grace. T8 resolves anything left as `disconnected`. |

### 4.10 Process exit and fork

- **At exit.** `discord/ext/songbird/__init__.py` registers `atexit.register(native._shutdown_all)`. That sets `SHUTTING_DOWN`, which stops the log forwarder after a final drain. It then snapshots `REGISTRY` (lock, clone the `Weak`s, unlock) and calls `close(ProcessExit)` on each live connection.
  - `NET`, `RX`, `SCHEDULER` and `DISPOSER` are statics that are never dropped. Exit joins no Rust thread.
  - By I2, Rust threads still running during `Py_Finalize` cannot touch the interpreter.
- **Fork.** `os.register_at_fork(after_in_child=native._disable_after_fork)` sets `FORKED`. Every native entry point in the child then raises `SongbirdError("discord.ext.songbird is unusable after fork()")`.
  - `Drop` is not an entry point, so the check alone would not protect the parent. A `native.Connection` inherited by the child and dropped there would run T9 `shutdown(SHUT_WR)` on the doorbell socket, which the child shares with the parent, and the parent's pump would see EOF and tear down its live connection. `ConnShared` and `WakeHub` therefore record the creating pid. `close()` and every `Drop` skip T2–T9 when `FORKED` is set or `getpid()` differs, and leak the state instead. They never call `shutdown()` or touch the inherited Driver from the child.

---

## 5. Threading and concurrency model

### 5.1 Process-global runtime objects (`sbcore::runtime`)

```rust
pub static NET: LazyLock<tokio::runtime::Runtime>;     // multi_thread, worker_threads(cfg.net_workers=2), enable_all,
                                                        //   thread_name("songbird-net"), max_blocking_threads(16),
                                                        //   on_thread_start(|| set_role(Role::Net))
pub static RX: LazyLock<tokio::runtime::Runtime>;      // multi_thread, worker_threads(cfg.rx_workers=1), enable_all,
                                                        //   thread_name("songbird-rx"), max_blocking_threads(4),
                                                        //   on_thread_start(|| set_role(Role::Rx))
pub static SCHEDULER: LazyLock<songbird::driver::Scheduler>;     // Scheduler::new(SchedulerConfig::default()) under NET.enter()
pub static DISPOSER: LazyLock<songbird::driver::DisposalThread>; // DisposalThread::run(): one thread per process
```

- **Sizing.** `native.configure_runtime(net_workers=2, rx_workers=1)` is valid only before the first runtime touch; a later call raises `RuntimeError`. Allowed ranges: `net_workers` 1–8, `rx_workers` 1–8.
- **Decision (graft C, resolving a must-fix).** Receive DSP runs on its own **RX runtime**, not on songbird's NET workers. A heavy receive load (many speakers × many lanes) therefore cannot delay songbird's UDP and WS timing. The per-tick budget is still bounded: about 0.3 ms for 10 speakers and 2 lanes.
- **Where songbird work runs.** Songbird's own tasks (core, events, WS, UDP-rx, Idle) are spawned on NET because the Driver and Scheduler are created under `NET.enter()`.

### 5.2 Threads: what each may and must never do

| Thread | Runs | Allowed | Forbidden |
|---|---|---|---|
| **asyncio loop thread** | discord.py, `_Pump`, wrappers, every native method (each synchronous and O(1) or O(input)) | Leaf parking_lot `lock()` with µs critical sections. Allocation. Creating Python objects **outside** sbcore calls. | Waiting on a condvar, channel, tokio task or the mixer. `blocking_lock`/`blocking_recv`/`block_on`. Holding a buffer across an sbcore call (I8). Blocking file I/O (`AudioFile.from_path` is documented for `asyncio.to_thread`). |
| **other Python threads** (3.14t; also GIL builds) | `read_nowait`, `write_nowait`, `clear`, `close`, properties, source constructors | As above, plus CPU-heavy work (`RawPcm` conversion, `AudioFile` probe/read) inside `py.detach(...)` after the input has been copied | Awaiting pump futures. They are bound to the connection's loop; other loops raise `RuntimeError`. |
| **Python `songbird-log` daemon thread** | `native._log_wait()`: parks **detached**, then drains while attached; emits `logging` records | `py.detach(\|\| thread::park_timeout(250 ms))` | — |
| **NET workers** (`songbird-net-*`) | songbird core/events/WS/UDP-rx/Idle tasks; `Dispatch::act` | Atomics, `try_send`, O(1) `apply()` (L2 step, then actions), `EventHub::emit` | Attaching (impossible: sbcore). `.await` inside `Dispatch::act`. Unwinding out of `act`: it is wrapped in `catch_unwind`, because a panic there would kill the events task and trigger `RebuildInterconnect`, which drops all global handlers (`message/mod.rs:36-54`). Per-tick logging. |
| **RX workers** (`songbird-rx-*`) | ConnWorker loops (receive DSP, housekeeping, deadlines, watchdog), connect futures, op futures | CPU work bounded per tick. `lock()` on leaf locks. Doorbell writes (after unlocking). `TrackHandle::pause/play/stop` (flume sends). `OwnedDriver` calls under L1. | Attaching. Sleeping. Blocking I/O. Holding a lock across `.await`. Unwinding out of a tick (the per-tick body runs inside `catch_unwind`). |
| **songbird live mixer threads** | `LiveReader::next_packet`, `PcmDecoder::decode`, `OpusStreamReader`, `MemOpusReader`, songbird decoding of in-memory `AudioFile` cursors | **Atomics and `try_lock` only.** `memcpy`. Our `TickResampler::process_tick` (preallocated). | `lock()`. Attaching. Sleeping. Syscalls (no doorbell, no `Notify`, no flume/`TrackHandle` sends). Logging. **Heap allocation on the PCM path** (the Opus packet readers allocate one `Box<[u8]>` per packet, which songbird's `Packet` API requires, §7.9). Returning a 0-frame packet (`mix_logic.rs:149-153` would spin). Returning `Err` before close+drain (any `Err` ends the track, `mix_logic.rs:103-106`). Unwinding (bodies are wrapped in `catch_unwind`, so a bug ends one track instead of killing a worker serving 16 calls). |
| **DisposalThread / Idle task** | Drops ended tracks and sources | `Drop` impls: atomic stores only (`DoneGuard`, `reader_gone`) | Locks shared with Python threads; attaching. |
| **songbird BlockyTaskPool** | Probing in-memory `AudioFile` bytes (`LiveInput::Raw` promotion) | CPU on Rust memory | — |
| **`songbird-decode-N`** (std threads, ≤ 16 per process) | `EncodedStream` symphonia probe/decode, resample to 48 kHz | Blocking waits on `ByteRing`'s condvar and the internal queue's space condvar. `close()` wakes both, so every wait is bounded by teardown. The `TrackHandle::play()` flume send that resumes the paused track on the first decoded frame (§7.3e). | Attaching. Waits that `close()` cannot wake. |

**Debug enforcement (graft C).**

- **`Role` thread-local.** `sbcore::runtime::Role` is set by the `on_thread_start` hooks (Net/Rx), lazily by the mixer readers on their first call (Mixer), and by decode threads (Decode).
- **Assertions.** `sbcore::sync::lock_checked(&m)` wraps every blocking `lock()` and calls `debug_assert_ne!(role(), Role::Mixer)`. Mixer entry points call `debug_assert_ne!(role(), Role::Python)`; tests that drive them from Python set `Role::Harness`.
- **I2 is structural.** The binding crate runs only on Python threads, because pyclass `Drop`s run where Python deallocates.

### 5.3 Lock inventory

All locks are `parking_lot`. None is held across `.await`, a Python call or a syscall.

| Id | Lock | Protects | Takers | Max hold | Nesting |
|---|---|---|---|---|---|
| L1 | `ConnShared.driver: Mutex<Option<OwnedDriver>>` | the Driver | Python threads, RX tasks, teardown | O(1) flume sends | leaf |
| L2 → L2q | `ConnShared.gw: Mutex<GatewayState>` → `ConnShared.pending: Mutex<VecDeque<ActionBatch>>` | state machine → ordered action batches | Python threads, `Dispatch` (NET), RX tasks | one pure `step()` plus one push; L2q alone: one pop | **L2 → L2q** only (actions run after unlock, §4.6) |
| L3 | `WorkerShared.cmds: Mutex<Vec<WorkerCmd>>` | pending subscribe/filter/source commands | Python (push), ConnWorker (swap) | Vec push/swap | leaf |
| L4 | `SubQueue.inner` | chunk deque, armed flag, counters | ConnWorker (push), Python (pop) | O(chunks dropped by policy). Freed chunks are dropped **after** unlock. | leaf |
| L5h → L5q | `EventHub.queues` → `EventQueue.inner` | registry → event deque | NET (Dispatch), RX (worker), Python | O(#queues) | **L5h → L5q** only |
| L6w → L6 | `PcmQueue.writer: Mutex<Carry>` → `PcmQueue.inner: Mutex<QueueInner>` | partial-frame carry (≤ 7 bytes + generation) → segments, anchor, tags, gates, positions | Writers: Python threads and the encoded decode thread (L6w → L6). Housekeeper, `clear()`, `close()`, T5 (**L6 only, never L6w**). **Mixer: `try_lock` on L6 only.** | L6w: splice ≤ 7 bytes (parsing happens before any lock, §7.3). L6: O(1) push, O(tags) for `clear`, which swaps the deque out. | **L6w → L6** only |
| L7 | `OpusQueue.inner` | packet deque | Python (push), mixer (`try_lock`), Housekeeper | O(1) | leaf |
| L8 | `ByteRing.inner` + `Condvar` | encoded bytes | Python (push), decode thread (wait) | memcpy | leaf |
| L9 | `LOG_RING` | log records | **any thread: `try_lock` only** (drop and count on contention); log drainer: `lock` | VecDeque push/pop | leaf |
| L10 | `REGISTRY`, `ConnShared.sources`, `ConnShared.tasks`, `OpRegistry`, `TrackRegistry.map`, `TrackQueue.items`, `CtrlQueue.slots` | registries | Python, RX, NET | O(n) small | leaf (nothing is called under them) |
| L11 | `LOG_WAITER: Mutex<Option<Thread>>` | parked log thread handle | producers: `try_lock` only; log thread: `lock` | clone | leaf |

**Rules:**

- **Ordering.** The only permitted nestings are L2 → L2q, L5h → L5q and L6w → L6. Every other lock is a leaf.
- **Why plain `lock()` from attached Python threads is deadlock-free, including against free-threaded stop-the-world GC.** Every critical section is sbcore code, which cannot call into Python, reach a safe point, or wait for one. A holder therefore always finishes in microseconds without needing the interpreter. Non-Python holders never attach (I2). Python holders never call Python inside the section (I7, I8). A waiting attached thread delays a stop-the-world pause by at most one critical section.
- **Decision.** Review allowed two options: enable pyo3's `parking_lot` feature and use `lock_py_attached`, or state and enforce this invariant. We chose the second. sbcore has no pyo3, so `lock_py_attached` is not available there. Making the invariant structural is stronger than detaching per lock.
- **The mixer never calls `lock()`**, only `try_lock()` on L6 and L7. Contention costs one tick of silence (`contended_ticks += 1`): the audio is delayed, not lost.
- **Doorbell writes, condvar notifies and `Notify::notify_*` happen after unlocking.**

### 5.4 The wake protocol: Rust → Python with zero Python references

**Setup (Python, in `connect()`):**

```python
rsock, wsock = socket.socketpair()        # AF_UNIX on Unix; CPython's AF_INET loopback pair on Windows
rsock.setblocking(False)
bell = native.WakeBell(wsock.detach())    # Rust adopts the fd/SOCKET immediately (OwnedFd / OwnedSocket)
conn = native.Connection(bell, guild_id, user_id, settings, reconnect)   # moves the bell out of WakeBell
```

- **Ownership of the fd.** `WakeBell.__new__(fd: int)` has a single integer argument, so argument extraction cannot fail after the descriptor leaves Python. From then on, Rust owns it.
  - If `Connection(...)` fails before taking the bell, the `WakeBell` object still owns the descriptor and closes it on dealloc.
  - If it fails after taking it, the partially built Rust state drops the `Doorbell`, which closes it.

  Either way the descriptor is closed exactly once.
- **Adopt** (`Doorbell::adopt`):
  - set non-blocking;
  - on Apple targets, `setsockopt(SO_NOSIGPIPE, 1)`;
  - on Windows, `TcpStream::from(OwnedSocket)` + `set_nonblocking(true)`.
- **Ring:**
  - Linux: `libc::send(fd, &1u8, 1, MSG_DONTWAIT | MSG_NOSIGNAL)`;
  - Apple: `send(…, MSG_DONTWAIT)` (SIGPIPE is suppressed by `SO_NOSIGPIPE`);
  - Windows: `write`.

  `EAGAIN` and `EPIPE` are ignored: either a byte is already pending, or Python is gone. An embedder that does not ignore SIGPIPE therefore cannot be killed by the doorbell.
- **Waiting.** Python awaits `loop.sock_recv(rsock, 256)`. This works on Selector, Proactor (Windows) and uvloop loops.

**Algorithm (`WakeHub`):**

```
notify(id):            // any non-mixer thread; wait-free apart from at most one non-blocking send
    bits[id/64].fetch_or(1 << (id%64), SeqCst)
    if !pending.swap(true, SeqCst): bell.ring()
take_ready(out):       // Python only, after sock_recv returned
    pending.store(false, SeqCst)
    for i in 0..4: w = bits[i].swap(0, SeqCst); push every set bit of w into out
```

**No lost wake-ups.** Each side writes one location and then reads the other: the producer sets `bits` and then reads `pending`, and the consumer clears `pending` and then reads `bits`. That is the store-buffer (Dekker) pattern, and only `SeqCst` on all four operations forbids its bad outcome under the Rust/C++20 model. In that outcome the producer's `swap` reads `pending == true` (it precedes the consumer's `store(false)`) while the consumer's `bits.swap` misses the producer's `fetch_or`: the bit would stay set with no byte written. With `AcqRel` nothing synchronises in that case; ARMv8's RCsc instructions happen to prevent it, but the model does not. With `SeqCst` the single total order S would have to contain the cycle store(false) < bits.swap < fetch_or < pending.swap < store(false), which is impossible. So either the consumer collects the bit, or the producer sees `pending == false` and rings again. The worst case is a spurious wake-up with an empty list. Ready ids are level-triggered, and a woken consumer always re-polls. `check_invariants.py` rejects any ordering other than `SeqCst` on these four operations.

**Arming.**

- A consumer arms inside the same critical section in which it observes "empty": `SubQueue::try_pop_or_arm`, `EventQueue::try_pop_or_arm`, `PcmQueue::arm_space`.
- A producer checks and clears `armed` under that lock, and calls `notify` **after unlocking**.
- At most one byte is written per notification burst. With 20 ms chunks that is ≤ 50 loop wake-ups per second per connection, regardless of how many streams exist.

**Reserved ids:** 0 = `WAKE_CONN` (status changed), 1 = `WAKE_OPS` (op results), 2 = `WAKE_CTRL` (control slots), 3 = `WAKE_TRACKS` (some track's `ended` changed, or a queued encoded stream became current; level-triggered, §7.10). Ids 4–255 are allocated per object and freed with `fetch_and` when the `WakeToken` drops, on any thread.

**Mixer threads never call `notify`.** Readers publish atomics (`space_epoch`, `queued_frames`, `pulls`, `reader_gone`). The ConnWorker Housekeeper turns those into wake-ups every tick (§7.5).

### 5.5 Cancellation semantics

- **Every native operation is atomic and synchronous.** Python-level awaits only wait for readiness:

  ```python
  while (item := native.try_pop_or_arm()) is None:
      await pump.wait(wake_id)
  ```

  Cancelling such an await (TaskGroup, `asyncio.timeout`) can never lose or duplicate a chunk.
- **`await PlaybackStream.write()`** is all-or-nothing per attempt. A cancelled `write()` has written nothing.
- **`_Pump.wait()`** removes its future in `finally`.
- **`connect()`** catches `BaseException`, which includes `CancelledError` (§6.2).
- **Every public awaitable is a real `async def` coroutine.** `asyncio.create_task`, `TaskGroup` and `asyncio.timeout` behave normally. 0.5's eager-Future `TypeError` is gone.

### 5.6 Free-threaded Python (3.14t) specifics

- `#[pymodule(gil_used = false)]`. CI asserts that `sys._is_gil_enabled()` is still `False` after importing the package, numpy, pyarrow and discord.
- **Every pyclass is `#[pyclass(frozen)]`** with `Send + Sync` fields (Arc, atomics, parking_lot). There are no `&mut self` methods, so `RuntimeError: Already borrowed` cannot occur. `Connection`, `ReceiveSubscription` and `Playback` also get `weakref`.
- **Concurrency guarantees:**
  - `AudioChunk` data is immutable after creation, so concurrent buffer exports are safe.
  - Concurrent `read_nowait` from N threads delivers each chunk exactly once.
  - Concurrent `write_nowait` calls on one stream parse in parallel outside any lock and are serialised only at the L6w carry splice and L6 commit (§7.3).
  - `clear()` is safe from any thread.

### 5.7 Panic containment

- **Lint policy.** sbcore uses `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)]` outside `cfg(test)`. The binding crate denies `unwrap_used`/`expect_used`.
- **Containment points:**

  | Location | Wrapped in | On panic |
  |---|---|---|
  | `Dispatch::act` | `catch_unwind` | The event is dropped and counted (`dispatch_panics`); always returns `None`. |
  | `LiveReader::next_packet`, `PcmDecoder::decode`, Opus readers | `catch_unwind` | The source is poisoned: `decode` returns silence once, then `next_packet` returns EOF and the track ends with `PlaybackEnded("internal_error")`. |
  | ConnWorker tick | `catch_unwind` | Every subscription closes with `ConnectionClosed("internal_error")`; the receive pipeline is rebuilt empty. |
  | Decode threads | `catch_unwind` | The stream closes with `"decode_error"`. |

- **Logging.** Every caught panic logs at ERROR once per site per minute and increments `stats()["panics"]`.

---

## 6. discord.py integration

These contract points were checked against `discord/abc.py:2137-2161`, `state.py:1665-1705`, `voice_client.py:98-188` and `guild.py:4539` at 2.7.0a5363:

- `Connectable.connect` constructs `cls(client, self)` and calls `state._add_voice_client` **before** `voice.connect(...)`. It runs `voice.disconnect(force=True)` **only** on `asyncio.TimeoutError`.
- discord.py schedules `on_voice_state_update` (for our own user only) and `on_voice_server_update` with `asyncio.create_task(logging_coroutine(...))`.
- `VoiceProtocol.cleanup()` calls `self.client._connection._remove_voice_client(key_id)`.

### 6.1 Class shape

```python
class SongbirdClient(discord.VoiceProtocol):           # py-src/discord/ext/songbird/client.py (pure Python)
    def __init__(self, client: discord.Client, channel: discord.abc.Connectable, *,
                 config: VoiceConfig | None = None) -> None
    @classmethod
    def with_config(cls, config: VoiceConfig | None = None, /, **overrides: Any
                    ) -> Callable[[discord.Client, discord.abc.Connectable], Self]   # functools.partial
```

- **Constructor.** `__init__` validates guild voice (`channel._get_voice_client_key()[1] == "guild_id"`, otherwise `TypeError`) and stores the config. It creates **no** native objects.
- **Passing a config.** discord.py calls `cls(client, channel)`, so a configuration is supplied with `cls=SongbirdClient.with_config(cfg)` or with the helper `songbird.connect(channel, config=…)`.
- **Composition, not inheritance.** `SongbirdClient` composes `native.Connection` rather than inheriting from it. A pure-Python class gives the correct MRO, real coroutines and accurate stubs.

### 6.2 Connect flow

```python
_pending_leaves: dict[int, asyncio.Task[None]] = {}   # module level: guild_id → best-effort leave of an aborted connect

async def connect(self, *, timeout: float, reconnect: bool, self_deaf: bool = False, self_mute: bool = False) -> None:
    loop = asyncio.get_running_loop()
    # An explicit set_self_mute()/set_self_deaf() made before connect() wins over discord.py's arguments (§6.7).
    self._self_mute = self_mute if self._explicit_mute is None else self._explicit_mute
    self._self_deaf = self_deaf if self._explicit_deaf is None else self._explicit_deaf
    try:
        if (prev := _pending_leaves.pop(self.guild.id, None)) is not None:
            # The leave op 4 of an earlier aborted connect must not land AFTER our join op 4.
            with contextlib.suppress(Exception):                           # incl. TimeoutError; not CancelledError
                await asyncio.wait_for(prev, 5.0)                          # on timeout it is cancelled: never sent late
        rsock, wsock = socket.socketpair()
        self._rsock = rsock; rsock.setblocking(False)
        bell = native.WakeBell(wsock.detach())                         # Rust owns the write end from here
        self._conn = native.Connection(bell, self.guild.id, self.client.user.id,
                                       self._config.to_native(), reconnect)   # Driver + Dispatch + ConnWorker
        self._pump = _Pump(loop, rsock, self._conn, self)              # holds weakref(self)
        self._conn.request_join(self.channel.id, self._self_mute, self._self_deaf)   # Idle → Joining; driver.mute()
        self._conn.set_users_bot([(m.id, m.bot) for m in self.channel.members])
        # The ONLY op 4 of a join: channel + flags together. No deafen()/mute() update before joining.
        self._sent_flags = (self._self_mute, self._self_deaf)
        await self.guild.change_voice_state(channel=self.channel, self_mute=self._self_mute,
                                            self_deaf=self._self_deaf)
        try:
            async with asyncio.timeout(timeout):
                st = await self._pump.wait_status(lambda s: s.state in ("connected", "closed"))
        except TimeoutError:
            raise ConnectTimeout(timeout) from None                    # subclass of TimeoutError
        if st.state == "closed":
            raise ConnectFailed(st.close_reason or "closed", st.close_code)
    except BaseException:                                              # incl. CancelledError, ConnectFailed
        self._abort_connect()                                          # sync: close + cleanup + best-effort leave
        raise
```

`_abort_connect()` is synchronous and safe to call after a cancellation:

1. `self._conn.close("connect_aborted")` runs T0–T9 if the connection exists. Otherwise it closes whichever socket halves are still owned by Python.
2. A best-effort `guild.change_voice_state(channel=None)` is scheduled as a background task with a 5 s timeout. This matters because the gateway may already have put the bot in the channel.
   - The task is stored in `_pending_leaves[guild.id]`, replacing any older entry, and removes itself when done. Failures are logged.
   - The next `connect()` for that guild awaits it first, for at most 5 s, before sending its join op 4. If user code retries immediately after a `ConnectTimeout`, the stale leave can therefore never arrive after the new join and disconnect the new session.
   - The leave is skipped when `disconnect()` already sent one (`self._leave_sent`), for example when a user disconnects during `connect()`.
3. `self.cleanup()` runs, which removes the client from `_voice_clients`.
4. `_finalized` is set.

When discord.py then calls `disconnect(force=True)` for a `TimeoutError`, our `disconnect` sees `_finalized` and returns immediately. That makes "Already connected" after a failed connect impossible, whatever the failure was.

### 6.3 Gateway update forwarding

```python
async def on_voice_state_update(self, data: GuildVoiceStatePayload, /) -> None:
    if self._conn is None: return
    ch = utils._get_as_snowflake(data, "channel_id")
    self._conn.on_voice_state(data["session_id"], ch)      # O(µs): L2 step, then actions (maybe driver.connect)
    if ch is not None and ch != self.channel.id and (moved := self.guild.get_channel(ch)) is not None:
        self.channel = moved                               # moderator move / move_to
    self._gateway_self_mute = data.get("self_mute", False); self._gateway_self_deaf = data.get("self_deaf", False)

async def on_voice_server_update(self, data: VoiceServerUpdatePayload, /) -> None:
    if self._conn is None: return
    self._conn.on_voice_server(data["token"], data.get("endpoint"))   # endpoint None = server reallocating: wait
```

- **Ordering.** Either update may arrive first; partial information is merged (§4.5).
- **Secrets.** The token is never logged. `session_id` is logged only as a length, and the endpoint only as a host.

### 6.4 How Rust requests voice-state changes without holding Python objects

Rust never calls Python. When the state machine needs the gateway, it sets a `Control` slot in `CtrlQueue` and rings `WAKE_CTRL`. `_Pump._on_ctrl()` runs on the loop. It holds only `weakref(SongbirdClient)` and a strong reference to the native connection, which holds nothing back.

**`CtrlQueue` is a set of slots, not a queue, so nothing can be dropped however long the loop stalls.** Every control is either level-triggered or coalesces to its latest value. `take()` returns the pending slots in the order of the table below, and clears them.

| `Control` (slot kind) | Python action (never awaited inline in the pump; long actions run as tracked tasks) |
|---|---|
| `ChannelMoved{channel_id}` (latest value wins) | `self.channel = guild.get_channel(channel_id) or self.channel` |
| `ClassifyUsers` (flag) | For each `uid` in `conn.take_unclassified_users()`: `m = guild.get_member(uid)`, then `conn.set_users_bot([(uid, bool(m and m.bot))])`. This reads the cache only; no HTTP. |
| `Rejoin{attempt, channel_id}` (latest value wins; dropped if `Closed` is set) | Task: `await guild.change_voice_state(channel=None)`, then `await guild.change_voice_state(channel=discord.Object(channel_id), self_mute=…, self_deaf=…)`. Rust is already in `Rejoining`, so the intermediate `VoiceState(None)` is not treated as a kick. |
| `Closed{reason}` (sticky) | `self._finalize(reason)`: close children, `cleanup()`, resolve all waiters. |

**Track completion is not a control.** The housekeeper rings `WAKE_TRACKS` whenever a track's `ended` resolves, and T8 rings it at close. On that wake, the pump re-checks `NativeTrack.ended` for every handle that has `wait_ended()` waiters, so completion is level-triggered and `wait_ended()` resolves on every termination path, even after a long loop stall. `TrackEnded` still goes to `EventStream`s, where drops are reported as `Lagged`.

If the weakref is dead (the client was collected), the pump calls `conn.close("dropped")` and exits.

### 6.5 Disconnect and cleanup

```python
async def disconnect(self, *, force: bool = False) -> None:
    conn = self._conn
    if conn is None or self._finalized:
        self.cleanup(); return                                  # idempotent (discord.py may call it again)
    conn.request_leave()                                        # VoiceState(None) ⇒ Closed(Requested), not "kicked"
    self._leave_sent = True                                     # _abort_connect() must not send a second leave
    try:
        async with asyncio.timeout(5.0):
            await self.guild.change_voice_state(channel=None)
    except Exception:
        if not force:
            _log.warning("voice leave (op 4) failed; tearing down locally", exc_info=True)
    finally:
        conn.close("requested")                                 # sync T0–T9 FIRST (cancellation cannot skip it)
        self.cleanup()                                          # always
        await self._pump.closed(timeout=2.0)                    # wait for pump EOF; cancellation-safe
```

- **`disconnect()` never raises** for gateway errors. Teardown is complete either way.
- **`cleanup()`** is idempotent (`_finalized`). It closes every child in `self._children` (a WeakSet), calls `conn.close("cleanup")` and then `super().cleanup()`.

### 6.6 Move, reconnect, external moves

- **`move_to(channel, *, timeout=30.0)`**
  - `None` means `disconnect()`.
  - Otherwise:
    1. `conn.request_move(channel.id)` puts the state machine in Joining with the new target.
    2. Send op 4 with the new channel and the current flags.
    3. `await` the status `connected` **with** `channel_id == channel.id`. A timeout raises `ConnectTimeout`; the connection keeps trying in its current state.
  - Streams, playbacks, events and the receive timeline survive the move. Synthetic silence covers the gap and is marked in `chunk.gap_ms`.
- **`reconnect(*, timeout=30.0)`** (graft C) forces a fresh voice session.
  1. `conn.begin_rejoin()` enters Rejoining, so the `channel=None` echo is ignored.
  2. Send op 4 `None`, then op 4 `(channel, flags)`.
  3. Wait for `connected` with a new `generation`.

  This is the documented recovery for DAVE stalls (songbird #310) and a dead UDP-rx task (#315). `auto_recover` triggers it automatically through §4.5.
- **External moves** (a moderator drags the bot) arrive as `VOICE_STATE_UPDATE` with a new channel. The same transition applies, and `self.channel` is updated.

### 6.7 `self_mute` / `self_deaf` without pre-join gateway traffic

- **Join flags.** They travel only in op 4s that Python sends: the join in `connect()`, each rejoin, `move_to`, and the flush described below. Every op 4 the client sends records its flags in `_sent_flags`.
- **Precedence (one rule).** `set_self_mute`/`set_self_deaf` store an explicit value in `_explicit_mute`/`_explicit_deaf` (initially `None`). An explicit value set before `connect()` (on a directly constructed client) **wins** over discord.py's `self_mute`/`self_deaf` arguments. Otherwise `connect()` uses its arguments. discord.py runs `_add_voice_client` and then `voice.connect(...)` with no await in between, so through `channel.connect()` the first point where user code can call `set_self_*` is during `connect()`.
- **`await set_self_mute(m)` / `await set_self_deaf(d)`:**
  - They always store the flag. If a connection exists they also call `conn.set_self_flags(m, d)` (`SetFlags`, §4.5). That calls `Driver::mute(m)` (stop sending audio), mirrors `m` into the send-mute atomic of every live source (§7.8), and records the flags for rejoins.
  - They send op 4 `(current channel, flags)` **only when the state is `connected`**. In `idle`, `joining`, `connecting` and `reconnecting` they send nothing. **They never raise `NotConnected` and never send `channel=None`.**
  - **Flush on connect.** Whenever the pump observes the state becoming `connected` (on `WAKE_CONN`) and the current flags differ from `_sent_flags`, it sends exactly one op 4 `(channel, flags)`. A flag changed during `connect()` therefore reaches the gateway as soon as the join completes.
- **Deaf is cosmetic for the gateway.** Receive keeps delivering whatever Discord forwards.

### 6.8 `reconnect`, `force` and voice close codes

- **`reconnect=True`** gives `Retry{retry_limit: Some(driver_retries)}` plus gateway-level rejoin with `rejoin_attempts` attempts.
- **`reconnect=False`** gives `Retry{retry_limit: Some(NonZeroU8::MIN)}` and no rejoin (terminal transitions close).
- **`force=True`** suppresses gateway errors in `disconnect`. Teardown happens regardless.

| Code / event | songbird 0.6.1 behaviour | Our policy |
|---|---|---|
| 4015 (server crash), 4009 (session timeout), WS IO errors | Resumed internally with no event. On success: `DriverReconnect`. On failure: `DriverDisconnect{Reconnect, _}`. | Streams continue. `Reconnected{gap_ms}`, or a rejoin, or close (§4.5). |
| 4014 (disconnected: moved, kicked or channel deleted) | Final: `DriverDisconnect{Runtime, WsClosed(4014)}`. Quashed if the attempt is stale. | **AwaitingMove (5 s grace).** The gateway update decides between move and kick. No auto-disconnect on the code itself. |
| 4006 (session no longer valid), other final codes, `Io`/`TimedOut` at Runtime | Final | `reconnect=True`: rejoin with backoff. Otherwise Closed(Lost). |
| 4016 (unknown encryption mode), 4017 (DAVE protocol required) | Final | Closed(SessionInvalid(code)) without rejoin: a rejoin cannot fix a protocol mismatch. The ERROR log names the songbird version. |

### 6.9 Parity with `discord.VoiceClient`

- **Provided:** `guild`, `user`, `channel`, `is_connected()`, `move_to`, `disconnect`, `stop_all()`, and `play()` as a **synchronous** call returning a `TrackHandle`.
- **Not provided:**
  - `latency` and `average_latency` (songbird exposes no heartbeat RTT);
  - `source`, `is_playing`, `pause`, `resume` (use `TrackHandle`/`TrackQueue`);
  - `play(AudioSource, after=)`.

---

## 7. Playback (send) design

### 7.1 The mixer contract

These are the rules for every live input we hand to songbird. They were verified in `driver/tasks/mixer/mix_logic.rs:50-260`.

1. **Every streaming input is `Input::Live(LiveInput::Parsed(Parsed{format, decoder, track_id: 0, meta, supports_backseek}), None)`.** songbird treats it as Ready immediately: no pool, no Preparing state, no `Compose` (`None`).
2. **`FormatReader::next_packet` reads atomics only.**
   - It returns `Err(symphonia::core::errors::Error::IoError(io::ErrorKind::UnexpectedEof.into()))` **iff** the source should end: Abort, Drain with an empty queue, poisoned after one silent tick, or connection closed. Any `Err` ends the track (`mix_logic.rs:103-106`).
   - Otherwise it returns `Packet::new_from_boxed_slice(0, ts, 960, Box::default())`. An empty `Box<[u8]>` does **not** allocate. `ts` advances by 960 each call.
   - songbird counts frames from the **decoded** buffer (`pkt_frames = source_packet.frames()`, `mix_logic.rs:149`), so an empty packet is legal.
3. **`Decoder::decode` pulls audio and always yields exactly 960 frames** of 48 kHz `f32` in a preallocated `AudioBuffer<f32>` (1 or 2 planes). It never returns `Err` and never returns 0 frames, which would spin `mix_logic.rs:151`.
4. **`CodecParameters`:**
   - `codec = CODEC_TYPE_PCM_F32LE`, so Opus passthrough never engages for PCM;
   - `sample_rate = 48000`;
   - channels `FRONT_LEFT` or `FRONT_LEFT | FRONT_RIGHT`;
   - `time_base = 1/48000`;
   - `max_frames_per_packet = 960`.

   With 48 kHz in, songbird's no-resample `mix_over_ref` path runs. That bypasses songbird's internal `FftFixedOut`, which is −10 dB at 0.8·Nyquist for 24 kHz input.
5. **`seek` is a no-op** `Ok(SeekedTo{track_id: 0, required_ts: ts, actual_ts: self.ts})` for live sources (`supports_backseek: false`). Memory PCM and Opus sources implement real seeks.
6. **Probe once per input, on the creating thread.** `ProbedMetadata` has a private field, no public constructor and no `Clone` (symphonia-core 0.5.5 `probe.rs:161-178`), so every `Parsed` needs its own probe. `FormatReader` also requires `into_inner(self: Box<Self>) -> MediaSourceStream` (`formats.rs:213`). When a live or memory input is created, on a Python thread or a decode thread and never on the mixer, we probe the 16-byte header `b"SbirdRaw" + le32(48000) + le32(ch)` with `songbird::input::codecs::get_probe().format(&Hint::new(), mss, &FormatOptions::default(), &MetadataOptions::default())?`. This is the header that `RawReader` recognises (`input/codecs/raw.rs:45-89`). The result supplies `.metadata` for `Parsed.meta`, and `.format.into_inner()` supplies the drained `MediaSourceStream`, which the reader stores and returns from its own `into_inner`. That costs one 64 KiB MSS ring per input, the minimum symphonia allows (`buffer_len` must be a power of two > 32 KiB), allocated at creation and counted in §10 T4.
7. **No allocation on the PCM pull path.** Every buffer is preallocated at construction on a Python thread, including the `TickResampler`, whose rubato constructor logs. **Decision (must-fix):** A's `Box<[i16]>` → `bytemuck::cast_slice_box` → `Box<[u8]>` is forbidden. `try_cast_slice_box` returns `AlignmentMismatch` (bytemuck `allocation.rs:206`), so the packet would end every track on its first pull. The empty-packet-plus-decoder-pull design removes the per-tick buffer entirely.
8. **Every body is wrapped in `catch_unwind`**, and every lock acquisition is `try_lock`.

### 7.2 `PcmQueue`: the realtime engine behind `PlaybackStream`

`PcmQueue` (and `EncodedStream`'s internal queue) stores audio **at the input rate** as interleaved `i16`. **Decision (graft B/D, resolving a must-fix):** resampling to 48 kHz happens on the mixer side, in a continuous per-track `TickResampler`. Positions and tags are therefore exact in input-rate frames, and no write-side resampler tail can leak across gaps, `clear()` or idle.

```rust
pub struct PcmQueue {
    id: u32,
    fmt: Format,                          // rate ∈ {8k,16k,24k,48k}, channels ∈ {1,2}, encoding = wire format of writes
    lim: PlaybackLimits,                  // cap_frames, max_gap_ms, idle_ticks, prebuffer_frames, prebuffer_ticks,
                                          //   output_latency_ms, auto_pause, when_disconnected
    writer: parking_lot::Mutex<Carry>,    // L6w: partial-frame carry only { bytes: [u8; 8], len: u8, gen: u64 }
    inner: parking_lot::Mutex<QueueInner>,     // L6
    st: QueueAtomics,                     // lock-free mirrors for the mixer, housekeeper and Python properties
    send_muted: Arc<AtomicBool>,          // one per connection; mirrors set_self_mute (§7.8)
    wake: WakeToken,                      // space / drain / idle / end wake-ups (rung by Python threads or housekeeper)
    track: OnceLock<Box<dyn TrackControl>>,
}
struct QueueInner {
    segs: VecDeque<Seg>,                  // capacity grows lazily (starts at 16)
    queued_frames: u64,                   // incl. non-materialised silence
    anchor: Option<Anchor>,               // wall-clock gap-fill anchor { remote_ms: i64, wall: Instant }
    timed_end_ms: Option<i64>,            // max(at_ms + duration) over accepted timed writes (source timeline)
    gap_reject: Option<GapReject>,        // clear(reject_until_gap_ms=…): { cut_end_ms: i64, gap_ms: u32 } (§7.7)
    tags: TagTable,                       // ≤ 256 tags {name: Arc<str>, written, played, last_pull: u64 (pull index),
                                          //   last_write: u64, open: bool, ended: bool}; eviction rule in §7.7
    rejected: RejectSet,                  // ≤ 256 names rejected by reject_late (FIFO eviction)
    hold_until: Option<Instant>,          // clear(hold_ms=…)
    gate: Prebuffer,                      // { armed: bool, ticks_left: u32 } (§7.3e, §7.4)
    pull_epoch: (u64, Instant),           // (pulls, when the housekeeper saw them advance): wall time of any pull index
    generation: u64,                      // bumped by clear(); mirrored in st.generation
    closed: Option<CloseMode>,            // Drain | Abort (mirrored in st.end_flags)
    space_wanted: u64, drain_waiting: bool, idle_waiting: bool, active: bool,
    paused_user: bool, paused_idle: bool /* true at open: live sources start paused */, link_up: bool,
}
enum Seg { Pcm { data: Box<[i16]>, off: u32, tag: Option<TagId> }, Silence { frames: u64 } }
struct QueueAtomics {
    pulls: AtomicU64, position: AtomicU64, played_real: AtomicU64, queued: AtomicU64, written: AtomicU64,
    underrun: AtomicU64, gap_filled: AtomicU64, rejected: AtomicU64, discarded: AtomicU64, contended: AtomicU64,
    space_epoch: AtomicU64, last_real_pull: AtomicU64, generation: AtomicU64, volume_bits: AtomicU32,
    carry_len: AtomicU8 /* snapshot for lock-free parsing, §7.3 */, tags_evicted: AtomicU64,
    end_flags: AtomicU8 /* DRAIN | ABORT | POISON | CONN_CLOSED; set BEFORE taking L6 */, reader_gone: AtomicBool,
}
```

### 7.3 Write path (Python thread)

**Binding crate (`src/convert.rs`, I8):**

| Input type | Handling |
|---|---|
| `bytes` | Borrowed as `&[u8]`, zero copies. |
| `str` | Rejected with `TypeError` (`str` is never audio; decode text-encoded audio in user code first). |
| any other buffer-protocol object (`bytearray`, `memoryview`, numpy, `array`) | Must be C-contiguous. The **format code is checked against the stream encoding**: `'B'`, `'b'` and `'c'` (raw bytes) are accepted for every encoding; `'h'`/`'<h'` (or `'='`-prefixed) only for `pcm16le`; `'f'`/`'<f'` only for `f32le`. Anything else raises `InvalidAudio`, for example a float32 array written to a `pcm16le` stream, a big-endian `'>i2'` buffer or an `'i'` array. Copied once into an owned `Vec<u8>` (`try_reserve` → `MemoryError`), and the `PyUntypedBuffer` is released **before** calling sbcore. |
| Arrow arrays (`__arrow_c_array__`, `arrow` feature) | Primitive arrays without nulls whose type matches the encoding (Int16 ↔ pcm16le, Float32 ↔ f32le, UInt8 ↔ any) are copied. The import is dropped while still attached, because pyarrow's release callback needs the interpreter. |

**`PcmQueue::write(data: &[u8], WriteMeta{tag, at_ms}, now)`** (sbcore). It is all-or-nothing.

1. **Pre-checks and parsing, no lock:**
   - closed (`st.end_flags`) → `StreamClosed(reason)`;
   - tag longer than 128 bytes → `InvalidAudio`;
   - connection closed → `StreamClosed("connection_closed")`;
   - **size cap before any allocation.** `len` is compared with the cap in bytes (`cap_frames·frame_bytes`). A larger write raises `LimitExceeded` before anything is allocated or parsed.
   - **Parse** using a snapshot `c = st.carry_len`: skip `k = (frame_bytes − c) % frame_bytes` head bytes (they complete the carried frame), and convert the whole frames of `src[k..]` into an exactly sized `Box<[i16]>`. Conversions: pcm16le is an LE copy, f32le is clamp + round, and µ-law/A-law use the `ULAW_DECODE`/`ALAW_DECODE` tables, bit-exact with Sun g711.c. Keep any trailing partial frame as the **tentative** new carry. **Odd byte counts never raise.**
2. **Lock L6w (O(1)).**
   - Drop the stored carry if its `gen` ≠ `st.generation`: a `clear()` happened, and pre-clear bytes must never prefix post-clear audio.
   - If the carry length differs from the snapshot `c` (two threads writing one stream at once), unlock and redo step 1's parse. That is bounded to two retries; the third attempt parses while holding L6w. Concurrent writers to one stream are safe but have no defined interleaving.
   - Splice the one head frame from carry + `src[..k]` (≤ 7 bytes).
3. **Lock L6 (O(1) or O(tags touched)):**
   - a. **Rejection.** The write is rejected if any of these holds: `now < hold_until`; the tag is in `rejected`; `gap_reject` matches (§7.7); or `when_disconnected == discard` and `!link_up`. A write that races with `clear()` is judged against the post-clear state. On rejection: `rejected += frames`, reset the carry, return `Rejected` (silently; counted in `rejected_ms`).
   - b. **Gap** (§7.6): compute `gap_frames` from `at_ms` with checked arithmetic. If `gap + frames > cap`, the write is treated as a **discontinuity**: re-anchor, `gap = 0`.
   - c. **Capacity.** If `queued + gap + frames > cap`: set `space_wanted = min(gap + frames, cap)`, restore the carry, unlock, raise `PlaybackFull(needed_ms, free_ms)`. **Nothing is consumed.**
   - d. Push `Seg::Silence{gap}` if `gap > 0` (not materialised; counted in `gap_filled`; **excluded** from tag accounting). Push `Seg::Pcm{data, tag}`. Update `tags[tag].written` and `last_write` (opening the tag if needed, §7.7), `timed_end_ms`, `written` and `queued`, and commit the carry with the current generation.
   - e. **Start.** If the write finds `queued == 0` at a segment start (never written, idle since the last real pull, or cleared since the last write) and `lim.prebuffer_frames > 0`, arm the prebuffer gate with `ticks_left = prebuffer_ticks` (§7.4). If `paused_idle`: call `track.play()` (a flume send) and clear the flag. **Every live source starts with `paused_idle = true`** (§4.9), so this is also how the first write starts playback, within ≤ 1 tick. Both happen under L6, ordered with the housekeeper's `pause()`.
4. **Unlock L6 and L6w. Return.**

`clear()`, `close()` and T5 never take L6w. `close()` sets `st.end_flags` before taking L6, so a writer that is parsing aborts at its next check (step 2 or 3) instead of the closer waiting for it.

**`await write()`** loops: try `write_nowait`; on `PlaybackFull`, `await pump.wait(wake_id)`. The housekeeper rings once the mixer has freed `space_wanted`, with ≤ 20 ms latency. Because `space_wanted ≤ cap`, an empty queue always satisfies it. The optional `timeout=` raises `TimeoutError`. Cancellation can only happen between attempts.

### 7.4 Pull path (mixer thread)

```text
LiveReader::next_packet():   if p.should_end() { Err(EOF) } else { Ok(Packet::new_from_boxed_slice(0, ts, 960, Box::default())) }
PcmDecoder::decode(_pkt):
  n = fmt.rate.frames_per_tick(); src = &mut self.in_i16[..n*ch]; src.fill(0); real = 0; consumed = 0
  if p.send_muted.load(Relaxed) { }                                // self-muted: hold position, consume nothing (§7.8)
  else match p.inner.try_lock():
    None    => st.contended += 1                                   // never wait; one tick of silence
    Some(g) => if g.paused_user { }                                // silence, consume nothing
               else if g.gate.armed && g.queued_frames < prebuffer_frames && g.gate.ticks_left > 0
                    { g.gate.ticks_left -= 1 }                     // prebuffering: silence, consume nothing
               else { g.gate.armed = false;                        // gate opens (or was open)
                      (consumed, real) = g.pop_into(src) }         // Silence segs → zeros (no copy), Pcm → memcpy,
                                                                  //   per-tag played += …, tag.last_pull = pulls
               g.queued_frames -= consumed
  st.pulls += 1; st.position += n; st.played_real += real
  if active: st.underrun += n - consumed
  if consumed > 0 { st.space_epoch += 1 }; if real > 0 { st.last_real_pull = pulls }
  if st.generation != self.seen_gen { resampler.reset(); ramp_in = 120 frames (2.5 ms); seen_gen = … }
  i16 → f32 (/32768) into in_f32;  TickResampler::process_tick(in_f32, out48)  (copy if 48 kHz)   // exactly 960*ch
  gain: target = f32::from_bits(volume_bits); linear 5 ms ramp on change; declick fade-in after clear
  buf.clear(); buf.render_reserved(Some(960)); write planes; Ok(buf.as_audio_buffer_ref())
```

- **Prebuffer gate.** It is armed by the first accepted write at a segment start (§7.3e). It opens when **any** of these holds: `queued ≥ prebuffer_frames`; `prebuffer_ticks = ⌈prebuffer_ms/20⌉` gated pulls have passed since it was armed, so a reply shorter than `prebuffer_ms` still plays after at most `prebuffer_ms`; or `drain()`, `aclose(drain=True)` or `end_item()` is called, each of which clears `gate.armed` under L6. `PlaybackSpec::validate` requires `prebuffer_ms ≤ max_buffer_ms/2`, so the size condition is always reachable.
- **No `Notify`, doorbell, flume or `TrackHandle` call happens here.** Wake-ups, events and auto-pause belong to the housekeeper (§7.5).
- **`LiveReader::drop` and `PcmDecoder::drop`** store `reader_gone = true` and drop the `DoneGuard` (atomic stores only). This runs inline on the mixer or Idle task for `SetTrack(None)`, or on the DisposalThread for natural ends.

### 7.5 Housekeeper

The housekeeper runs on the ConnWorker (RX runtime), once per tick while live sources exist; otherwise it costs no wake-ups. For each `Weak<dyn LiveSource>`:

- **Space.** If `space_wanted > 0` and free frames ≥ `space_wanted`: clear it and ring `wake`. Writers re-try.
- **Drain.** If `drain_waiting` and `queued == 0` and a pull has happened since: ring.
- **Pull epoch.** If `st.pulls` advanced since the last housekeeping tick, set `pull_epoch = (pulls, now)` under L6. The wall time of any earlier pull index `p` is then `epoch.1 − 20 ms·(epoch.0 − p)`, and that estimate stays correct while the track is paused or parked (§7.7 `heard_ms`). The mixer never reads a clock.
- **Idle.** If `queued == 0` for `idle_ms` (default 250) after real audio: mark inactive, **close every open tag** (playback went idle, §7.7), `emit(PlaybackIdle{playback_id, played_ms})`, and ring if `idle_waiting`.
  - If `auto_pause`, lock L6. If the queue is still empty, set `paused_idle = true` and call `track.pause()`. songbird then sends 5 Opus silence frames and stops RTP, so the speaking ring turns off. Resuming on the next write costs ≤ 1 tick.
  - **Never-written sources** need no housekeeping to stay silent: they are created paused (`paused_idle = true`, §4.9), so an opened but unwritten playback produces no pulls and no RTP.
- **Started.** Real audio pulled after idle → `emit(PlaybackStarted{playback_id})`.
- **End.** `reader_gone` → `emit(PlaybackEnded{playback_id, reason})`, ring (resolves `aclose(drain=True)`), deregister.
- **Link changes.** These arrive as `WorkerCmd::Link{up, ssrc, seq}`, which comes from the `RxLink` action; a `seq` older than the last applied one is ignored (§4.6). Set `link_up`. For `when_disconnected == "discard"`, clear the queue on link-down, counted in `discarded_ms`.
- **Encoded streams.** Notify the decode thread's space condvar. **Opus streams.** Refill the silent-frame reserve (§7.9) and ring writers. Apply the same idle rule: after `idle_ms` with an empty `OpusQueue`, pause the track (it starts paused too), so an unfed Opus stream does not send `SILENT_FRAME` RTP indefinitely. The next `send` resumes it.

The housekeeper also polls `TrackRegistry` for dropped or ended tracks, finalises them (§7.10) and rings `WAKE_TRACKS`. Mixer and disposal threads cannot notify, so polling is how completion is observed.

### 7.6 Gap filling from source timestamps (`at_ms`)

**Decision (graft C, resolving a must-fix).** The anchor is **wall-clock based**. songbird does not pull a paused or parked track (`mixer/mod.rs:830`), so a frame-based playhead freezes during idle-pause or a disconnect. That frozen playhead would insert stale silence for time that has already passed, adding up to `max_gap_ms` of latency after every idle period.

```text
earliest(now) = now + queued_frames / rate        // when the next written sample would start playing
first timed write (anchor None):   anchor = (at_ms, earliest(now)); gap = 0
later timed write:                 Δ = at_ms.checked_sub(anchor.remote_ms)          // i64; None or |Δ| > 86_400_000
                                                                                     //   → re-anchor, no silence
                                   desired = anchor.wall.checked_add(Δ ms) (negative Δ: checked_sub)
                                   gap_ms  = desired − earliest(now)   (saturating; None → re-anchor)
  gap_ms > max_gap_ms              → re-anchor (at_ms, earliest(now)); no silence (discontinuity)
  gap + frames > cap               → re-anchor; no silence (a gap that can never fit is a discontinuity)
  40 < gap_ms ≤ max_gap_ms         → insert Seg::Silence(gap_ms) (not materialised; excluded from tags/played)
  −40 ≤ gap_ms ≤ 40                → append (jitter tolerance = 2 ticks; no micro-silences mid-speech)
  gap_ms < −40 (late / overlap)    → append immediately (NEVER trim or drop speech); re-anchor (at_ms, earliest(now))
write without at_ms                → append; anchor untouched
clear() / link-down with "discard" → anchor = None
```

`at_ms` is any Python `int` that fits in `i64` (larger → `OverflowError` at extraction). No caller-supplied value can overflow the anchor arithmetic or panic `Instant` arithmetic. `PlaybackSpec::validate` requires `max_gap_ms ≤ max_buffer_ms/2`, so an inserted gap plus at least half a buffer of audio always fits an empty queue.

**Required test (§13.3):**

1. A 1000 ms write at `at_ms = 0` is played out.
2. 250 ms of idle follows, and the track is auto-paused.
3. The next write has `at_ms = 2500` and arrives at wall time `t0 + 2500 ms`.

Expected: **≤ one tick** of silence is inserted. If that same write instead arrives early, at `t0 + 1100 ms`, about 1400 ms of silence is inserted, so it starts on time.

### 7.7 `clear()` (interruption), tags and positions

**Tags.** A tag is an application-defined segment id (a sentence, a message, a reply: whatever unit the application needs positions for). It is **open** from its first accepted write until one of these happens:

- `end_item(tag)`, called once the application has written the whole segment. It closes the tag, sets `ended`, and releases the prebuffer gate (§7.4).
- `clear()`.
- The stream's next Active → Idle transition (§7.5).

An open tag stays open across underruns shorter than `idle_ms`, so a segment that is still streaming but momentarily has nothing queued is still "in play". Apps that never call `end_item` still get bounded open sets, because tags close when playback goes idle.

**Tag-table overflow** (`MAX_TAGS = 256`). A write with a new tag while 256 are live evicts one entry, chosen in this order:

1. the oldest closed tag;
2. otherwise, the open tag with the oldest `last_write`.

The eviction is counted in `stats().tags_evicted`, and a later write with an evicted tag starts a fresh entry. Writes never fail because of tags.

`clear(*, reject_late=True, hold_ms=0, reject_until_gap_ms=None) -> ClearResult` is synchronous, O(tags), and callable from any thread. It never takes L6w (§7.3):

1. Lock L6.
2. Build `items`: **every open tag, plus every tag pulled within the last `output_latency_ms`** (wall time via `pull_epoch`). Each item is `ItemProgress{tag, written_ms, played_ms = min(played, written), heard_ms, complete}`, where `complete = ended && played == written`.
3. Set `discarded += queued_real`. **Swap `segs` out** (freed after unlock). Set `queued = 0`, `space_wanted = 0`, `anchor = None`, `gate.armed = false`. Increment `generation`, which also invalidates the writer's partial-frame carry (§7.3 step 2). Close every open tag.
   - Capacity is released **immediately**, even while the mixer is parked or idle-paused. The next segment's `write_nowait` can never hit `PlaybackFull` because of audio that was discarded.
4. **Late-write rejection.** Three options, which can be combined:
   - **`reject_late`** (default `True`). Add **every open tag** and every item tag to `rejected` (FIFO, 256). Late writes of an interrupted segment are then dropped, even if the segment had nothing queued at the moment of the clear.
   - **`reject_until_gap_ms=N`**, for timed sources (`at_ms`) without tags. If `timed_end_ms` is known, set `gap_reject = {cut_end_ms: timed_end_ms, gap_ms: N}`.
     - A later timed write with `at_ms < cut_end_ms + N` continues or repeats the interrupted audio. It is rejected, and `cut_end_ms` grows to `max(cut_end_ms, at_ms + duration)`.
     - The first timed write with `at_ms ≥ cut_end_ms + N` is accepted and ends the window. This suits sources whose timeline leaves a gap before each new segment.
     - `N` must exceed the source's pauses inside one segment (e.g. 250 ms). A longer pause inside the interrupted segment ends the window early.
     - Untimed writes are unaffected.
   - **`hold_ms`**, a fallback for untimed writes. It drops **all** writes until `now + hold_ms`. This is a wall-clock guess: too short and the tail resumes mid-sentence (the anchor was reset, so it plays at once); too long and it eats the start of the next segment. Prefer `reject_late` or `reject_until_gap_ms` whenever the source supplies segment ids or timestamps.
5. Unlock, drop the old segments, and ring `wake` if a writer was waiting for space.

The cut becomes audible at the next pull. Audio already pulled (≤ 1 tick) and the packet in flight (≤ 1 tick) still play, so expect **≤ 40 ms** plus network.

**Positions.** All are monotonic since open, in milliseconds, computed as `frames·1000/rate` floored. "Since the last real pull" is wall time derived from `pull_epoch` (§7.5), so it keeps advancing while the track is paused, idle-paused or parked.

| Property | Meaning |
|---|---|
| `written_ms` | Accepted real audio (excludes gap silence and rejected writes). |
| `played_ms` | Real audio handed to the encoder (excludes underrun and gap silence, and audio held back while self-muted, §7.8). |
| `heard_ms` | `played_ms − clamp(output_latency_ms − since_last_real_pull, 0, played_ms)`, an estimate. It converges to `played_ms` once `output_latency_ms` has passed after the last real pull. `output_latency_ms` defaults to 40 (range 0–1000). |
| `position_ms` | Playhead including silence, advancing per pull while the mixer is live. |
| `buffered_ms` | Queued, including inserted gap silence. |
| `underrun_ms`, `gap_filled_ms`, `rejected_ms`, `discarded_ms`, `contended_ticks`, `tags_evicted` | Counters. |
| `item(tag)` | `ItemProgress`, with `played_ms` clamped to `written_ms` (a consumer never learns that more was played than written). Per-item `heard_ms = played − clamp(output_latency_ms − since(tag.last_pull), 0, played)`, so latency compensation applies only to audio pulled within the last `output_latency_ms` of wall time. `complete` is true once `end_item(tag)` was called and everything written has been played. |

`played_ms` means "handed to the Discord encoder". Network and listener jitter-buffer delay (about 40–100 ms) lie downstream and cannot be observed. The documentation says so.

### 7.8 Flush, drain, close, pause, volume

- **Flush semantics.** No resampler tail is held on the write side. At most one partial frame (< `frame_bytes` bytes) is carried. There is therefore no separate `flush()`; **`await drain(timeout=None) -> bool` is the flush**.
  - It releases the prebuffer gate (§7.4), arms `drain_waiting` and waits until `queued == 0` and a pull has followed.
  - It returns `False` on timeout or when the stream or connection closes.
  - While the link is down, or while self-muted, it keeps waiting, because the buffered audio plays after reconnect or unmute.
- **`await wait_idle()`** resolves at the next Active → Idle transition: the end-of-playback signal for sources that have no explicit end-of-segment event.
- **`close()`** is synchronous and idempotent.
  - It sets `closed = Abort` and the end flag, frees the segments now, and calls `track.stop()` (flume; songbird fires `TrackEvent::End`).
  - The reader returns EOF at its next pull, or the track is dropped at teardown if parked.
- **`await aclose(*, drain=False, timeout=None)`.**
  - With `drain=True`: release the prebuffer gate, set `closed = Drain` (the reader returns EOF once empty), then await `reader_gone` with deadline `timeout or buffered_ms + 2 s`. On timeout it escalates to Abort.
  - `__aexit__` is `aclose(drain=False)`.
- **`pause()`/`resume()`** set `paused_user` under L6. While set, the gate emits silence without consuming. They also call `track.pause()`/`track.play()`, so RTP stops while paused.
- **Self-mute.** songbird still pulls every track while muted and only afterwards zeroes the mix (`if self.muted { mix_len = MixedPcm(0) }` follows `mix_tracks`, `mixer/mod.rs:529-539`). Left alone, a `PlaybackStream` would be consumed and counted as played and heard without being sent; `drain()` would complete; and `heard_ms` would report unheard audio as heard. So `set_self_flags` mirrors the send-mute flag into a per-connection `send_muted: Arc<AtomicBool>` that every live PCM and Opus source reads. While it is set, the decoder emits silence **without consuming**: the same gate as a user pause, checked before `try_lock`. `played_ms`/`heard_ms` hold, and the audio plays after unmute. Use `clear()` to discard it instead. Memory and file tracks (`play()`) keep advancing silently, as songbird mixes them and then discards the mix; this is documented.
- **`volume`** is in the range 0.0–4.0. It is an atomic `f32` applied in our decoder with a 5 ms ramp, so it works while parked. songbird's `TrackHandle::set_volume` only reaches live mixers.

### 7.9 Other inputs

| Python API | sbcore type | songbird input | Mixer work | Seek/loop | Notes |
|---|---|---|---|---|---|
| `vc.open_playback(fmt, …)` → `PlaybackStream` | `PcmQueue` + `LiveReader` + `PcmDecoder` | Parsed, `supports_backseek: false` | `try_lock` + memcpy + resample | no (`TrackError`) | Realtime PCM written as it arrives (§7.2–7.8). Starts paused until the first accepted write (§4.9). |
| `vc.open_encoded_stream(max_buffer_bytes=1 MiB)` → `EncodedStream`; `vc.queue.enqueue_stream(feed)` (lazy, §7.10) | `ByteRing` (L8) → `songbird-decode-N` thread → internal `PcmQueue` (48 kHz stereo, fixed 1 s cap) → `LiveReader` | Parsed | as above | no | Replaces `StreamInput`. The decode thread runs `MediaSourceStream::new(Box::new(ByteRingSource), Default::default())`, probes via `get_probe()`, decodes via `get_codec_registry()`, and resamples any rate (e.g. 44.1 kHz) to 48 kHz with `dsp::StreamResampler` (rubato `SincFixedIn`, arbitrary ratio, off-mixer). There is no `hint` parameter: symphonia-core 0.5.5 `Probe::format` ignores its hint (`_hint`, `probe.rs:312-314`), and so does songbird (`live_input.rs:57`). The track **starts paused** and is resumed by the first decoded frame's write (§7.3e). Pushing blocks on the queue's space condvar, which the housekeeper notifies and `close()` wakes. `ByteRingSource::read` blocks on L8's condvar and returns `Ok(0)` only after `finish()` and drain. **At most 16 decode threads per process** (`ENCODED_WORKERS: AtomicUsize`; exceeding the limit raises `LimitExceeded`), and ≤ 16 live sources per connection. Decode errors close the stream with `"decode_error"`, giving `TrackEnded(reason="error")`. `feed_from(reader)` is a Python helper. |
| `vc.open_opus_stream(max_packets=250)` → `OpusPacketStream` | `OpusQueue` (L7) + `OpusStreamReader` + the registry Opus decoder (`get_codec_registry().make(&params, &DecoderOptions::default())`) | Parsed, `CODEC_TYPE_OPUS`, 48 kHz | `try_lock`. Pops a packet and **moves** its `Box<[u8]>` into `Packet::new_from_boxed_slice`. On underrun, contention or self-mute it uses a pre-allocated `SILENT_FRAME` box from a 64-entry reserve that the housekeeper refills (it allocates 3 bytes only if the reserve is empty). | no | **Starts paused**; the first `send` resumes it, and the housekeeper pauses it again after `idle_ms` without packets (§7.5), so an unfed stream sends no RTP. Passthrough when it is the only playing track at volume 1.0 (`mix_logic.rs:69-94`). `send_nowait` validates 1–1275 bytes and `opus::packet::get_nb_samples(p, 48000) == 960`. EOF only after close and drain. |
| `AudioFile(data)`, `AudioFile.from_path(path, *, max_bytes=256 MiB)`, `await AudioFile.load(path, …)`, `AudioFile.from_arrow(array)` | `EncodedBytes{data: Arc<[u8]>, info}`; copied once (`try_reserve` → `MemoryError`); **eagerly probed** inside `py.detach` (raises `SourceError`; exposes `codec`, `sample_rate`, `channels`, `duration_ms` from the probe result) | `Input::Live(LiveInput::Raw(AudioStream{input: Box::new(DoneSource::new(Cursor::new(ArcBytes(data)), done))}), None)`. `AudioStream` has only an `input` field in 0.6.1 (`input/audiostream.rs:2-8`). Probed on songbird's BlockyTaskPool from memory. | songbird decodes from memory (no disk I/O on the mixer) | yes | `from_path` reads the file **inside `py.detach`**. `load()` wraps it in `asyncio.to_thread`. `from_arrow` accepts a UInt8 primitive array, or a single-value Binary/LargeBinary array, without nulls (pyarrow arrays do not implement the buffer protocol); it is copied once and released while attached (§8.12). No `hint` parameter (see the EncodedStream row). Reusable: each play gets a fresh cursor over the same `Arc`. |
| `RawPcm(data, format)`, `RawPcm.from_arrow(array, format)`, `RawPcm.from_samples(data, sample_rate, channels=2, encoding="f32le")` | `MemPcm{fmt, samples: Arc<[i16]>}`. `RawPcm(...)`/`from_arrow` store the input rate (8/16/24/48 kHz), parsed once and detached above 1 MiB. `from_samples` accepts **any rate from 8 to 192 kHz** (pcm16le or f32le, buffer or Arrow array, as 0.5's `RawPCMInput` did). Unless the rate is one of the four stream rates, it resamples once to 48 kHz with `dsp::StreamResampler` inside `py.detach` and stores 48 kHz. | Parsed: `MemPcmReader` + `PcmDecoder` (the same decoder, resampling on the mixer side), `supports_backseek: true` | slice copy + resample | yes (ts → frame; resampler reset) | Reusable. |
| `OpusPackets(frames)`, `OpusPackets.from_arrow(binary_array)` | `OpusFrames{bytes: Arc<[u8]>, offsets: Arc<[u32]>}`, each frame validated as above | Parsed: `MemOpusReader` + registry Opus decoder, `supports_backseek: true` | one packet-sized `Box` copy per tick (symphonia's `Packet` owns its buffer; songbird's own inputs do the same) | yes (by index) | Passthrough-capable. |

**No `Compose` is implemented anywhere.** Every input passes `None`, so 0.5's `unimplemented!()`/`todo!()` paths cannot exist. `DoneSource<M: MediaSource>` forwards `read`, `seek`, `is_seekable` and `byte_len`. It records `EOF_SEEN` in its `TrackFlags` when `read` returns 0, and drops its `DoneGuard` on drop. `MemPcmReader`, `MemOpusReader` and `LiveReader` likewise record `EOF_SEEN` when they return EOF (§7.10).

### 7.10 Tracks, handles and the queue

- **`vc.play(source, *, volume=1.0, loops=0, paused=False) -> TrackHandle`** is synchronous and O(1).
  1. Check `MAX_PLAYING_TRACKS_PER_CONN` (32, counting `play()` tracks plus the current queue entry that have not ended) → `LimitExceeded`. Each playing track is decoded on songbird's shared mixer thread, so an unbounded number would stall up to 16 other calls (the defect-9 class).
  2. Register `TrackFlags` and a `DoneGuard`.
  3. Build `Track::new(input).volume(v).loops(LoopState::Finite(NonMaxU32::new(n)?)`, or `LoopState::Infinite` when `loops=None`. Add `.pause()` if `paused`.
  4. `driver.play(track)` under L1 returns songbird's `TrackHandle`.
  5. `registry.insert(handle.uuid().as_u128(), Weak<TrackFlags>)`.
  - Live sources (`PlaybackStream`, …) are created by `open_*`, not by `play`.
- **Completion is authoritative, fires on every termination path, and is resolved by priority, not by the first writer.** songbird disposes a finished, stopped or errored track on the DisposalThread (`audio_commands_events`: `disposer.dispose(Track)`, `mixer/mod.rs:466-480`) **before** it sends `ChangeState(Mode)` and `Tick` to the events task. Global track events fire only on that later `Tick` (`GlobalEvents::tick`), and the DisposalThread drops the input as soon as it receives it (`disposal.rs:43`). So a reader's `Drop` usually runs before `Dispatch` sees `End`/`Error`, and first-writer-wins would mislabel most natural ends and errors as `stopped`.
  - **Dispatch's global `TrackEvent::End`/`Error` handler is authoritative.** It maps `TrackState.playing` to a reason: `PlayMode::End` → `finished`, `Stop` → `stopped`, `Errored(e)` → `error` (with `e` as `TrackEnd.error`). It overrides any provisional state. `TrackHandle::stop()` *does* fire `End` (`tracks/mode.rs:55`).
  - **`DoneGuard::drop` is provisional.** It sets a `DROPPED` bit and records the time. Our readers set `EOF_SEEN` when they return EOF (`DoneSource` when `read` returns 0) and `DECODE_ERROR` on a decode error, before they are dropped.
  - **Delayed finalisation.** The housekeeper finalises a track that is `DROPPED` but still unresolved once 2 ticks have passed since the drop. That is enough for the `Tick` that carries `End`, which is sent in the same mixer cycle. The reason is: `disconnected` if the connection is closing; otherwise `finished` if `EOF_SEEN`; `error` if `DECODE_ERROR`; otherwise `stopped`. `Driver::stop()` (`SetTrack(None)`) and Poison fire no `End` event (`events.rs:110`), so `stop_all()` and teardown resolve through this path or through T8.
  - On resolution the housekeeper emits `TrackEnded{track_id, reason}` to event streams and rings `WAKE_TRACKS`. The pump then resolves `wait_ended()` futures **level-triggered** (§6.4).
- **`TrackHandle`:**
  - `pause()`, `resume()`, `stop()`, `set_volume(v)` and `set_loops(n | None)` (`enable_loop`/`disable_loop`/`loop_for(NonMaxU32)`). songbird `ControlError` → `TrackError`. `set_loops` and `seek` on live sources raise `TrackError`.
  - `await seek(seconds, *, timeout=5.0) -> float` and `await info(*, timeout=1.0) -> TrackInfo` go through `OpRegistry` (≤ 64 per connection). `seconds` is validated with `Duration::try_from_secs_f64` and `0 ≤ seconds ≤ MAX_SEEK_S` (86 400). Negative, NaN, infinite or huge values raise `LimitExceeded` instead of panicking. A task on RX runs `tokio::time::timeout(t, handle.seek(d).result_async())` or `handle.get_info()`, stores the result, and rings `WAKE_OPS`. songbird processes these only on **live** mixers (`mixer/mod.rs:417-496`), so while parked they raise `TimeoutError`, which is documented.
  - **Mixer-dependent commands.** songbird processes track commands only on live mixers. `pause`, `resume`, `stop` and `volume` for file tracks take effect once the mixer is live; this is documented. `PlaybackStream` volume does not depend on this (§7.8).
- **`TrackQueue`** lives in sbcore. **Decision (graft D):** songbird's `builtin-queue` is dropped.
  - Pending entries `(SourceSpec, PlayOpts, TrackRef, DoneGuard)` are **not given to songbird until they become current**, so queued inputs are not resident in the mixer. At most `MAX_QUEUE_ENTRIES` (10 000) are pending; a further `enqueue` raises `LimitExceeded`.
  - **Sources.** `SourceSpec` is `File | Pcm | Opus | Encoded(EncodedFactory)`. `Encoded` is lazy: `vc.queue.enqueue_stream(feed, *, volume=1.0, loops=0, paused=False, max_buffer_bytes=1 MiB)` stores only the spec, and opens the `ByteRing` and decode thread when the entry becomes current. A queued live stream therefore holds no buffer and no thread. The Python `TrackQueue` keeps `feed` (an `AsyncIterable[bytes]` or `asyncio.StreamReader`) in a dict keyed by track id. When the entry becomes current, the housekeeper rings `WAKE_TRACKS`, the pump calls `conn.queue_take_stream(track)` and starts `feed_from(feed)` as a tracked task. The dict entry is dropped when the track ends or is dequeued. This restores 0.5's `vc.enqueue(Track(StreamInput(reader)))`.
  - **Advancing.** The housekeeper advances the queue when the current track's flags resolve and the queue is not paused: pop under L10, release, then `play` under L1. This is independent of the Python loop but **not gapless**: detecting the end by polling (≤ 20 ms), the `AddTrack` pickup on the next mixer cycle, and the probe of an `AudioFile` on the BlockyTaskPool (or an encoded stream's first decode) leave an audible gap of about 20–60 ms between entries.
  - **Pending entries are fully controllable.** `pause()`/`resume()`/`set_volume()`/`set_loops()` on a pending entry update its stored `PlayOpts`, which apply when it starts. `stop()` removes it (`stopped`). Only `seek()`/`info()` raise `TrackError("track has not started")`.
  - **Indexing.** Index 0 is the **first pending entry**. The current track is excluded from `__len__`, `__getitem__`, `__iter__` and `dequeue`, and is available as `current`.
  - Methods:

    | Method | Behaviour |
    |---|---|
    | `enqueue(source, *, volume, loops, paused)` | add an entry (`LimitExceeded` beyond 10 000) |
    | `enqueue_stream(feed, *, volume, loops, paused, max_buffer_bytes)` | add a lazy encoded-stream entry |
    | `skip()` | `current.stop()`, then advance |
    | `stop()` | `clear()`, then `skip()`: stop the current track and empty the queue (0.5 `Queue.stop()`) |
    | `pause()`/`resume()` | apply to the current track and block or allow advancing |
    | `clear()` | pending entries end with `stopped`; the current track keeps playing |
    | `dequeue(i)` | remove pending entry `i` and mark it `stopped` |
    | `current` | the playing entry (property) |
    | `__len__` | number of pending entries |
    | `__iter__` | snapshot of pending entries |
    | `__getitem__` | pending entry `i`; raises `IndexError` |

---

## 8. Receive design

### 8.1 songbird receive configuration

- **Decode mode.** `DecodeMode::Decrypt` when `VoiceConfig.receive` is true; `Pass` when false. In `Pass` mode no `VoiceTick` handler is registered, and `listen_*` raise `ReceiveDisabled`.
- **Decrypt instead of `Decode(..)`.** songbird's own decode path has two defects:
  - its packet-loss concealment length is wrong for anything except 48 kHz stereo (`ssrc_state.rs:110-119`);
  - with DAVE it feeds the leftover supplemental block to the decoder (`udp_rx/mod.rs:225-234`, upstream #315).
- **Jitter buffer.** `playout_buffer_length = playout_buffer_ms/20`, default **60 ms** (3 packets). `VoiceConfig.low_latency()` gives 40 ms. Range 20–200 ms. `playout_spike_length` defaults to 3.

### 8.2 The single per-connection dispatcher (`sbcore::conn::dispatch`)

```rust
#[derive(Clone)] pub struct Dispatch(Arc<DispatchInner>);
pub(crate) struct DispatchInner { conn: Weak<ConnShared>, rx: Option<RxSender>, tracks: Weak<TrackRegistry>,
                                  ctrl_overflow: Arc<Mutex<VecDeque<RxInput>>> /* cap 64 */, ctr: DispatchCounters }
#[async_trait]
impl songbird::events::EventHandler for Dispatch {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if catch_unwind(AssertUnwindSafe(|| self.0.on_event(ctx))).is_err() { self.0.ctr.panics.fetch_add(1, Relaxed); }
        None      // never Some(Event::Cancel): a no-op for untimed handlers (events/store.rs:127-143)
    }
}
```

The dispatcher is registered once per event type at Driver creation (§4.8) and never added or removed afterwards, except by teardown and by watchdog re-installation (§8.8). Unsubscribing is purely a Rust registry operation (§8.9).

`on_event` is O(#SSRC) and never awaits:

| Event | Action |
|---|---|
| `VoiceTick(t)` | Build `RawTick{arrived: Instant::now(), voices, silent}`. Each `t.speaking` entry becomes `RawVoice::Packet{ssrc, packet: rd.packet.clone() /* Bytes refcount */, payload_offset, payload_end: rd.payload_end_pad}`, or `RawVoice::Lost{ssrc}` when `packet: None`. Then `rx.try_send(RxInput::Tick)`. If the channel is full, increment `ticks_dropped`; the pipeline treats the hole like any other gap. |
| `SpeakingStateUpdate(s)` | If `s.user_id` is `Some`: `RxInput::Map{ssrc, user_id}`. |
| `ClientDisconnect(c)` | `RxInput::UserLeft{user_id}`. |
| `DriverConnect(d)` / `DriverReconnect(d)` | `conn.apply(GwInput::DriverConnect / DriverReconnect{ssrc})` (§4.6), plus `RxInput::LinkUp{ssrc}`. |
| `DriverDisconnect(d)` | `conn.apply(GwInput::DriverDisconnect{kind, reason})`, plus `RxInput::LinkDown`. |
| `Track(list)` from `End`/`Error` | `tracks.on_track_event(list)` (§7.10). |
| heartbeat (`Periodic(1 s)`) | Registered as a separate `Heartbeat(Arc<DispatchInner>)` handler, because a global timed event's context is `EventContext::Track(every track)` (`events/store.rs`, `GlobalEvents::tick`), which `on_event` could not tell apart from `End`. It only does `ctr.heartbeat.store(now_ms)`, and the ConnWorker compares that with the clock (§8.8). |

Control inputs (`Map`, `UserLeft`, `Link*`) also use `try_send`. When the channel is full they go to `ctrl_overflow`, a 64-entry leaf lock that the worker drains first, so mappings are never silently lost.

### 8.3 Opus extraction (`sbcore::rx::opus::extract_opus`; pure; fuzzed)

In Decrypt mode, `VoiceTick`'s `RtpData` has two quirks:

- `payload_end_pad` is an **end index** (`ssrc_state.rs:86`), despite its documentation.
- The RTP extension and padding are not stripped.

After DAVE decryption the packet also still carries the leftover supplemental block, because songbird copies the plaintext to the front of the buffer but keeps the full length (`udp_rx/mod.rs:225-234,271-276`). The whole workaround is isolated in this one function:

```rust
pub fn extract_opus(pkt: &[u8], payload_offset: usize, payload_end: usize) -> Option<Range<usize>> {
    let rtp = songbird::packet::rtp::RtpPacket::new(pkt)?;
    let hdr = pkt.len().checked_sub(rtp.payload().len())?;
    let (mut s, mut e) = (hdr.checked_add(payload_offset)?, hdr.checked_add(payload_end)?);
    if e > pkt.len() || s >= e { return None; }
    if rtp.get_extension() != 0 { s = s.checked_add(RtpExtensionPacket::new(pkt.get(s..e)?)?.packet_size())?; }
    if rtp.get_padding() != 0 { e = e.checked_sub(usize::from(*pkt.get(e.checked_sub(1)?)?))?; }
    let body = pkt.get(s..e)?;
    if body.len() >= 11 && body.ends_with(&[0xFA, 0xFA]) {               // DAVE leftover (tag8|nonce|ranges|size|FA FA)
        let n = usize::from(*body.get(body.len().checked_sub(3)?)?);
        if (11..body.len()).contains(&n) {
            let stripped = body.get(..body.len().checked_sub(n)?)?;
            if matches!(opus::packet::get_nb_samples(stripped, 48_000), Ok(120..=5760)) {  // TOC sanity
                e = e.checked_sub(n)?;
            }
        }
    }
    (s < e).then_some(s..e)
}
```

Every access uses `get`/`checked_*`, as sbcore's `deny(clippy::indexing_slicing)` requires.

- **Failure handling.** Any `None` is treated as a lost packet and counted in `bad_packets`. If upstream fixes songbird, the strip becomes a no-op.
- **Testing.** The fuzz test feeds 10⁶ random packets with a fixed-seed xorshift and asserts no panic and no out-of-bounds access.

### 8.4 Decoding and tick normalisation

- **Per-SSRC state.** `SsrcState{decoder: opus::Decoder::new(48_000, opus::Channels::Stereo), carry: VecDeque<f32> (≤ 120 ms = 5 760 frames × 2), user: Option<u64>, last_seq, last_seen_tick}`. At most `max_decoded_users` states exist (default 64, range 1–256, LRU-evicted).
- **Decode only what is needed.** An SSRC is decoded only if a consumer needs its user: a per-user lane, a mix bus that selects that user, a router, or a configured `voice_threshold_dbfs`. Otherwise speaking detection uses packet presence only.
- **Per tick, for each needed SSRC:**
  - **Real packet:** `decode_float(&pkt[range], &mut scratch[..5760*2], false)` decodes 20/40/60/120 ms into the carry.
  - **`Lost`:** concealment with `decode_float(&[], &mut scratch[..1920], false)` gives **exactly 960 frames**.
  - **Decode error:** treated as lost, plus `decode_errors += 1`. Five consecutive errors trigger `reset_state()`.
  - **Sequence jump greater than 10:** `reset_state()`.
  - **Normalise:** pop exactly 960 frames from the carry, zero-filling any shortfall. Carry overflow drops the oldest frames and is counted.
- **Per-user frame.** Each user's SSRCs are summed into a 960-frame 48 kHz stereo f32 frame. Unmapped SSRCs are summed into an `unmapped` frame.

### 8.5 Speaking, join and leave events

- **Voiced tick:** a real packet whose payload is not `SILENT_FRAME` (`F8 FF FE`). If `voice_threshold_dbfs` is set, the decoded RMS must also be at or above it. Up to 3 consecutive lost ticks inside a talk spurt still count as voiced.
- **Speaking FSM, per user:**
  - Idle → Speaking on the first voiced tick: `SpeakingStarted{user_id, timeline_ms, captured_at}`.
  - Speaking → Idle after `speaking_hangover_ms` (default 200, range 20–5000) without a voiced tick: `SpeakingStopped{user_id, timeline_ms, duration_ms}`.
- **`UserMapped{user_id, ssrc}`** fires on a new or changed SSRC mapping. It also queues the user for bot classification (`Control::ClassifyUsers`).
- **`UserLeft{user_id}`** fires on `ClientDisconnect`. A `SpeakingStopped` is emitted first if the user was speaking. SSRC state is pruned 1 s later so the jitter buffer can drain.
- **Joins are not observable through songbird** (`ClientConnect` was discontinued, `ws.rs:270-272`). The documentation points to discord.py's `on_voice_state_update`. `UserMapped` means "first transmitted".
- **Retention.** The SSRC → user map survives reconnects. Idle SSRC state is pruned after `decode_state_timeout_s` (default 60 s).

### 8.6 Sources: per-user and mixed (`MixBus`)

- **`RxSource::User(u64)`** is the user frame: zeros while the user is absent, silent or gone. A per-user stream never ends on its own.
- **`RxSource::Mixed(MixFilter)`.** `MixFilter{include: Option<BTreeSet<u64>>, exclude: BTreeSet<u64>, ignore_bots: bool, policy}`. Each distinct filter gets one `MixBus`, shared by every lane with an equal filter.
  - **`policy="mix"`:** the f32 sum of the selected users, plus `unmapped` only when `include is None`. Then a feed-forward `Limiter`: −1 dBFS threshold, instant attack, 80 ms release, no look-ahead. **Never divide by N**, because the level would jump as speakers join and confuse downstream voice-activity detection.
  - **`policy="floor"`** (graft B; a dominant-speaker mix for consumers that expect one speaker at a time): the first voiced user holds the floor (ties go to the higher RMS) until they have been silent for `floor_release_ms` (default 600). Only the holder is mixed, and `chunk.speakers == {holder}`.
  - **`ignore_bots`** uses the flags set from Python. A bot's first few hundred ms can be mixed before its classification arrives; this is documented.

### 8.7 Conversion, lanes and fan-out (one decode, shared conversion)

- **Converters** are keyed by `(SourceKey, rate, channels)` and reference-counted by lanes.
  1. Channel map: stereo → mono as `(L+R)·0.5`, or keep stereo.
  2. `TickResampler` from 48 kHz to the target rate (`SincFixedIn<f32>`, `Nearest`, `oversampling_factor = L`, `sinc_len = 64·M`, `f_cutoff = 0.95`, `BlackmanHarris2`, chunk 960):
     - **exactly `rate/50` frames out per tick** (the output FIFO is primed once with `output_delay` zeros);
     - added delay 1.3 / 2.0 / 4.0 ms for 24 / 16 / 8 kHz;
     - ≥ 131 dB alias rejection;
     - ≤ 5.4 µs per tick.
- **Lanes** are keyed by `(SourceKey, Format, chunk_ms)`, with **graft C sharing**.
  - A lane encodes into a 4-byte-aligned `PcmBuf` accumulator:
    - pcm16le: round and clamp;
    - f32le;
    - µ-law/A-law via `dsp::g711` (requires 8 kHz mono).
  - It emits `Arc<ChunkData>` at **timeline-aligned boundaries**, i.e. where the chunk's start sample on the connection timeline is ≡ 0 mod `chunk_frames`. `chunk_frames = rate·chunk_ms/1000` is an integer for every supported rate and every `chunk_ms` from 10 to 1000.
  - Every subscriber in a lane receives the same `Arc`. Each chunk owns only its own source's samples (defect 5).
- **Subscribing** attaches the subscription to its lane. A new lane starts at the next aligned boundary, so the first chunk arrives within `chunk_ms` plus at most one chunk.
- **`set_include`/`set_exclude`** move the subscription to its new lane at that lane's first complete chunk. The old lane keeps delivering until then: no gap and no duplicate.
- **`ChunkData`:**

  ```rust
  pub struct ChunkData { pub meta: ChunkMeta, pub data: PcmBuf }
  pub struct ChunkMeta { pub format: Format, pub user_id: Option<u64>, pub timeline_ms: u64, pub frames: u32,
                         pub speaking: bool, pub voiced_ms: u32, pub concealed_ms: u32, pub gap_ms: u32,
                         pub speakers: Box<[u64]>, pub level: Level, pub captured_at: Instant }
  ```

  The payload is Arc-backed, so Arrow export's release callback can own a Rust `Arc` (§8.12).

### 8.8 Continuous timeline, silence filling and the stall watchdog

- **Timeline origin.** `t0` is the ConnWorker start (connection creation). `tick_index` counts real and synthetic ticks, and `timeline_ms = 20·tick_index`. `expected(now) = ⌊(now − t0)/20 ms⌋`.
- **Synthesis.** While receive is enabled and at least one subscription or router exists, whenever `tick_index < expected(now) − 5`, the worker synthesises silent ticks up to `expected(now) − 2`. That is a 100 ms trigger with 40 ms of slack.
  - This covers time **before the first real tick**, link-down, reconnects, rejoins and a dead UDP-rx task.
  - Synthetic ticks are marked, and chunks report them in `gap_ms`.
  - Streams therefore emit one chunk per `chunk_ms` of real time from subscription until close, which keeps any consumer (a file writer, a network uplink) real-time paced.
  - With no subscribers, synthesis is lazy: `tick_index` fast-forwards on the next tick or subscribe, so idle connections cost no wake-ups.
- **Debt.** When real ticks come back in a burst (songbird never skips ticks, `udp_rx/mod.rs:86-111`) and `tick_index − expected(now) > 5`:
  - a real tick that is **fully silent** (no voiced packet for any user) is dropped and counted in `ticks_skipped`;
  - **voiced ticks are always processed and never dropped.** The timeline then converges at the next silence.
- **Watchdog.** Two independent signals:
  - **Handler liveness (always on).** The `Periodic(1 s)` heartbeat (§4.8) fires from the `Tick`s of live **and** parked mixers, so it does not depend on `receive` or on the link. If no heartbeat has arrived for 3 s while the Driver exists, the worker calls `apply(GwInput::HandlersLost)`, whose `ReinstallDispatch` calls `remove_all_global_events()` and re-registers, at most once per 5 s. This handles songbird's `restart_volatile_internals` (`RebuildInterconnect`, `message/mod.rs:35-54`), which starts a fresh events runner with no global handlers, so `DriverConnect`/`DriverDisconnect`/track `End` would otherwise be lost silently, even with `receive=False`. Counted in `stats()["dispatch_reinstalls"]`.
  - **Receive stall (receive on, link up).** If no real tick has arrived for `stall_timeout_ms` (default 2000): emit `ReceiveStalled{since_ms}` once per stall and `apply(GwInput::RxStalled)` (same rate-limited reinstall). If the stall lasts ≥ 5 s and `auto_recover` is on (with `reconnect=True`), the state machine schedules a rejoin (§4.5). This is the DAVE #310 / UDP-rx #315 case, where handlers are alive but no audio flows.

### 8.9 Subscriptions: explicit close and weak ownership

- **`ConnShared::subscribe(SubSpec)`:**
  1. Validates against §10.
  2. Reserves the per-connection receive budget: Σ (`max_chunks × (chunk_bytes + 160)`) ≤ `max_receive_mb` (default 64 MiB). Otherwise `LimitExceeded`.
  3. Allocates a `WakeToken` and an `Arc<SubQueue>`.
  4. Pushes `WorkerCmd::AddSub`.
  5. Returns a `Subscription` immediately.
- **Ownership.** Python owns the `Subscription` (`Arc<SubQueue>` + `Weak<WorkerShared>`). The worker owns the only other `Arc`.
- **Unregistration** happens in any of these ways:
  - `close()` or `Drop` sets `closed` and pushes `RemoveSub(id)`;
  - the worker also removes any subscription whose `is_closed()` is true at every tick;
  - the budget is released at the same time.
- **No double registration.** Each `subscribe` creates a new `SubId` (a monotonic `u64` that is never reused). Re-registering a subscription object is impossible.
- **`SubQueue`** is `Mutex<VecDeque<Delivered>>` + armed flag + atomics. The bound is `max_chunks = ⌈max_queue_ms/chunk_ms⌉`. Capacity starts at `min(max_chunks, 16)` and grows lazily.

  ```rust
  pub struct Delivered { pub chunk: Arc<ChunkData>, pub seq: u64, pub start_ms: u64, pub dropped_ms: u64 }
  ```

  - **Stream clock.** `start_ms = chunk.timeline_ms − origin`, where the origin is the `timeline_ms` of the first delivered chunk (or of the first chunk after `skip_to_live(reset_clock=True)`). `seq = start_ms / chunk_ms`, so `seq` jumps over dropped chunks. `AudioStream.origin_timeline_ms` exposes the current origin, and `chunk.timeline_ms − chunk.start_ms` gives it for any chunk. For a consumer that receives every chunk and compensates every `dropped_ms` with silence, and that has not called `skip_to_live` since it started, `chunk.start_ms` equals the consumer's input timeline (for example offsets in a transcript), so those offsets can be mapped back to `chunk.speakers`. A consumer that skips keeps its own sent-ms counter instead.
  - **Overflow policies:**

    | Policy | Behaviour |
    |---|---|
    | `drop_oldest` (default) | Evict the front. Its `dropped_ms + duration` is added to the new front's `dropped_ms`. |
    | `drop_newest` | Discard the incoming chunk and add its duration to `pending_drop`. That is attached to the next accepted chunk's `dropped_ms`. |
    | `error` | Close with `Overflow`. Reading delivers the queued chunks, then **raises `StreamOverflow` once** (also from `async for`, which does not stop silently), then `StopAsyncIteration`/`StreamClosed`. So `error` is distinguishable from an ordinary close. |

    In every case, **a chunk's `dropped_ms` is the audio dropped immediately before it**, so a consumer can insert compensating silence.
  - **Counters:** `delivered`, `dropped_chunks`, `dropped_ms`, `queued_ms`.
  - **`skip_to_live(*, reset_clock=True) -> int`** drops everything queued and returns the milliseconds dropped (not reported as `dropped_ms`). With `reset_clock=True` it also resets the stream clock: the next delivered chunk has `start_ms == 0` and `seq == 0`, so the stream clock starts at the first audio a downstream consumer receives. The pending drop counter is cleared too.
- **Router (`listen_users`):**
  - `RouterSpec{format, chunk_ms, max_queue_ms, overflow, include, ignore_bots, close_on_leave}`.
  - On a user's first voiced tick that passes the filters, the worker creates a per-user subscription (budget-checked) and pushes it into the router queue. The queue holds 64 entries; drop-oldest closes the dropped stream and counts it.
  - `close_on_leave=True` closes that user's stream with `"user_left"`. A user who returns gets a new stream.
  - `UserStreams.close()` closes the router and, by default, every stream it created.

### 8.10 Delivery to Python and `AudioChunk`

- **`ReceiveSubscription.try_recv()`** (native) calls `SubQueue::try_pop_or_arm()`. The lock is released before the binding builds the frozen `AudioChunk{chunk: Arc<ChunkData>, seq, start_ms, dropped_ms}`, so there is no PCM copy. It returns `None` after arming, and raises `StreamClosed`/`StreamOverflow` once the queue is closed and drained.
- **`read_nowait()`** pops without arming, so it is usable from any thread.
- **Buffer protocol.** `AudioChunk.__getbuffer__` exports `chunk.data` read-only and 1-D:
  - format `"h"` (pcm16le), `"f"` (f32le) or `"B"` (G.711);
  - `itemsize` 2/4/1;
  - `view.obj = self`.

  Format strings are native-endian; a `compile_error!` on big-endian targets makes that equal little-endian. `memoryview(chunk).tolist()` works because the format is `"h"`, not `"<h"`. `np.frombuffer(chunk, "<i2")`, `wave.writeframes(chunk)` and `PlaybackStream.write(chunk)` read it without an intermediate copy.
- **Other methods:**
  - `to_bytes()` makes one copy (`PyBytes::new`);
  - `rms_dbfs` and `peak` come from `ChunkMeta.level`.
- **`captured_at`** maps `Instant` to `time.monotonic()` through a `(Instant, time.monotonic())` pair captured at module init.

### 8.11 Event delivery

- **`EventHub::emit(&EventData)`** pushes into every `EventQueue` whose `KindMask` matches.
  - Queues are bounded (`max_queue` 1–4096, default 256) with drop-oldest.
  - After drops, a synthetic `Lagged{dropped}` precedes the next delivered event.
  - Emitters are NET (Dispatch), RX (worker, housekeeper) and Python threads. The mixer never emits.
- **`Disconnected{will_retry: false}`** is always the last event before queues close. `EventHub::close_all(final)` appends it to every queue and marks the hub closed in one L5h critical section, synchronously in T6, so no concurrent `emit` can follow it.

### 8.12 Optional Arrow API (`arrow` feature)

- **`AudioChunk.__arrow_c_array__(requested_schema=None)`** exports a zero-copy array over the chunk's own buffer:
  - the array type is `Int16Array`, `Float32Array` or `UInt8Array`;
  - it is built with `Buffer::from_custom_allocation(ptr, len, Arc<ChunkData> as Arc<dyn Allocation>)`, then `pyo3_arrow::ffi::to_array_pycapsules(py, field, &array, requested_schema)`;
  - field metadata is `{"sample_rate", "channels", "encoding"}`, and stereo is interleaved.

  The release callback drops a Rust `Arc` on whichever thread the consumer releases from. That is Rust-only memory, so it never touches Python. Exporting one user's chunk pins only that chunk.
- **`AudioStream.read_batch(max_chunks=50) -> ChunkBatch | None`** drains up to `max_chunks` chunks into one record batch.
  - Schema: `user_id: uint64 (nullable)`, `seq: uint64`, `start_ms: int64`, `timeline_ms: int64`, `duration_ms: uint32`, `speaking: bool`, `dropped_ms: uint32`, `pcm: binary`.
  - `pcm` is built from **owned**, concatenated bytes in one copy.
  - `ChunkBatch` implements `__arrow_c_schema__`, `__arrow_c_array__` (as a `StructArray`), `num_rows` and `to_pyarrow()` (lazy import).
  - We never return pyo3-arrow's statically linked arro3 classes.
- **Import:**
  - `RawPcm.from_arrow(array, format)` accepts Int16/Float32/UInt8 primitive arrays without nulls. `RawPcm.from_samples(array, sample_rate, …)` accepts Int16/Float32 at any rate from 8 to 192 kHz.
  - `OpusPackets.from_arrow(array)` accepts Binary/LargeBinary.
  - `AudioFile.from_arrow(array)` accepts a UInt8 primitive array, or a Binary/LargeBinary array with exactly one non-null value (the 0.5 `AudioInput(pyarrow.Array)` migration path; pyarrow arrays do not implement the buffer protocol).
  - All of them copy into Rust-owned memory and drop the imported array **while still attached**, before any sbcore call.
- **Without the `arrow` feature** every member above exists and raises `SongbirdError("built without the 'arrow' feature")` (§2.4).

---

## 9. Python API specification

The public surface is `discord.ext.songbird`. `discord.ext.songbird.native` is internal: its stubs are generated, and its classes are re-exported only where listed below. The Python layer is fully typed, so there is no hand-written `.pyi`.

### 9.1 Module exports and type aliases

```python
# py-src/discord/ext/songbird/__init__.py
from collections.abc import AsyncIterable, AsyncIterator, Buffer, Callable, Iterable, Iterator
from typing import Any, Final, Literal, Self, final
import asyncio, os
import discord

__version__: Final[str]                        # == native.VERSION == "0.6.0"

type Encoding  = Literal["pcm16le", "f32le", "mulaw", "alaw"]
type Overflow  = Literal["drop_oldest", "drop_newest", "error"]
type MixPolicy = Literal["mix", "floor"]
type FormatLike = AudioFormat | str            # str → AudioFormat.parse(str)
type UserLike  = int | discord.abc.Snowflake   # Member/User/Object accepted wherever a user id is expected
type Source    = AudioFile | RawPcm | OpusPackets

__all__ = [
    "SongbirdClient", "connect", "VoiceConfig", "ConnectionStatus", "configure_runtime", "set_log_levels",
    "supported_codecs", "AudioFormat", "PCM16_8K", "PCM16_16K", "PCM16_24K", "PCM16_48K", "PCM16_48K_STEREO",
    "F32_48K", "F32_48K_STEREO", "MULAW_8K", "ALAW_8K", "AudioChunk", "AudioStream", "UserStreams", "StreamStats",
    "ChunkBatch", "VoiceEvent",
    "EventStream", "PlaybackStream", "PlaybackStats", "ClearResult", "ItemProgress", "EncodedStream",
    "OpusPacketStream", "AudioFile", "RawPcm", "OpusPackets", "Source", "TrackHandle", "TrackQueue", "TrackInfo",
    "TrackEnd", "SongbirdError", "NotConnected", "ConnectFailed", "ConnectTimeout", "ConnectionClosed",
    "StreamClosed", "StreamOverflow", "PlaybackFull", "ReceiveDisabled", "InvalidAudio", "LimitExceeded",
    "SourceError", "TrackError",
]
# also: atexit.register(native._shutdown_all); os.register_at_fork(after_in_child=native._disable_after_fork);
#       _log.start()  (the "songbird-log" daemon thread, §11.2)
```

### 9.2 Formats

```python
@final
class AudioFormat:                                  # native: frozen, eq, hash
    """Immutable raw-audio format. Little-endian, interleaved.

    sample_rate ∈ {8000, 16000, 24000, 48000}; channels ∈ {1, 2};
    "mulaw"/"alaw" (G.711) require 8000 Hz mono. Invalid combinations raise InvalidAudio.
    """
    def __init__(self, sample_rate: int = 24_000, channels: int = 1, encoding: Encoding = "pcm16le") -> None: ...
    @classmethod
    def parse(cls, spec: str) -> AudioFormat:
        """Parse 'pcm16_24k', 'pcm16_48k_stereo', 'f32_48k', 'mulaw', 'alaw' or 'pcm16le/24000/1'."""
    sample_rate: int
    channels: int
    encoding: Encoding
    @property
    def sample_width(self) -> int: """Bytes per sample: 2 (pcm16le), 4 (f32le), 1 (G.711)."""
    @property
    def frame_bytes(self) -> int: """sample_width * channels."""
    @property
    def bytes_per_ms(self) -> int: """Exact bytes per millisecond (48 for PCM16 24 kHz mono)."""
    @property
    def mime_type(self) -> str: """'audio/pcm;rate=24000', 'audio/pcmu', 'audio/pcma', 'audio/x-f32le;rate=48000'."""
    def bytes_for(self, ms: int) -> int: """Exact byte length of `ms` milliseconds."""
    def ms_for(self, nbytes: int) -> float: """Duration of `nbytes` bytes."""
    def silence(self, ms: int) -> bytes:
        """`ms` of digital silence (0x00; µ-law 0xFF; A-law 0xD5). 0 <= ms <= 600_000, checked before any
        arithmetic (LimitExceeded); allocated with try_reserve (MemoryError)."""

PCM16_8K: Final[AudioFormat];  PCM16_16K: Final[AudioFormat];  PCM16_24K: Final[AudioFormat]
PCM16_48K: Final[AudioFormat]; PCM16_48K_STEREO: Final[AudioFormat]
F32_48K: Final[AudioFormat];   F32_48K_STEREO: Final[AudioFormat]
MULAW_8K: Final[AudioFormat];  ALAW_8K: Final[AudioFormat]
```

Only these neutral presets exist (plus any string accepted by `parse`). **`PCM16_48K_STEREO` is Discord's native
format and the default of every `listen*` method and of `open_playback`** (§9.4), so audio passes through without
resampling or channel conversion unless the caller asks for another format. There are no service-specific
presets and no service-specific format helpers (Decision 2026-10-27).

### 9.3 Configuration, runtime and module functions

```python
@dataclass(frozen=True, slots=True, kw_only=True)
class VoiceConfig:
    """Per-connection settings. Validated on construction by native.VoiceSettings (ValueError/LimitExceeded)."""
    receive: bool = True                   # False: DecodeMode::Pass, no VoiceTick handler; listen_* raise ReceiveDisabled
    playout_buffer_ms: int = 60            # 20..=200, multiple of 20 (songbird jitter buffer)
    playout_spike_ms: int = 60             # 0..=200, multiple of 20
    decode_state_timeout_s: float = 60.0   # 1..=600
    speaking_hangover_ms: int = 200        # 20..=5000
    voice_threshold_dbfs: float | None = None   # -100..=0; None = packet activity only (no extra decoding)
    max_decoded_users: int = 64            # 1..=256 concurrently decoded SSRCs (LRU beyond)
    max_receive_mb: int = 64               # 1..=1024: Σ of all receive-queue bounds per connection
    stall_timeout_ms: int = 2_000          # 500..=30_000 receive watchdog
    mix_mode: Literal["mono", "stereo"] = "stereo"   # "mono" halves Opus encode cost (voice bots)
    bitrate: int | None = None             # 6_000..=510_000 bps; None = songbird default (128 kbps)
    softclip: bool = True
    driver_timeout_s: float = 10.0         # 1..=60 per transport attempt
    driver_retries: int = 5                # 1..=10 songbird transport retries (reconnect=True); never "infinite"
    rejoin_attempts: int = 3               # 0..=10 gateway-level rejoins (reconnect=True)
    auto_recover: bool = True              # rejoin after a ≥5 s receive stall (DAVE/UDP-rx recovery)

    @classmethod
    def low_latency(cls, **overrides: Any) -> VoiceConfig:
        """Preset for interactive, low-latency audio: playout_buffer_ms=40, mix_mode="mono"."""
    def replace(self, **changes: Any) -> VoiceConfig: """dataclasses.replace with validation."""

def supported_codecs() -> list[str]:
    """Codec/container names compiled into this build (for AudioFile/EncodedStream), e.g. 'opus', 'mp3', 'format:ogg'."""
def configure_runtime(*, net_workers: int = 2, rx_workers: int = 1) -> None:
    """Size the two internal tokio runtimes (1..=8 each). Call before the first connection, else RuntimeError."""
def set_log_levels(*, library: int | str = "INFO", songbird: int | str = "WARNING") -> None:
    """Set Rust-side log filtering for 'discord.ext.songbird.native.*' and 'discord.ext.songbird.songbird.*'."""
async def connect(channel: discord.abc.Connectable, *, config: VoiceConfig | None = None, timeout: float = 30.0,
                  reconnect: bool = True, self_deaf: bool = False, self_mute: bool = False) -> SongbirdClient:
    """Shortcut for `await channel.connect(cls=SongbirdClient.with_config(config), ...)`."""

@final
class ConnectionStatus:                     # native, frozen
    state: Literal["idle", "joining", "connecting", "connected", "reconnecting", "closed"]
    channel_id: int | None                  # channel of the current media connection
    target_channel_id: int | None           # channel being joined/moved to
    ssrc: int | None
    self_mute: bool; self_deaf: bool
    rejoin_attempt: int
    generation: int                         # increments on every new media connection
    close_reason: str | None; close_code: int | None
    timeline_ms: int
```

### 9.4 Client

```python
class SongbirdClient(discord.VoiceProtocol):
    """discord.py VoiceProtocol backed by songbird. Use `await channel.connect(cls=SongbirdClient)`."""
    def __init__(self, client: discord.Client, channel: discord.abc.Connectable, *,
                 config: VoiceConfig | None = None) -> None: ...
    @classmethod
    def with_config(cls, config: VoiceConfig | None = None, /, **overrides: Any
                    ) -> Callable[[discord.Client, discord.abc.Connectable], Self]:
        """Factory for `channel.connect(cls=...)` carrying a VoiceConfig."""

    # ---- VoiceProtocol hooks (called by discord.py) ----
    async def connect(self, *, timeout: float, reconnect: bool, self_deaf: bool = False, self_mute: bool = False) -> None:
        """Send one op 4 with the flags and wait for the media connection.
        Raises ConnectTimeout (a TimeoutError) or ConnectFailed. On ANY failure, including cancellation,
        the connection is torn down, a best-effort leave is sent and cleanup() runs before re-raising."""
    async def disconnect(self, *, force: bool = False) -> None:
        """Leave the channel and tear down every stream, playback and track. Always calls cleanup().
        Idempotent; never raises for gateway errors (logged unless force=True)."""
    async def on_voice_state_update(self, data: GuildVoiceStatePayload, /) -> None: ...
    async def on_voice_server_update(self, data: VoiceServerUpdatePayload, /) -> None: ...
    def cleanup(self) -> None: """Idempotent; closes all children and unregisters from discord.py."""

    # ---- state ----
    @property
    def guild(self) -> discord.Guild: ...
    @property
    def user(self) -> discord.ClientUser: ...
    @property
    def status(self) -> ConnectionStatus: ...
    @property
    def ssrc(self) -> int | None: ...
    @property
    def timeline_ms(self) -> int: """Receive timeline position (20 ms ticks since connect); aligns chunks across streams."""
    @property
    def self_mute(self) -> bool: ...
    @property
    def self_deaf(self) -> bool: ...
    def is_connected(self) -> bool: """status.state == 'connected'."""
    async def wait_connected(self, timeout: float | None = None) -> None:
        """Wait for state 'connected'. ConnectionClosed if closed; TimeoutError on timeout."""
    async def move_to(self, channel: discord.abc.Snowflake | None, *, timeout: float = 30.0) -> None:
        """Move and wait until connected on the new channel. None == disconnect(). Streams survive."""
    async def reconnect(self, *, timeout: float = 30.0) -> None:
        """Force a fresh voice session (op 4 leave + join). Recovery for stalled receive/DAVE."""
    async def set_self_mute(self, muted: bool) -> None:
        """Stop/resume sending audio; live streams hold their position while muted (§7.8). Sends op 4 only while
        connected; otherwise the flag is stored, wins over connect()'s argument if set before connect(), and is
        flushed with one op 4 when the state next becomes 'connected' (§6.7)."""
    async def set_self_deaf(self, deafened: bool) -> None: """Gateway flag; same sending rules as set_self_mute."""
    def set_bitrate(self, bps: int | None) -> None: """6_000..=510_000 or None (songbird default)."""
    def stats(self) -> dict[str, int]:
        """ticks_real, ticks_synthetic, ticks_dropped, ticks_skipped, bad_packets, decode_errors, plc_frames,
        dave_trailers_stripped, decoders_active, subscriptions, dispatch_reinstalls, heartbeat_misses, panics,
        log_dropped, …"""

    # ---- receive (valid in every state except 'closed'; NotConnected before connect()) ----
    # Every format defaults to PCM16_48K_STEREO: Discord's native 48 kHz stereo PCM16LE (no conversion).
    def listen(self, format: FormatLike = PCM16_48K_STEREO, *, user: UserLike | None = None,
               **options: Any) -> AudioStream:
        """listen_user(user, ...) if user is given, else listen_mixed(...)."""
    def listen_user(self, user: UserLike, format: FormatLike = PCM16_48K_STEREO, *, chunk_ms: int = 20,
                    max_queue_ms: int = 2_000, overflow: Overflow = "drop_oldest") -> AudioStream:
        """Continuous stream of one user: silence while they are quiet or absent."""
    def listen_mixed(self, format: FormatLike = PCM16_48K_STEREO, *, chunk_ms: int = 20, max_queue_ms: int = 2_000,
                     overflow: Overflow = "drop_oldest", include: Iterable[UserLike] | None = None,
                     exclude: Iterable[UserLike] = (), ignore_bots: bool = True,
                     policy: MixPolicy = "mix", floor_release_ms: int = 600) -> AudioStream:
        """Continuous, silence-filled mix of the channel; one chunk every chunk_ms (10..=1000).
        policy="floor": dominant-speaker mix for consumers that expect one speaker at a time."""
    def listen_users(self, format: FormatLike = PCM16_48K_STEREO, *, chunk_ms: int = 20, max_queue_ms: int = 2_000,
                     overflow: Overflow = "drop_oldest", include: Iterable[UserLike] | None = None,
                     ignore_bots: bool = True, close_on_leave: bool = True) -> UserStreams:
        """Router yielding one AudioStream per user when they first speak."""
    def events(self, *, max_queue: int = 256, kinds: Iterable[type[VoiceEvent]] | None = None) -> EventStream: ...

    # ---- send ----
    def open_playback(self, format: FormatLike = PCM16_48K_STEREO, *, max_buffer_ms: int = 60_000, prebuffer_ms: int = 0,
                      idle_ms: int = 250, auto_pause: bool = True, max_gap_ms: int = 2_000,
                      output_latency_ms: int = 40, volume: float = 1.0,
                      when_disconnected: Literal["buffer", "discard"] = "buffer") -> PlaybackStream:
        """Realtime PCM output fed by write(). Several may be open at once; songbird mixes them. Starts paused
        (nothing is sent until the first write). prebuffer_ms and max_gap_ms must be <= max_buffer_ms / 2."""
    def open_encoded_stream(self, *, max_buffer_bytes: int = 1 << 20, volume: float = 1.0) -> EncodedStream:
        """Push-based encoded input (mp3/ogg/…); starts when the first decoded audio is ready."""
    def open_opus_stream(self, *, max_packets: int = 250, volume: float = 1.0) -> OpusPacketStream: ...
    def play(self, source: Source, *, volume: float = 1.0, loops: int | None = 0,
             paused: bool = False) -> TrackHandle:
        """Start a track now, mixed with others. Synchronous and O(1). loops=None loops forever.
        LimitExceeded beyond 32 playing tracks per connection."""
    def enqueue(self, source: Source, *, volume: float = 1.0, loops: int | None = 0,
                paused: bool = False) -> TrackHandle: """== self.queue.enqueue(...)."""
    @property
    def queue(self) -> TrackQueue: ...
    def stop_all(self) -> None:
        """Stop every track, queue entry and live stream (they close); the connection stays up."""
```

### 9.5 Receive

```python
@final
class AudioChunk(Buffer):                   # native, frozen; read-only buffer ('h' | 'f' | 'B')
    """One received chunk on a continuous stream clock. memoryview(chunk), bytes(chunk),
    np.frombuffer(chunk, '<i2') and wave.writeframes(chunk) read it without an intermediate copy."""
    format: AudioFormat
    user_id: int | None        # None for mixed streams
    seq: int                   # start_ms // chunk_ms (jumps over dropped chunks)
    start_ms: int              # stream clock (0 = first chunk of this stream)
    end_ms: int
    duration_ms: int           # == chunk_ms
    frames: int
    start_sample: int          # start_ms * sample_rate // 1000
    timeline_ms: int           # connection receive timeline (same clock as VoiceEvent.timeline_ms)
    captured_at: float         # time.monotonic() of the tick that produced the first sample
    speaking: bool             # any voiced audio in this chunk
    speakers: frozenset[int]   # voiced users contributing (mixed); {user_id} or {} (per-user)
    voiced_ms: int; concealed_ms: int
    gap_ms: int                # synthetic silence (no Discord audio flow: link down, reconnect, stall)
    dropped_ms: int            # audio dropped by overflow immediately before this chunk
    rms_dbfs: float; peak: float
    def __buffer__(self, flags: int, /) -> memoryview: ...
    def __len__(self) -> int: """Payload length in bytes."""
    def to_bytes(self) -> bytes: """One copy."""
    def __arrow_c_array__(self, requested_schema: object | None = None) -> tuple[object, object]: """[arrow] zero-copy."""

@final
class StreamStats:                          # native, frozen
    queued_chunks: int; queued_ms: int; delivered_chunks: int; dropped_chunks: int; dropped_ms: int; overflowed: bool

class AudioStream(AsyncIterator[AudioChunk]):
    """Bounded, continuous, cancel-safe receive stream: one chunk per chunk_ms whether or not anyone speaks.
    Ends (StopAsyncIteration) after close() or connection close, once queued chunks are drained.
    Under overflow='error', __anext__ raises StreamOverflow once after the queued chunks, then stops."""
    format: AudioFormat; chunk_ms: int; user_id: int | None; kind: Literal["user", "mixed"]
    @property
    def closed(self) -> bool: ...
    @property
    def close_reason(self) -> str | None: """'closed' | 'connection_closed' | 'user_left' | 'overflow' | 'internal_error'."""
    @property
    def queued_ms(self) -> int: ...
    @property
    def dropped_ms(self) -> int: """Total audio dropped by the overflow policy."""
    @property
    def origin_timeline_ms(self) -> int | None:
        """timeline_ms of this stream's clock origin (start_ms == 0); None until the first chunk after a reset."""
    def stats(self) -> StreamStats: ...
    def __aiter__(self) -> Self: ...
    async def __anext__(self) -> AudioChunk:
        """Pops only after wake-up, synchronously: cancellation loses nothing. StreamOverflow (once) under
        overflow='error'; StopAsyncIteration once closed and drained."""
    async def read(self, timeout: float | None = None) -> AudioChunk:
        """Next chunk; StreamClosed when finished; StreamOverflow (once) under overflow='error'; TimeoutError."""
    def read_nowait(self) -> AudioChunk | None: """Any thread. None if empty; StreamClosed when finished."""
    def skip_to_live(self, *, reset_clock: bool = True) -> int:
        """Drop everything queued (e.g. once a downstream consumer is ready); returns ms dropped. reset_clock:
        the next chunk has start_ms == 0 and seq == 0 (the stream clock starts at the first audio the consumer gets)."""
    def set_include(self, users: Iterable[UserLike] | None) -> None:
        """Mixed streams only (ValueError otherwise). None = everyone. Applied at the next chunk boundary, no gap."""
    def set_exclude(self, users: Iterable[UserLike]) -> None: """Mixed streams only."""
    def read_batch(self, max_chunks: int = 50) -> ChunkBatch | None: """[arrow] Drain up to max_chunks into one batch."""
    def close(self) -> None: """Idempotent, any thread; unregisters at the next tick and frees queued chunks."""
    async def aclose(self) -> None: ...
    async def __aenter__(self) -> Self: ...
    async def __aexit__(self, *exc: object) -> None: """aclose()."""

class UserStreams(AsyncIterator[AudioStream]):
    """Yields one AudioStream per user as they first speak; each ends on UserLeft when close_on_leave=True."""
    @property
    def dropped(self) -> int: """Streams discarded because the router queue (64) overflowed."""
    def __aiter__(self) -> Self: ...
    async def __anext__(self) -> AudioStream: ...
    def close(self, *, close_streams: bool = True) -> None: ...
    async def aclose(self, *, close_streams: bool = True) -> None: ...
    async def __aenter__(self) -> Self: ...
    async def __aexit__(self, *exc: object) -> None: ...

@final
class ChunkBatch:                           # native [arrow]
    num_rows: int
    def __arrow_c_schema__(self) -> object: ...
    def __arrow_c_array__(self, requested_schema: object | None = None) -> tuple[object, object]: ...
    def to_pyarrow(self) -> "pyarrow.RecordBatch": ...
```

### 9.6 Events

```python
class VoiceEvent:     # native complex enum (frozen); each variant has __match_args__
    class Connected(VoiceEvent):       channel_id: int; ssrc: int; timeline_ms: int
    class Reconnected(VoiceEvent):     channel_id: int; ssrc: int; gap_ms: int
    class Disconnected(VoiceEvent):    reason: str; code: int | None; will_retry: bool
    class UserMapped(VoiceEvent):      user_id: int; ssrc: int
    class UserLeft(VoiceEvent):        user_id: int
    class SpeakingStarted(VoiceEvent): user_id: int; timeline_ms: int; captured_at: float
    class SpeakingStopped(VoiceEvent): user_id: int; timeline_ms: int; duration_ms: int
    class PlaybackStarted(VoiceEvent): playback_id: int
    class PlaybackIdle(VoiceEvent):    playback_id: int; played_ms: int
    class PlaybackEnded(VoiceEvent):   playback_id: int; reason: str
    class TrackEnded(VoiceEvent):      track_id: int; reason: Literal["finished", "stopped", "error", "disconnected"]
    class ReceiveStalled(VoiceEvent):  since_ms: int
    class Lagged(VoiceEvent):          dropped: int

class EventStream(AsyncIterator[VoiceEvent]):
    """Bounded (drop-oldest) event queue; a Lagged event reports drops. Ends after Disconnected(will_retry=False)."""
    @property
    def dropped(self) -> int: ...
    def __aiter__(self) -> Self: ...
    async def __anext__(self) -> VoiceEvent: ...
    def read_nowait(self) -> VoiceEvent | None: ...
    def close(self) -> None: ...
    async def aclose(self) -> None: ...
    async def __aenter__(self) -> Self: ...
    async def __aexit__(self, *exc: object) -> None: ...
```

### 9.7 Playback

```python
@final
class ItemProgress:                         # native, frozen
    tag: str; written_ms: int; played_ms: int; heard_ms: int
    complete: bool                          # end_item(tag) was called and everything written has been played

@final
class ClearResult:                          # native, frozen
    discarded_ms: int; played_ms: int; heard_ms: int; position_ms: int; generation: int
    items: tuple[ItemProgress, ...]         # every OPEN tag (even with nothing queued) + every tag pulled within
                                            #   the last output_latency_ms, at the moment of clear() (§7.7)
    def item(self, tag: str) -> ItemProgress | None: ...

@final
class PlaybackStats:                        # native, frozen
    written_ms: int; played_ms: int; heard_ms: int; buffered_ms: int; underrun_ms: int; gap_filled_ms: int
    rejected_ms: int; discarded_ms: int; contended_ticks: int; clears: int; tags_evicted: int

class PlaybackStream:
    """Realtime PCM output. write_nowait/clear are synchronous, thread-safe and never block the mixer.
    Starts paused (nothing is sent until the first write). Underrun plays silence; the track auto-pauses after
    idle_ms (speaking indicator off) and resumes on write. While self-muted, playback holds its position."""
    playback_id: int; format: AudioFormat; track: TrackHandle
    @property
    def played_ms(self) -> int: """Real audio handed to the Discord encoder."""
    @property
    def heard_ms(self) -> int:
        """Estimate: played_ms minus the part of output_latency_ms not yet elapsed since the last real pull
        (reaches played_ms once playback has stopped for output_latency_ms; §7.7)."""
    @property
    def position_ms(self) -> int: """Playhead including silence."""
    @property
    def buffered_ms(self) -> int: ...
    @property
    def written_ms(self) -> int: ...
    @property
    def is_idle(self) -> bool: ...
    @property
    def closed(self) -> bool: ...
    volume: float                                     # settable, 0.0..=4.0, 5 ms ramp
    async def write(self, data: Buffer, *, tag: str | None = None, at_ms: int | None = None,
                    timeout: float | None = None) -> None:
        """Append raw audio in self.format (any size; partial frames are carried). Waits only while the buffer
        is full. tag = application-defined segment tag; at_ms = source-timeline timestamp in ms (gap filling).
        A str raises TypeError (decode text-encoded audio first)."""
    def write_nowait(self, data: Buffer, *, tag: str | None = None, at_ms: int | None = None) -> None:
        """Like write(); raises PlaybackFull (nothing consumed) instead of waiting. Use this in tasks that read
        audio from a network connection. Typed buffers must match the encoding ('h' for pcm16le, 'f' for f32le,
        or raw bytes), else InvalidAudio."""
    def clear(self, *, reject_late: bool = True, hold_ms: int = 0,
              reject_until_gap_ms: int | None = None) -> ClearResult:
        """Interruption: discard all queued audio now (silent within ≤40 ms). Any thread.
        reject_late drops later writes for every open/interrupted tag (segments still arriving after the cut).
        reject_until_gap_ms drops later TIMED writes contiguous with the cleared audio until one starts at least
        that many ms after it (§7.7). hold_ms (fallback for untimed writes) drops ALL writes."""
    def end_item(self, tag: str) -> None:
        """The tagged segment is fully written: closes the tag, releases the prebuffer gate, and lets
        ItemProgress.complete become True once it has played."""
    def item(self, tag: str) -> ItemProgress | None: ...
    def played_ms_of(self, tag: str) -> int: """item(tag).played_ms or 0 (clamped to written)."""
    async def drain(self, timeout: float | None = None) -> bool: """Flush: wait until everything written was played."""
    async def wait_idle(self) -> None: """Wait for the next Active→Idle transition (playback finished)."""
    def pause(self) -> None: ...
    def resume(self) -> None: ...
    def stats(self) -> PlaybackStats: ...
    def close(self) -> None: """Stop immediately and free the buffer; idempotent."""
    async def aclose(self, *, drain: bool = False, timeout: float | None = None) -> None: ...
    async def __aenter__(self) -> Self: ...
    async def __aexit__(self, *exc: object) -> None: """aclose(drain=False)."""

class EncodedStream:
    """Push encoded bytes (mp3/ogg/wav/…; see supported_codecs()); decoded off the mixer thread."""
    track: TrackHandle
    async def write(self, data: Buffer, *, timeout: float | None = None) -> None: ...
    def write_nowait(self, data: Buffer) -> None: """Raises PlaybackFull (nothing consumed)."""
    async def feed_from(self, src: asyncio.StreamReader | AsyncIterable[bytes], *, chunk_size: int = 16_384,
                        finish: bool = True) -> int: """Pump a reader into the stream; returns bytes fed."""
    def finish(self) -> None: """End of input: the track ends after the decoded audio has played."""
    def close(self) -> None: ...
    async def aclose(self, *, drain: bool = True, timeout: float | None = None) -> None: ...
    async def __aenter__(self) -> Self: ...
    async def __aexit__(self, *exc: object) -> None: ...

class OpusPacketStream:
    """Push 20 ms / 48 kHz Opus packets; underrun sends Opus silence frames, and after idle_ms (250) without
    packets the track pauses (no RTP) until the next send. Starts paused. Passthrough when it is the sole track."""
    track: TrackHandle
    @property
    def buffered_ms(self) -> int: ...
    async def send(self, packet: Buffer, *, timeout: float | None = None) -> None: ...
    def send_nowait(self, packet: Buffer) -> None: """Raises PlaybackFull / InvalidAudio."""
    def clear(self) -> int: """Discard queued packets; returns ms discarded."""
    def close(self) -> None: ...
    async def aclose(self, *, drain: bool = True, timeout: float | None = None) -> None: ...
    async def __aenter__(self) -> Self: ...
    async def __aexit__(self, *exc: object) -> None: ...
```

### 9.8 Sources and tracks

```python
@final
class AudioFile:                            # native, frozen, reusable
    """Encoded audio held in memory (copied once, probed eagerly; the container is detected from content,
    since symphonia 0.5 ignores hints). Seekable, loopable."""
    def __init__(self, data: Buffer) -> None: """SourceError if unrecognised."""
    @classmethod
    def from_path(cls, path: str | os.PathLike[str], *, max_bytes: int = 256 << 20) -> Self:
        """Blocking read (GIL/thread state released). On the event loop prefer `await AudioFile.load(...)`."""
    @classmethod
    async def load(cls, path: str | os.PathLike[str], *, max_bytes: int = 256 << 20) -> Self:
        """from_path in asyncio.to_thread."""
    @classmethod
    def from_arrow(cls, array: object) -> Self:
        """[arrow] UInt8 array, or a Binary/LargeBinary array holding exactly one value (copied once)."""
    nbytes: int; codec: str; sample_rate: int | None; channels: int | None; duration_ms: int | None

@final
class RawPcm:                               # native, frozen, reusable
    """Raw PCM/G.711 in memory (whole frames; parsed once). Seekable, loopable."""
    def __init__(self, data: Buffer, format: FormatLike) -> None: ...
    @classmethod
    def from_arrow(cls, array: object, format: FormatLike) -> Self: """[arrow] Int16/Float32/UInt8 array, no nulls."""
    @classmethod
    def from_samples(cls, data: Buffer | object, sample_rate: int, channels: int = 2,
                     encoding: Literal["pcm16le", "f32le"] = "f32le") -> Self:
        """Interleaved PCM at ANY rate 8_000..=192_000 Hz (buffer, or [arrow] Int16/Float32 array); resampled once
        to 48 kHz at construction unless the rate is 8/16/24/48 kHz (0.5 RawPCMInput replacement)."""
    format: AudioFormat; duration_ms: int

@final
class OpusPackets:                          # native, frozen, reusable
    """Pre-encoded 20 ms / 48 kHz Opus packets; passthrough-capable; seekable by packet index."""
    def __init__(self, packets: Iterable[Buffer]) -> None: """InvalidAudio for a non-960-sample or >1275-byte packet."""
    @classmethod
    def from_arrow(cls, array: object) -> Self: """[arrow] Binary/LargeBinary, no nulls (copied)."""
    def __len__(self) -> int: ...
    duration_ms: int

@final
class TrackInfo:                            # native, frozen
    playing: Literal["play", "pause", "stop", "end", "errored"]; volume: float
    position_ms: int; play_time_ms: int; loops: int | None

@dataclass(frozen=True, slots=True)
class TrackEnd:
    reason: Literal["finished", "stopped", "error", "disconnected"]; error: str | None

class TrackHandle:
    id: int                                  # stable per connection
    uuid: str | None                         # songbird track uuid (None until a queued entry starts)
    kind: Literal["file", "pcm", "opus", "playback", "encoded_stream", "opus_stream"]
    @property
    def ended(self) -> bool: ...
    @property
    def end_reason(self) -> str | None: ...
    def pause(self) -> None: ...
    def resume(self) -> None: ...
    def stop(self) -> None: ...
    def set_volume(self, volume: float) -> None: """0.0..=4.0. File tracks: applied by songbird's live mixer."""
    def set_loops(self, count: int | None) -> None: """None = forever. TrackError for live sources."""
    async def seek(self, seconds: float, *, timeout: float = 5.0) -> float:
        """Returns the new position. 0 <= seconds <= 86_400 and finite (LimitExceeded). TrackError for live
        sources and not-yet-started queue entries; TimeoutError while the mixer is parked."""
    async def info(self, *, timeout: float = 1.0) -> TrackInfo: """TimeoutError while the mixer is parked."""
    async def wait_ended(self) -> TrackEnd: """Resolves on every termination path (end, error, stop, disconnect)."""
    def __eq__(self, other: object) -> bool: ...
    def __hash__(self) -> int: ...

class TrackQueue:
    """Sequential queue; only the current entry is handed to songbird. Indexing covers PENDING entries only
    (index 0 = next up); the playing entry is `current`. Not gapless (~20–60 ms between entries)."""
    def enqueue(self, source: Source, *, volume: float = 1.0, loops: int | None = 0,
                paused: bool = False) -> TrackHandle: """LimitExceeded beyond 10_000 pending entries."""
    def enqueue_stream(self, feed: AsyncIterable[bytes] | asyncio.StreamReader, *, volume: float = 1.0,
                       loops: int | None = 0, paused: bool = False, max_buffer_bytes: int = 1 << 20) -> TrackHandle:
        """Queue a live encoded stream (0.5 enqueue(Track(StreamInput(reader)))). Its buffer and decode thread
        are opened, and `feed` is pumped, only when the entry becomes current."""
    @property
    def current(self) -> TrackHandle | None: ...
    def skip(self) -> None: ...
    def stop(self) -> None: """clear() then skip(): stop the current track and empty the queue."""
    def pause(self) -> None: ...
    def resume(self) -> None: ...
    def clear(self) -> None: """Drop pending entries (they end with 'stopped'); the current track keeps playing."""
    def dequeue(self, index: int) -> TrackHandle: """Remove pending entry `index` (IndexError if out of range)."""
    def __len__(self) -> int: """Number of pending entries."""
    def __iter__(self) -> Iterator[TrackHandle]: """Snapshot of pending entries."""
    def __getitem__(self, index: int) -> TrackHandle: """Pending entry; IndexError when out of range."""
```

### 9.9 Errors (`discord/ext/songbird/errors.py`, pure Python; raised by native code through a cached import)

```python
class SongbirdError(Exception): ...
class NotConnected(SongbirdError): ...                      # used before connect() was called
class ConnectFailed(SongbirdError):
    reason: str; code: int | None
    def __init__(self, reason: str, code: int | None = None) -> None:
        super().__init__(reason if code is None else f"{reason} ({code})"); self.reason, self.code = reason, code
class ConnectTimeout(ConnectFailed, TimeoutError):          # isinstance(e, asyncio.TimeoutError) → discord.py cleanup
    timeout: float
    def __init__(self, timeout: float) -> None:              # reason='timeout', code=None
        super().__init__("timeout", None); self.timeout = timeout
class ConnectionClosed(SongbirdError):
    reason: str
class StreamClosed(SongbirdError):
    reason: str                                             # 'closed' | 'connection_closed' | 'user_left' | …
class StreamOverflow(StreamClosed): ...                    # raised once by read()/__anext__ after the queued chunks
class PlaybackFull(SongbirdError):
    needed_ms: int; free_ms: int
class ReceiveDisabled(SongbirdError): ...
class InvalidAudio(SongbirdError, ValueError): ...
class LimitExceeded(SongbirdError, ValueError):
    what: str; value: int | float; minimum: int | float; maximum: int | float   # float for volume/seconds/dBFS
class SourceError(SongbirdError): ...
class TrackError(SongbirdError): ...
# Deprecated aliases for one release via module __getattr__ (DeprecationWarning):
# PySongbirdError → SongbirdError, PyJoinError → ConnectFailed, PyControlError → TrackError
```

### 9.10 asyncio usage rules (documented)

1. **Context managers.** Every stream, playback and event stream is an async context manager. Every `close()` is synchronous, idempotent and safe from any thread. Dropping an unclosed object closes it.
2. **Loop binding.** Awaiting is bound to the event loop that ran `connect()`; awaiting from another loop raises `RuntimeError`. The synchronous methods (`read_nowait`, `write_nowait`, `clear`, `close`, properties) work from any thread.
3. **Writing from a network reader task.** In a task that reads audio from a network connection, use `write_nowait` and treat `PlaybackFull` as an overrun. Never `await write()` there, or backpressure would delay handling of interruption messages.
4. **When a consumer becomes ready.** Call `skip_to_live()` (which resets the stream clock to 0 by default) right after a downstream consumer becomes ready, and again after it reconnects. Discord streams can stay open while the consumer reconnects.
5. **Cancellation.** Iterators wait while empty and end only on close or disconnect, or with one `StreamOverflow` under `overflow="error"`. Cancelling a pending `__anext__` or `write()` never loses or duplicates audio.
6. **Examples are importable.** Every example guards `bot.run(...)` with `if __name__ == "__main__":`, and the bridge function takes its `url` as an argument, so tests can import it and point it at a local fake server.

### 9.11 Example A: echo bot (`examples/echo.py`)

Receive and playback streaming in one loop: one member's voice is played back `DELAY_MS` later. Only voiced chunks
are written, so the playback goes idle (and stops sending RTP) between utterances, and `prebuffer_ms` holds the
start of each utterance back by the delay. Both streams use the default format, Discord's native 48 kHz stereo
PCM16, so nothing is resampled.

```python
import asyncio
import os
from typing import Any

import discord
from discord.ext import songbird
from discord.ext.songbird import PCM16_48K_STEREO, PlaybackFull

FMT = PCM16_48K_STEREO  # the default of listen_*() and open_playback(); spelled out for clarity
DELAY_MS = 500  # 0..=1000


async def echo(vc: songbird.SongbirdClient, user: Any, stop: asyncio.Event, *, delay_ms: int = DELAY_MS) -> int:
    """Play ``user``'s voice back ``delay_ms`` late until ``stop`` is set or the connection closes.

    Returns the milliseconds of audio echoed.
    """
    async with (
        vc.listen_user(user, FMT, chunk_ms=20) as mic,
        vc.open_playback(FMT, prebuffer_ms=delay_ms, max_buffer_ms=max(2_000, 4 * delay_ms)) as speaker,
    ):

        async def stopper() -> None:
            await stop.wait()
            mic.close()  # ends the `async for` below once the queued chunks are read

        async with asyncio.TaskGroup() as tg:
            st = tg.create_task(stopper())
            async for chunk in mic:  # ends on !stop OR when the connection closes
                if not chunk.speaking:
                    continue  # silence fill: write nothing, so playback idles between utterances
                try:
                    speaker.write_nowait(chunk)  # buffer protocol: no intermediate bytes object
                except PlaybackFull:
                    pass  # the buffer is bounded; drop rather than fall further behind
            st.cancel()
        await speaker.drain(timeout=delay_ms / 1000 + 1)
        return speaker.written_ms


intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)
sessions: dict[int, asyncio.Event] = {}


@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.guild is None or not isinstance(msg.author, discord.Member):
        return
    if msg.content == "!echo" and msg.author.voice and msg.author.voice.channel and msg.guild.id not in sessions:
        vc = await songbird.connect(msg.author.voice.channel)
        stop = sessions[msg.guild.id] = asyncio.Event()
        try:
            echoed = await echo(vc, msg.author, stop)
            await msg.channel.send(f"Echoed {echoed / 1000:.1f} s of audio.")
        finally:
            sessions.pop(msg.guild.id, None)
            await vc.disconnect()
    elif msg.content == "!stop" and (ev := sessions.get(msg.guild.id)) is not None:
        ev.set()


if __name__ == "__main__":
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
```

### 9.12 Example B: generic WebSocket bridge (`examples/websocket_bridge.py`)

The shortest connection to an external audio service, with no service-specific code. The protocol is deliberately
minimal: the bot sends one **binary frame per received chunk** of raw audio in a configurable `input_format` and
`chunk_ms`; the service sends **binary frames of raw audio** in `output_format`, which are written to a
`PlaybackStream` as they arrive; a **text frame `"clear"`** is an interruption (`speaker.clear()`). Adapting it to a
particular service changes only the framing (wrapping chunks in the service's own messages and unwrapping its
replies, including any text encoding of the audio, which is done in user code). For services that report how much of
a reply was heard, write each reply with `tag=` and read `item(tag)` or `clear().items` (§7.7).

```python
import asyncio
import logging
import os
from typing import Any

import discord
import websockets
from discord.ext import songbird
from discord.ext.songbird import PCM16_48K_STEREO, FormatLike, PlaybackFull, VoiceConfig
from websockets.asyncio.client import connect as ws_connect

log = logging.getLogger("websocket_bridge")


async def bridge(
    vc: songbird.SongbirdClient,
    url: str,
    *,
    input_format: FormatLike = PCM16_48K_STEREO,
    output_format: FormatLike | None = None,
    chunk_ms: int = 20,
    user: Any | None = None,
    stop: asyncio.Event | None = None,
) -> str:
    """Stream ``vc``'s audio to the WebSocket at ``url`` and play what comes back.

    Parameters
    ----------
    vc : SongbirdClient
        A connected client.
    url : str
        The WebSocket to connect to.
    input_format : AudioFormat or str
        Format of the audio sent to the service.
    output_format : AudioFormat or str or None
        Format of the audio the service sends back (``None``: same as ``input_format``).
    chunk_ms : int
        Duration of each binary frame sent.
    user : user or None
        Send only this member's voice; ``None`` sends the channel mix.
    stop : asyncio.Event or None
        Ends the bridge when set.

    Returns
    -------
    str
        Why it ended: ``"websocket_closed"``, ``"voice_closed"`` or ``"stopped"``.
    """
    out_format = input_format if output_format is None else output_format
    async with (
        ws_connect(url, max_size=None) as ws,
        vc.listen(input_format, user=user, chunk_ms=chunk_ms, max_queue_ms=1_000) as mic,
        vc.open_playback(out_format) as speaker,
    ):
        mic.skip_to_live()  # drop what was captured while connecting

        async def uplink() -> str:
            try:
                async for chunk in mic:  # continuous: silence while nobody speaks
                    await ws.send(bytes(chunk))  # one binary frame per chunk
            except websockets.ConnectionClosed:
                return "websocket_closed"
            return "voice_closed"

        async def downlink() -> str:
            try:
                async for message in ws:
                    if isinstance(message, str):
                        if message == "clear":
                            speaker.clear()  # interruption: silent within one or two ticks
                        continue
                    try:
                        speaker.write_nowait(message)  # never blocks the reader
                    except PlaybackFull:
                        log.warning("playback overrun; %d bytes dropped", len(message))
            except websockets.ConnectionClosed:
                pass
            return "websocket_closed"

        async def stopped() -> str:
            await (stop or asyncio.Event()).wait()
            return "stopped"

        tasks = [asyncio.create_task(c) for c in (uplink(), downlink(), stopped())]
        try:
            done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        finally:
            for t in tasks:
                t.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
        reason = next(iter(done)).result()
        if reason == "websocket_closed":
            await speaker.drain(timeout=5)  # let the last reply finish
        return reason


intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)
sessions: dict[int, asyncio.Event] = {}


@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.guild is None or not isinstance(msg.author, discord.Member):
        return
    if msg.content == "!bridge" and msg.author.voice and msg.author.voice.channel and msg.guild.id not in sessions:
        vc = await songbird.connect(msg.author.voice.channel, config=VoiceConfig.low_latency())
        stop = sessions[msg.guild.id] = asyncio.Event()
        try:
            reason = await bridge(
                vc,
                os.environ["BRIDGE_URL"],
                input_format=os.environ.get("BRIDGE_FORMAT", "pcm16_48k_stereo"),
                chunk_ms=int(os.environ.get("BRIDGE_CHUNK_MS", "20")),
                stop=stop,
            )
            await msg.channel.send(f"bridge ended: {reason}")
        finally:
            sessions.pop(msg.guild.id, None)
            await vc.disconnect()
    elif msg.content == "!stop" and (ev := sessions.get(msg.guild.id)) is not None:
        ev.set()


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
```

### 9.13 Example C: recording bot (`examples/record.py`)

The bot writes one timeline-aligned WAV per user plus a log of speaking events.

```python
import asyncio, json, os, time, wave
from pathlib import Path
import discord
from discord.ext import songbird
from discord.ext.songbird import PCM16_48K, AudioStream, VoiceEvent

REC = PCM16_48K                                      # 48 kHz mono PCM16LE
ONE_S = REC.silence(1_000)                           # silence(ms) is capped at 600 s, so long gaps go in pieces

def write_silence(w: wave.Wave_write, ms: int) -> None:
    for _ in range(ms // 1_000):
        w.writeframes(ONE_S)
    w.writeframes(REC.silence(ms % 1_000))

async def record_user(stream: AudioStream, out: Path, origin_ms: int) -> None:
    async with stream:
        with wave.open(str(out / f"{stream.user_id}.wav"), "wb") as w:
            w.setnchannels(REC.channels); w.setsampwidth(REC.sample_width); w.setframerate(REC.sample_rate)
            first = True
            async for chunk in stream:               # continuous: silence while the user is quiet
                if first:                            # align every file on the shared receive timeline
                    write_silence(w, max(0, chunk.timeline_ms - origin_ms))
                    first = False
                if chunk.dropped_ms:                 # only if the disk could not keep up
                    write_silence(w, chunk.dropped_ms)
                w.writeframes(chunk)                 # buffer protocol: no intermediate bytes object

async def log_events(vc: songbird.SongbirdClient, out: Path) -> None:
    kinds = [VoiceEvent.SpeakingStarted, VoiceEvent.SpeakingStopped, VoiceEvent.UserLeft]
    with (out / "events.jsonl").open("a") as f:
        async with vc.events(kinds=kinds) as events:
            async for ev in events:
                f.write(json.dumps({"event": type(ev).__name__,
                                    **{k: getattr(ev, k) for k in ev.__match_args__}}) + "\n")

async def record(vc: songbird.SongbirdClient, out: Path, stop: asyncio.Event) -> None:
    out.mkdir(parents=True, exist_ok=True)
    origin = vc.timeline_ms
    async with vc.listen_users(REC, chunk_ms=200, max_queue_ms=10_000, ignore_bots=True) as users:
        async with asyncio.TaskGroup() as tg:
            logger = tg.create_task(log_events(vc, out))
            async def stopper() -> None:
                await stop.wait()
                users.close()                        # ends the router and every per-user stream
            st = tg.create_task(stopper())
            async for stream in users:               # ends on !stop OR when the connection closes
                tg.create_task(record_user(stream, out, origin))
            st.cancel(); logger.cancel()             # never wait for a !stop that will not come

intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)
sessions: dict[int, asyncio.Event] = {}

@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.guild is None or not isinstance(msg.author, discord.Member):
        return
    if msg.content == "!record" and msg.author.voice and msg.author.voice.channel and msg.guild.id not in sessions:
        vc = await msg.author.voice.channel.connect(cls=songbird.SongbirdClient, self_mute=True)
        stop = sessions[msg.guild.id] = asyncio.Event()
        try:
            await record(vc, Path("recordings") / f"{msg.guild.id}-{int(time.time())}", stop)
        finally:
            sessions.pop(msg.guild.id, None)
            await vc.disconnect()
    elif msg.content == "!stop" and (ev := sessions.get(msg.guild.id)) is not None:
        ev.set()

if __name__ == "__main__":
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
```

Memory is bounded per user at 10 s × 96 B/ms = 960 KB, however slowly the disk is written.

---

## 10. Memory budget table

Abbreviations: B/ms = bytes per millisecond of audio at the given format; ch = channels; N = concurrently decoded speakers.

**Validation rule.** Every user-supplied duration, count or size is checked against `limits.rs` **before any arithmetic** (`check(...)`, then `checked_mul`). That includes the one-shot helpers `AudioFormat.silence(ms)` (`SILENCE_MS`), `TrackHandle.seek(seconds)` (`SEEK_S`), and the length of a write, checked against the buffer cap before anything is allocated (§7.3). Violations raise `LimitExceeded`, which is a `ValueError`. `VecDeque` capacities start at `min(bound, 16)` and grow lazily. **Nothing calls `with_capacity(user_value)`**, and no `tokio::sync::broadcast` exists. Large copies use `try_reserve`, so failure raises `MemoryError` instead of aborting.

| # | Buffer / queue | Owner (thread) | Bound (formula) | Default size | User control (validated cap) | Freed when |
|---|---|---|---|---|---|---|
| R1 | songbird per-SSRC jitter buffer (`PlayoutBuffer`) | songbird UDP-rx (NET) | **64 slots** × ≤ 1460 B: `store_packet` rejects only `desired_index ≥ 64` (`playout_buffer.rs:78-112`), whatever the playout length | ≈ 93 KB per SSRC worst (typically a few packets) | `playout_buffer_ms` 20–200, `playout_spike_ms` 0–200 (they set the drain point, not the cap) | SSRC pruned (`decode_state_timeout_s`; `ClientDisconnect` + 1 s), T4 |
| R2 | songbird events queue (flume, unbounded upstream) | songbird events task | ≈ 1 tick, because `Dispatch` is O(1) and never awaits | ~1 KB | — | continuously drained |
| R3 | tick channel `mpsc<RxInput>(50)` + control overflow (64) | Dispatch → ConnWorker | 50 × (`RawTick` ~200 B + N × `Bytes` refs to ≤ 1460 B packets) | ≤ 50 × N × 1.5 KB (N = 10: 0.73 MB worst) | fixed | consumed; channel dropped at T2/T7 |
| R4 | `SsrcState` (libopus 48 kHz stereo decoder ≈ 26 KB + carry ≤ 120 ms = 46 KB) | RxPipeline (RX) | `max_decoded_users` × 72 KB | 64 × 72 KB = 4.6 MB worst; 72 KB per active speaker | `max_decoded_users` 1–256 (≤ 18.4 MB) | LRU, `UserLeft` + 1 s, idle prune, T7 |
| R5 | decode scratch (5760×2 f32) + per-user tick frames (1920 f32) | RxPipeline | 46 KB + 7.7 KB × users | ~120 KB for 10 users | via R4 | T7 |
| R6 | `MixBus` (frame + limiter + floor state) | RxPipeline | 8 KB × distinct filters | 8 KB | ≤ 64 (subscription cap) | last lane leaves |
| R7 | converters (`TickResampler`: sinc table ≤ 384 taps × L + planar buffers) | RxPipeline | ≤ 25 KB per (source, rate, ch) | ~20 KB | implicit (≤ 64) | refcount 0, T7 |
| R8 | lane accumulator `PcmBuf` | RxPipeline | `chunk_ms × B/ms` per lane | 20 ms × 48 B/ms = 960 B | `chunk_ms` 10–1000 (≤ 384 KB at 48 kHz stereo f32) | lane removed |
| R9 | `SubQueue` (shared `Arc<ChunkData>` per lane; counted once in memory, per subscriber in the budget) | ReceiveSubscription + ConnWorker | ⌈`max_queue_ms`/`chunk_ms`⌉ × (chunk bytes + 160 B meta); Σ over subscriptions ≤ `max_receive_mb` | 2 s × 48 B/ms = 96 KB | `max_queue_ms` `chunk_ms`–60 000; `max_receive_mb` 1–1024; ≤ 64 subscriptions | popped, evicted, `skip_to_live`, close/Drop (freed immediately), T6 |
| R10 | router queue | UserStreams | 64 pending streams | < 8 KB | fixed | router close |
| R11 | `EventQueue` | EventSubscription | `max_queue` × ~64 B | 256 → 16 KB | 1–4096 (≤ 256 KB); ≤ 16 streams | pop, close, T6 |
| R12 | `CtrlQueue` (4 slots: moved, classify, rejoin, closed) + `OpRegistry` | ConnShared | 4 × 48 B + 64 × ~64 B | ~4 KB | fixed | consumed; T8 |
| T1 | `PcmQueue` segments (input rate, i16; gap silence not materialised: 24 B/segment) | Playback + mixer | `max_buffer_ms × rate × ch × 2 B` | 60 s @ 48 kHz stereo (the default format) = 11.5 MB; 60 s @ 24 kHz mono = 2.88 MB | `max_buffer_ms` 100–120 000 (≤ 23 MB @ 48 kHz stereo); ≤ 16 live sources per connection | pull, `clear()` (immediate, even while parked), close, T5 |
| T2 | write staging (parsed `Box<[i16]>`; a copy of a non-`bytes` buffer, §7.3) | caller (transient: freed when the call returns, never retained as scratch) | ≤ one write; the write length is checked against the T1 cap **before** parsing | size of the write | via T1 | moved into T1 / dropped |
| T3 | tag table + rejected set | PcmQueue | 256 × (≤ 128 B + 56 B) + 256 × ≤ 128 B | ≤ 80 KB | tag ≤ 128 B (`InvalidAudio`); 257th tag evicts (§7.7) | eviction (oldest closed, else least-recently-written open); close |
| T4 | mixer-side decoder scratch (`in_i16`, `in_f32`, resampler, `AudioBuffer<f32>` 960 × ch) + the reader's 64 KiB `MediaSourceStream` ring from its per-input probe (§7.1 rule 6) | track (mixer) | ≈ 105 KB per live or memory track | 105 KB | ≤ 16 live sources + ≤ 32 playing tracks per connection | track drop (DisposalThread / inline / Poison) |
| T5 | `ByteRing` (EncodedStream) | EncodedStream + decode thread | `max_buffer_bytes` | 1 MiB | 64 KiB–16 MiB | finish+drain, close, T5 |
| T6 | EncodedStream decoded lookahead (internal `PcmQueue`) | decode thread → mixer | 1 s × 48 kHz × 2 ch × 2 B | 192 KB | fixed | track end |
| T7 | symphonia `MediaSourceStream` ring | decode thread / songbird | 64 KiB | 64 KiB | — | track end |
| T8 | `OpusQueue` + silent-frame reserve | OpusPacketStream + mixer | `max_packets` × ≤ 1275 B + 64 × 3 B | 250 → 319 KB | `max_packets` 1–3000 (≤ 3.8 MB) | pull, clear, close |
| T9 | `AudioFile` bytes (`Arc<[u8]>`, shared by object + tracks) | source object | input size | — | ≤ 1 GiB; `from_path(max_bytes)` default 256 MiB | last ref |
| T10 | `RawPcm` (`Arc<[i16]>` at input rate; `from_samples` at other rates stores 48 kHz, with transient f32 resampling buffers inside `py.detach`) | source object | duration × rate × ch × 2 B | — | input ≤ 1 GiB; `from_samples` rate 8 000–192 000 | last ref |
| T11 | `OpusPackets` (`Arc<[u8]>` + `Arc<[u32]>` offsets) | source object | Σ packet bytes + 4 B/packet | — | ≤ 1 000 000 packets (5.5 h), ≤ 1 GiB | last ref |
| T12 | songbird mixer per Driver (encoder, scratch, packet buffers, `tracks` Vec preallocated to 48), WS/UDP buffers, DAVE state | songbird | fixed by songbird | ~100–300 KB per connection | `mix_mode`, `bitrate` | Poison (T4) |
| T13 | `TrackQueue` pending entries (`SourceSpec` `Arc` + `PlayOpts` + flags; lazy `Encoded` entries hold no buffer) | ConnShared | 10 000 × ~128 B | ~0 | `MAX_QUEUE_ENTRIES` 10 000 (≤ 1.3 MB, sources shared) | advance, `dequeue`, `clear`, T5/T8 |
| T14 | `AudioFormat.silence(ms)` results | caller | `ms × B/ms` | — | `SILENCE_MS` 0–600 000 (≤ 230 MB at 48 kHz stereo f32), `try_reserve` → `MemoryError` | caller drops it |
| G1 | `LOG_RING` | process | 1024 records × ≤ 512 B message (truncated) | ≤ 600 KB | `set_log_levels` | drained by the log thread |
| G2 | doorbell socket buffer | kernel | ≤ 1 byte pending per connection (`pending` flag) | 1 B | — | T9 |
| G3 | threads | process | NET 2 + RX 1 workers (1–8 each); blocking pools ≤ 16 / ≤ 4; ≤ 16 decode threads; 1 DisposalThread; songbird live workers (1 per 16 live mixers, culled after 60 s idle); 1 Python log thread | — | `configure_runtime` | process exit |

**Scaling of songbird live workers.** A paused track still counts in the mixer's `tracks`, and `demote_and_remove_mixers` parks a mixer only when it has no tracks (and no silence frames left) or no connection (`scheduler/live.rs:410-440`). A connection with an **open** `PlaybackStream`, `EncodedStream` or `OpusPacketStream` therefore keeps its mixer live for as long as the stream is open, even while auto-paused and sending no RTP. Live worker threads scale with *connections that hold open live sources* (1 thread per 16), not with active speech. The documentation says so: open a playback for as long as audio is expected (a session, a conversation) rather than per bot. Removing an idle `PcmQueue` track from songbird and re-adding it on the next write, so that the mixer can park, is a possible later optimisation; it would need a new `TrackHandle` per resume, so it is not in 0.6.0.

**Typical interactive streaming connection** (one mixed 24 kHz mono subscription, 8 speakers, one 24 kHz mono playback holding 10 s, one event stream): 96 KB + 8 × 72 KB + ~120 KB + 480 KB + 105 KB + 16 KB + songbird ≈ **1.6 MB**. The worst case per connection at the hard caps is dominated by the user-controlled terms R9 (≤ `max_receive_mb`) and T1 (16 × 23 MB). The documentation flags both.

---

## 11. Error handling and logging

### 11.1 Error model and mapping

sbcore returns `CoreError` (thiserror). The binding converts it to Python exceptions while attached, through `errors.rs`. The exception classes live in `discord/ext/songbird/errors.py` (so they can inherit from both `TimeoutError` and `ValueError`) and are imported lazily into `static EXC_TYPES: [PyOnceLock<Py<PyType>>; N]`, the I1 allow-list.

| Situation | `CoreError` | Python exception |
|---|---|---|
| Connect not complete within `timeout` | — (Python layer) | `ConnectTimeout(ConnectFailed, TimeoutError)`. discord.py's `except asyncio.TimeoutError` then runs `disconnect(force=True)`, which is a no-op because the client is already finalised. |
| Terminal state `Closed(ConnectFailed / SessionInvalid / RejoinFailed / Lost / Kicked)` during `connect`/`move_to` | `ConnectFailed(reason)` | `ConnectFailed(reason, code)` |
| Any native method before `connect()` (`_conn is None`) | — | `NotConnected` |
| Connection-level method after close | `ConnectionClosed(reason)` | `ConnectionClosed(reason)` |
| Read or write on a closed stream, playback or subscription | `StreamClosed(reason)` | `StreamClosed(reason)`. Iterators end with `StopAsyncIteration` instead. |
| Receive overflow under `overflow="error"` | `StreamOverflow` | `StreamOverflow`, raised once after the queued chunks by `read()`, `read_nowait()` **and `__anext__`** (so `async for` does not end silently); afterwards the stream is closed and iteration stops |
| No space for a `*_nowait` write | `PlaybackFull{needed_ms, free_ms}` | `PlaybackFull` (nothing consumed) |
| `listen_*` with `receive=False` | `ReceiveDisabled` | `ReceiveDisabled` |
| Bad format, non-contiguous buffer, buffer format code that does not match the stream encoding (e.g. float32 into pcm16le, big-endian `'>i2'`), wrong Opus frame size, bad tag | `InvalidAudio(msg)` | `InvalidAudio` (also a `ValueError`) |
| Range or cap violation, including `silence(ms)`, `seek(seconds)` (negative, NaN, infinite, > 86 400 s), a write over the buffer cap, `prebuffer_ms`/`max_gap_ms` > `max_buffer_ms/2`, > 32 playing tracks, > 10 000 queue entries | `Limit(LimitError)` | `LimitExceeded(what, value, minimum, maximum)` (also a `ValueError`; `value`/bounds are `int` or `float`) |
| `try_reserve` failure | `Alloc` | `MemoryError` |
| Unrecognised encoded data, missing file | `Source(msg)` | `SourceError`, or `OSError` from `io::Error` for `from_path` |
| songbird `ControlError`; seek/loops on live sources; `seek`/`info` on a not-yet-started queue entry | `Track(msg)` | `TrackError` |
| Arrow member called on a build without the `arrow` feature | `FeatureMissing("arrow")` | `SongbirdError("built without the 'arrow' feature")` |
| `info()`/`seek()` exceeding their timeout (mixer parked) | `OpResult::TimedOut` | `TimeoutError` |
| `configure_runtime` after start; awaiting from a foreign loop | `RuntimeConfigured` | `RuntimeError` |
| Call after `fork()` | `Forked` | `SongbirdError("unusable after fork()")` |
| Caught panic (§5.7) | `Internal(site)` | Never raised from realtime threads. Surfaces as `StreamClosed("internal_error")`, `PlaybackEnded("internal_error")` or a `SongbirdError` from a synchronous call, with an ERROR log that asks for a bug report. |

- **Background failures are delivered as state, never as exceptions in unrelated tasks:**
  - track decode errors → `TrackEnded(reason="error")`, `TrackHandle.wait_ended()` → `TrackEnd(error=…)`;
  - connection loss → `VoiceEvent.Disconnected`, stream `close_reason`, `wait_connected()`;
  - pump-side gateway task failures → logged with `_log.exception`.
- **`disconnect()` never raises** for gateway errors (§6.5).

### 11.2 Logging: pull-based, works without any connection, zero Rust-thread attaches

```
any thread ─ log::Record ─► RingLogger::log  (sbcore::log; tracing→log for songbird)
                              ├─ SHUTTING_DOWN? → return
                              ├─ enabled(): atomic per-target-prefix level table (no locks)
                              ├─ format → LogRecord{level, target, message≤512 B, file, line, unix_ts}
                              ├─ token bucket (50/s, burst 200, atomics) → else dropped += 1
                              ├─ LOG_RING.try_lock() → push (cap 1024) | else dropped += 1      (never blocks)
                              └─ if ring was empty: LOG_WAITER.try_lock() → thread.unpark()     (never blocks)
Python daemon thread "songbird-log" (started at import by _log.start()):
    while not stopping:
        recs = native._log_wait(256, 0.25)   # Rust: py.detach(|| park_timeout(250 ms)) then drain while attached
        for level, target, msg, ts in recs: logging.getLogger(_map(target)).log(level, msg, extra={"rust_target": target})
        if (n := native._log_dropped()): logging.getLogger("discord.ext.songbird.native").warning("%d log records dropped", n)
```

- **Decision (resolving two must-fixes).**
  - Logging never takes a blocking lock from an arbitrary thread: the ring and the waiter both use `try_lock`.
  - Delivery does not depend on a connection or a pump existing; the dedicated Python thread handles it.
  - The forwarder is a **Python-created** thread waiting detached inside a native call, so I2 still holds.
  - pyo3-log is not used. Its attach-on-any-thread model is exactly what the earlier designs had to work around.
- **Shutdown.** `native._shutdown_all()` at atexit sets `SHUTTING_DOWN`, so `log()` becomes a no-op. The thread performs a final drain and exits (joined with a 0.5 s timeout).
- **Logger names:**
  - our crate → `discord.ext.songbird.native.<module>`;
  - songbird targets → `discord.ext.songbird.songbird.<module>`;
  - Python layer → `discord.ext.songbird`.
- **Filtering.** `tracing::span*` records are dropped. Defaults: ours INFO, songbird WARN, rubato WARN. `set_log_levels()` changes the atomic table.
- **Secrets.** `token` is never logged. `session_id` is logged only as a length and `endpoint` only as a host. songbird's own `ConnectionInfo` `Debug` already redacts the token (`info.rs:120-131`).
- **Hot paths.** The mixer never logs (it writes counters only). Per-tick code logs only state transitions, never per packet or per tick. Repeated warnings (decode errors, bad packets, overflow) log the first occurrence, then every 1000th with a count.

### 11.3 Observability

- **`SongbirdClient.stats()`** returns `dict[str, int]`:
  - receive: `ticks_real`, `ticks_synthetic`, `ticks_dropped`, `ticks_skipped`, `bad_packets`, `decode_errors`, `plc_frames`, `carry_overflow_frames`, `dave_trailers_stripped`, `decoders_active`, `subscriptions`, `lanes`;
  - control and health: `dispatch_reinstalls`, `dispatch_panics`, `heartbeat_misses`, `panics`, `connects`, `rejoins`, `stale_connects_dropped`, `gw_unlisted_inputs`, `log_dropped`.
- **`AudioStream.stats()` and `PlaybackStream.stats()`** return frozen snapshots of atomics.
- **`native._debug_live_counts() -> dict[str, int]`** reports live instances of `ConnShared`, `OwnedDriver`, `ConnWorker`, `Dispatch` (it drops when songbird's events task exits on Poison, so this proves events-task teardown), `SubQueue`, `EventQueue`, `PcmQueue`, `LiveReader`, `DoneGuard`, `SongbirdInput` (a sentinel inside every `Input` we hand to songbird, which proves track and mixer-side teardown), `SsrcState`, `TrackFlags`, `DecodeThread`, and `songbird_live_tasks` (= `SCHEDULER.live_tasks()`). It is always compiled in and used by the leak tests. **`SCHEDULER.total_tasks()` is deliberately not reported.** In songbird 0.6.1, culling a parked mixer after Poison (`idle.rs:102,146-150`: `to_cull` → `tasks.remove`) never calls `remove_idle_mixers`; only the live cull path decrements `total` (`live.rs:432,452`). It would never return to baseline.

---

## 12. Rust module/file layout with interface contracts

### 12.1 Workstreams and file ownership

Ownership is disjoint by file. Each workstream writes Rust unit tests in its own files.

| WS | Scope | Files |
|---|---|---|
| **W1** Foundations | workspace, runtime, wake, logging, limits, errors, formats, DSP, binding glue, stubs, invariant script | `Cargo.toml`, `build.rs`, `.cargo/config.toml`, `crates/sbcore/Cargo.toml`, `crates/sbcore/src/{lib,limits,error,runtime,sync,clock,wake,log,format}.rs`, `crates/sbcore/src/dsp/{mod,resample,stream_resample,limiter,g711,mix,level}.rs`, `src/lib.rs`, `src/errors.rs`, `src/convert.rs`, `src/logging.rs`, `src/py/format.rs`, `src/bin/stub_gen.rs`, `scripts/check_invariants.py` |
| **W2** Connection & control | settings, state machine, Driver ownership, Dispatch, ConnWorker loop, ops, events, control queue, tracks and queue | `crates/sbcore/src/{events,ctrl}.rs`, `crates/sbcore/src/conn/{mod,settings,shared,gateway,driver,dispatch,worker,ops,registry}.rs`, `crates/sbcore/src/tracks/{mod,flags,registry,queue}.rs`, `src/py/connection.rs`, `src/py/events.rs`, `src/py/tracks.rs` |
| **W3** Receive | ingestion, extraction, decoding, speaking, buses, converters, lanes, subscriptions, router, chunks, Arrow export | `crates/sbcore/src/rx/{mod,raw,opus,decoder,speaking,bus,convert,lane,queue,router,chunk,pipeline}.rs`, `src/py/receive.rs`, `src/py/chunk.rs`, `src/py/arrow.rs` |
| **W4** Send | PcmQueue engine, mixer reader/decoder, housekeeper, encoded/Opus streams, memory sources | `crates/sbcore/src/tx/{mod,queue,writer,anchor,tags,reader,decoder,housekeep,probe,encoded,opus_stream,memory,source}.rs`, `src/py/playback.rs`, `src/py/sources.rs` |
| **W5** Python layer, tests, CI, docs | package, pump, log thread, test harness, examples, docs, workflows, packaging | `crates/sbcore/src/testing/{mod,harness,rtp,mixer}.rs` (feature `test-hooks`), `src/py/testing.rs`, `py-src/discord/ext/songbird/{__init__,client,_pump,_log,config,formats,streams,playback,tracks,errors}.py`, `tests/**`, `examples/{echo,websocket_bridge,record,music}.py`, `docs/{index,send,receive,migration}.md`, `README.md`, `AGENTS.md`, `justfile`, `pyproject.toml`, `.github/workflows/{ci,release}.yml`, `scripts/smoke.py` |

`py-src/discord/ext/songbird/native/__init__.pyi` is generated by `cargo run --bin stub_gen` and is never edited by hand. stub_gen writes only the flat `native` module.

### 12.2 W1 contracts (`sbcore`: limits, error, runtime, sync, clock, wake, log, format, dsp)

```rust
// limits.rs ────────────────────────────────────────────────────────────────────────────────────────────
pub const TICK_MS: u32 = 20;                    pub const MIX_RATE: u32 = 48_000;
pub const FRAMES_PER_TICK_48K: usize = 960;     pub const MAX_WAKE_IDS: usize = 256;
pub const MAX_SUBS_PER_CONN: usize = 64;        pub const MAX_EVENT_STREAMS_PER_CONN: usize = 16;
pub const MAX_LIVE_SOURCES_PER_CONN: usize = 16; pub const MAX_ENCODED_WORKERS: usize = 16;
pub const MAX_PLAYING_TRACKS_PER_CONN: usize = 32; pub const MAX_QUEUE_ENTRIES: usize = 10_000;
pub const PREALLOCATED_TRACKS: u8 = (MAX_LIVE_SOURCES_PER_CONN + MAX_PLAYING_TRACKS_PER_CONN) as u8;   // 48
pub const MAX_OPS_PER_CONN: usize = 64;         pub const TICK_CHANNEL_CAP: usize = 50;
pub const RX_CTRL_OVERFLOW_CAP: usize = 64;     pub const ROUTER_QUEUE_CAP: usize = 64;
pub const RX_CARRY_MAX_FRAMES: usize = 5_760;   pub const OPUS_SILENT_RESERVE: usize = 64;
pub const MAX_TAGS: usize = 256;                pub const MAX_TAG_LEN: usize = 128;
pub const MAX_REJECTED_TAGS: usize = 256;       pub const HEARTBEAT_MISS: Duration = Duration::from_secs(3);
pub const LOG_RING_CAP: usize = 1024;           pub const LOG_MSG_MAX: usize = 512;
pub const MAX_SOURCE_BYTES: usize = 1 << 30;    pub const MAX_OPUS_PACKETS: usize = 1_000_000;
pub const CHUNK_MS: RangeInclusive<u32> = 10..=1_000;            pub const MAX_QUEUE_MS_MAX: u32 = 60_000;
pub const PLAYOUT_BUFFER_MS: RangeInclusive<u32> = 20..=200;     pub const PLAYOUT_SPIKE_MS: RangeInclusive<u32> = 0..=200;
pub const PLAYBACK_BUFFER_MS: RangeInclusive<u32> = 100..=120_000;
pub const PREBUFFER_MS: RangeInclusive<u32> = 0..=1_000;         pub const IDLE_MS: RangeInclusive<u32> = 20..=60_000;
pub const MAX_GAP_MS: RangeInclusive<u32> = 0..=10_000;          pub const OUTPUT_LATENCY_MS: RangeInclusive<u32> = 0..=1_000;
// cross-field (PlaybackSpec::validate): prebuffer_ms ≤ max_buffer_ms/2 and max_gap_ms ≤ max_buffer_ms/2
pub const HOLD_MS: RangeInclusive<u32> = 0..=10_000;             pub const REJECT_GAP_MS: RangeInclusive<u32> = 0..=10_000;
pub const SILENCE_MS: RangeInclusive<u64> = 0..=600_000;         pub const SEEK_S: RangeInclusive<f64> = 0.0..=86_400.0;
pub const SOURCE_RATE_HZ: RangeInclusive<u32> = 8_000..=192_000; pub const EVENT_QUEUE: RangeInclusive<usize> = 1..=4_096;
pub const ENCODED_BUFFER_BYTES: RangeInclusive<usize> = (64 << 10)..=(16 << 20);
pub const OPUS_STREAM_PACKETS: RangeInclusive<usize> = 1..=3_000;
pub const VOLUME: RangeInclusive<f32> = 0.0..=4.0;
/// Limit values keep their kind: `u128: From<usize>` and `From<f32>` do not exist (checked with rustc 1.99),
/// so a single `Into<u128>` bound cannot cover usize/f32/f64 ranges.
#[derive(Debug, Clone, Copy, PartialEq)] pub enum LimitNum { Int(i128), Float(f64) }
pub trait LimitScalar: Copy + PartialOrd { fn num(self) -> LimitNum; }   // impl for u8, u16, u32, u64, usize, i64, f32, f64
#[derive(Debug, Clone)] pub struct LimitError { pub what: &'static str, pub value: LimitNum, pub min: LimitNum, pub max: LimitNum }
/// `Err` unless `r.contains(&v)`; NaN is never contained, so NaN is rejected too.
pub fn check<T: LimitScalar>(what: &'static str, v: T, r: RangeInclusive<T>) -> Result<T, LimitError>;
pub fn mul_checked(what: &'static str, a: u64, b: u64) -> Result<u64, LimitError>;

// error.rs ─────────────────────────────────────────────────────────────────────────────────────────────
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason { Requested, LeftChannel, Kicked, ConnectFailed, SessionInvalid(Option<u16>), RejoinFailed, Lost,
    Dropped, LoopClosed, ProcessExit, ConnectAborted, Cleanup, Overflow, UserLeft, InternalError, Closed, ConnectionClosed }
impl CloseReason { pub fn as_str(&self) -> &'static str; pub fn code(&self) -> Option<u16>; }
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)] Limit(#[from] LimitError),
    #[error("invalid audio: {0}")] InvalidAudio(Cow<'static, str>),
    #[error("allocation failed")] Alloc,
    #[error("connection closed: {}", .0.as_str())] ConnectionClosed(CloseReason),
    #[error("stream closed: {}", .0.as_str())] StreamClosed(CloseReason),
    #[error("receive overflow")] StreamOverflow,
    #[error("playback buffer full")] PlaybackFull { needed_ms: u64, free_ms: u64 },
    #[error("receive disabled")] ReceiveDisabled,
    #[error("connect failed: {0}")] ConnectFailed(Cow<'static, str>),
    #[error("source: {0}")] Source(String),
    #[error("track: {0}")] Track(Cow<'static, str>),
    #[error("runtime already configured")] RuntimeConfigured,
    #[error("unusable after fork()")] Forked,
    #[error("built without the '{0}' feature")] FeatureMissing(&'static str),
    #[error("internal error at {0}")] Internal(&'static str),
}

// runtime.rs / sync.rs / clock.rs ──────────────────────────────────────────────────────────────────────
pub struct RuntimeConfig { pub net_workers: usize, pub rx_workers: usize }   // 1..=8 each
pub fn configure(cfg: RuntimeConfig) -> Result<(), CoreError>;              // Err(RuntimeConfigured) after first use
pub fn net() -> &'static tokio::runtime::Runtime;
pub fn rx() -> &'static tokio::runtime::Runtime;
pub fn scheduler() -> &'static songbird::driver::Scheduler;                 // created under net().enter()
pub fn disposer() -> &'static songbird::driver::DisposalThread;             // DisposalThread::run()
pub static SHUTTING_DOWN: AtomicBool;  pub static FORKED: AtomicBool;
pub fn live_counts() -> Vec<(&'static str, u64)>;
pub struct LiveCounter(&'static AtomicU64);                                 // RAII inc/dec; one static per tracked type
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Role { Unknown, Python, Net, Rx, Mixer, Decode, Harness }
pub fn role() -> Role;  pub fn set_role(r: Role);
pub fn lock_checked<T>(m: &parking_lot::Mutex<T>) -> parking_lot::MutexGuard<'_, T>;  // debug_assert_ne!(role(), Mixer)
pub struct Shutdown { /* AtomicBool + tokio::sync::Notify */ }
impl Shutdown { pub fn new() -> Arc<Self>; pub fn trigger(&self); pub fn is_set(&self) -> bool; pub async fn wait(&self); }
#[derive(Clone)] pub enum Clock { Real, Manual(Arc<ManualClock>) }
impl Clock { pub fn now(&self) -> Instant; }
pub struct ManualClock { /* base: Instant, offset_us: AtomicU64 */ }
impl ManualClock { pub fn new() -> Arc<Self>; pub fn advance(&self, d: Duration); }

// wake.rs ──────────────────────────────────────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub struct WakeId(pub u8);
pub const WAKE_CONN: WakeId = WakeId(0); pub const WAKE_OPS: WakeId = WakeId(1); pub const WAKE_CTRL: WakeId = WakeId(2);
pub const WAKE_TRACKS: WakeId = WakeId(3);                          // level-triggered track completion (§6.4, §7.10)
pub struct Doorbell { /* cfg(unix): OwnedFd; cfg(windows): std::net::TcpStream */ }
impl Doorbell {
    /// Takes ownership of `raw` (fd or SOCKET); non-blocking; SO_NOSIGPIPE on Apple. Closes on drop.
    pub unsafe fn adopt(raw: i64) -> std::io::Result<Self>;
    pub fn ring(&self);        // MSG_DONTWAIT | MSG_NOSIGNAL (Linux); EAGAIN/EPIPE ignored
    pub fn shutdown(&self);    // SHUT_WR
}
pub struct WakeHub { /* bits: [AtomicU64; 4], used: [AtomicU64; 4], pending: AtomicBool, closed: AtomicBool, bell,
                        pid: u32 (creating process) */ }
impl WakeHub {
    pub fn new(bell: Doorbell) -> Arc<Self>;                         // ids 0..=3 pre-reserved; records getpid()
    pub fn alloc(self: &Arc<Self>) -> Result<WakeToken, LimitError>;
    pub fn notify(&self, id: WakeId);                                // SeqCst (§5.4); never from mixer threads
    pub fn take_ready(&self, out: &mut Vec<u8>);                     // Python only; SeqCst (§5.4)
    pub fn close(&self);  pub fn is_closed(&self) -> bool;          // close(): no shutdown() in a forked child (§4.10)
}
pub struct WakeToken { /* hub: Weak<WakeHub>, id: WakeId */ }
impl WakeToken { pub fn id(&self) -> WakeId; pub fn notify(&self); }   // Drop frees the id (any thread)

// log.rs ───────────────────────────────────────────────────────────────────────────────────────────────
pub struct LogRecord { pub level: log::Level, pub target: Box<str>, pub message: Box<str>,
                       pub file: Option<Box<str>>, pub line: Option<u32>, pub unix_ts: f64 }
pub fn install() -> bool;                                            // false if another logger is installed
pub fn set_level(target_prefix: &str, level: log::LevelFilter);
pub fn wait(timeout: Duration);                                      // registers caller in LOG_WAITER; park_timeout
pub fn drain(max: usize, out: &mut Vec<LogRecord>);
pub fn take_dropped() -> u64;
pub fn shutdown();

// format.rs ────────────────────────────────────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub enum SampleRate { Hz8000, Hz16000, Hz24000, Hz48000 }
impl SampleRate { pub const fn hz(self) -> u32; pub fn from_hz(hz: u32) -> Option<Self>; pub const fn frames_per_tick(self) -> usize; }
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub enum Encoding { Pcm16Le, F32Le, MuLaw, ALaw }
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub struct Format { pub rate: SampleRate, pub channels: u8, pub encoding: Encoding }
impl Format {
    pub fn new(rate_hz: u32, channels: u8, encoding: Encoding) -> Result<Self, CoreError>;   // G.711 ⇒ 8 kHz mono
    pub fn parse(spec: &str) -> Result<Self, CoreError>;
    pub const fn sample_width(self) -> usize;  pub const fn frame_bytes(self) -> usize;
    pub const fn frames_per_tick(self) -> usize;
    pub fn frames_for_ms(self, ms: u32) -> usize;  pub fn bytes_for_ms(self, ms: u32) -> Result<usize, LimitError>;
    pub fn ms_for_frames(self, frames: u64) -> u64;  pub fn silence_byte(self) -> u8;
    pub fn mime_type(self) -> Cow<'static, str>;
    /// check(SILENCE_MS) before any arithmetic, then try_reserve (Alloc → MemoryError).
    pub fn silence(self, ms: u64, out: &mut Vec<u8>) -> Result<(), CoreError>;
}
pub struct PcmBuf { /* words: Box<[u32]>, len: usize — 4-byte aligned for typed views */ }
impl PcmBuf { pub fn zeroed(len: usize) -> Result<Self, CoreError>; pub fn as_bytes(&self) -> &[u8];
              pub fn as_mut_bytes(&mut self) -> &mut [u8]; pub fn len(&self) -> usize; pub fn as_ptr(&self) -> *const u8; }
pub struct PcmParser { /* fmt */ }                                   // stateless: runs OUTSIDE every lock (§7.3)
impl PcmParser {
    pub fn new(fmt: Format) -> Self;
    /// Bytes needed to complete a carried partial frame of `carry_len` bytes.
    pub fn head_len(&self, carry_len: usize) -> usize;
    /// Converts the whole frames of `src` to interleaved i16 (`try_reserve`); returns (frames, trailing partial bytes).
    pub fn parse_frames(&self, src: &[u8], out: &mut Vec<i16>) -> Result<(usize, usize), CoreError>;
}
pub struct Carry { /* bytes: [u8; 8], len: u8, gen: u64 */ }           // the only state under L6w
impl Carry {
    pub fn len(&self) -> usize;
    /// Completes the carried frame with `head` (≤ 7 bytes) into `out`; returns false if no frame was carried.
    pub fn splice(&mut self, p: &PcmParser, head: &[u8], out: &mut [i16]) -> bool;
    pub fn set(&mut self, tail: &[u8], gen: u64);  pub fn reset(&mut self);  pub fn gen(&self) -> u64;
}

// dsp/* ────────────────────────────────────────────────────────────────────────────────────────────────
pub struct TickResampler { /* Option<rubato::SincFixedIn<f32>>, planar in/out, primed output FIFO */ }
impl TickResampler {
    /// Construct OFF the mixer thread (rubato logs in its constructor). from == to ⇒ passthrough.
    pub fn new(from: SampleRate, to: SampleRate, channels: usize) -> Result<Self, CoreError>;
    /// input = from.frames_per_tick()*ch, output = to.frames_per_tick()*ch, interleaved. No alloc, no log.
    pub fn process_tick(&mut self, input: &[f32], output: &mut [f32]);
    pub fn reset(&mut self);  pub fn delay_frames(&self) -> usize;
}
pub struct StreamResampler { /* SincFixedIn, arbitrary ratio, variable chunks; off-mixer only */ }
impl StreamResampler { pub fn new(from_hz: u32, to: SampleRate, channels: usize) -> Result<Self, CoreError>;
                       pub fn process(&mut self, interleaved: &[f32], out: &mut Vec<f32>); pub fn flush(&mut self, out: &mut Vec<f32>); }
pub struct Limiter { /* thr, rel, gain */ }
impl Limiter { pub fn new(rate: u32, threshold_dbfs: f32, release_ms: f32) -> Self;
               pub fn process(&mut self, interleaved: &mut [f32], channels: usize); }
pub mod g711 { pub fn ulaw_encode(x: i16) -> u8; pub fn alaw_encode(x: i16) -> u8;
               pub static ULAW_DECODE: [i16; 256]; pub static ALAW_DECODE: [i16; 256]; }
pub mod mix { pub fn i16_to_f32(src: &[i16], dst: &mut [f32]); pub fn f32_to_i16(src: &[f32], dst: &mut [i16]);
              pub fn downmix_stereo(src: &[f32], dst: &mut [f32]); pub fn upmix_mono(src: &[f32], dst: &mut [f32]);
              pub fn add_into(dst: &mut [f32], src: &[f32]); }
#[derive(Clone, Copy, Default, Debug)] pub struct Level { pub rms_dbfs: f32, pub peak: f32 }
pub fn measure(samples: &[f32]) -> Level;
pub fn encode_from_f32(fmt: Format, src: &[f32], dst: &mut [u8]);     // dst.len() == src.len() * fmt.sample_width()
```

### 12.3 W2 contracts (events, ctrl, conn, tracks)

```rust
// events.rs ────────────────────────────────────────────────────────────────────────────────────────────
#[derive(Clone, Debug)]
pub enum EventData {
    Connected { channel_id: u64, ssrc: u32, timeline_ms: u64 }, Reconnected { channel_id: u64, ssrc: u32, gap_ms: u64 },
    Disconnected { reason: &'static str, code: Option<u16>, will_retry: bool },
    UserMapped { user_id: u64, ssrc: u32 }, UserLeft { user_id: u64 },
    SpeakingStarted { user_id: u64, timeline_ms: u64, captured_at: Instant },
    SpeakingStopped { user_id: u64, timeline_ms: u64, duration_ms: u64 },
    PlaybackStarted { playback_id: u32 }, PlaybackIdle { playback_id: u32, played_ms: u64 },
    PlaybackEnded { playback_id: u32, reason: &'static str },
    TrackEnded { track_id: u64, reason: TrackEndReason }, ReceiveStalled { since_ms: u64 }, Lagged { dropped: u64 },
}
#[derive(Clone, Copy)] pub struct KindMask(pub u16);
impl KindMask { pub const ALL: Self; pub fn of(ev: &EventData) -> u16; }
pub enum Pop<T> { Item(T), Empty, Closed(CloseReason) }
pub struct EventHub { /* queues: Mutex<Vec<Arc<EventQueue>>> (L5h) */ }
impl EventHub {
    pub fn new() -> Arc<Self>;
    pub fn emit(&self, ev: &EventData);                              // NET/RX/Python threads only; no-op once closed
    pub fn subscribe(&self, cap: usize, kinds: KindMask, token: WakeToken) -> Result<Arc<EventQueue>, CoreError>;
    /// One L5h section: mark closed, append `last` (if any) to every queue, close every queue (T6).
    pub fn close_all(&self, last: Option<&EventData>);
}
pub struct EventQueue { /* inner: Mutex<…> (L5q) */ }
impl EventQueue { pub fn try_pop_or_arm(&self) -> Pop<EventData>; pub fn try_pop(&self) -> Pop<EventData>;
                  pub fn close(&self); pub fn dropped(&self) -> u64; pub fn wake_id(&self) -> WakeId; }

// ctrl.rs ──────────────────────────────────────────────────────────────────────────────────────────────
#[derive(Clone, Debug)]
pub enum Control { ChannelMoved { channel_id: u64 }, ClassifyUsers, Rejoin { attempt: u32, channel_id: u64 },
                   Closed { reason: CloseReason } }          // no TrackEnded: completion is level-triggered (WAKE_TRACKS)
pub struct CtrlQueue { /* slots: Mutex<Slots { moved: Option<u64>, classify: bool, rejoin: Option<(u32, u64)>,
                                               closed: Option<CloseReason> }> (L10); nothing is ever dropped */ }
impl CtrlQueue { pub fn new(wake: Arc<WakeHub>) -> Arc<Self>;
                 pub fn set(&self, c: Control);              // latest value wins; Closed is sticky; rings WAKE_CTRL
                 pub fn set_closed(&self, r: CloseReason);
                 pub fn take(&self, out: &mut Vec<Control>); }   // table order of §6.4; Rejoin omitted once Closed

// conn/settings.rs ─────────────────────────────────────────────────────────────────────────────────────
pub struct RawVoiceSettings { pub receive: bool, pub playout_buffer_ms: u32, pub playout_spike_ms: u32,
    pub decode_state_timeout_s: f64, pub speaking_hangover_ms: u32, pub voice_threshold_dbfs: Option<f32>,
    pub max_decoded_users: u32, pub max_receive_mb: u32, pub stall_timeout_ms: u32, pub mix_mode_mono: bool,
    pub bitrate: Option<i32>, pub softclip: bool, pub driver_timeout_s: f64, pub driver_retries: u8,
    pub rejoin_attempts: u8, pub auto_recover: bool }
pub struct VoiceSettings { pub receive: bool, pub playout_packets: NonZeroU8, pub spike_packets: u8,
    pub decode_state_timeout: Duration, pub hangover_ms: u32, pub voice_threshold_dbfs: Option<f32>,
    pub max_decoded_users: usize, pub max_receive_bytes: usize, pub stall_timeout: Duration,
    pub mix_mode: songbird::driver::MixMode, pub bitrate: Option<i32>, pub softclip: bool,
    pub driver_timeout: Duration, pub driver_retries: NonZeroU8, pub rejoin_attempts: u8, pub auto_recover: bool }
impl VoiceSettings { pub fn validate(raw: RawVoiceSettings) -> Result<Self, CoreError>; }
pub struct ConnParams { pub guild_id: NonZeroU64, pub user_id: NonZeroU64, pub reconnect: bool,
                        pub settings: VoiceSettings, pub clock: Clock }

// conn/gateway.rs (pure) ───────────────────────────────────────────────────────────────────────────────
pub enum Phase { Idle, Joining, Connecting { generation: u64 }, Connected, AwaitingMove { deadline: Instant },
                 AwaitingRejoin { attempt: u8, due: Instant }, Rejoining { attempt: u8, deadline: Instant }, Closed(CloseReason) }
pub enum DiscKind { Connect, Reconnect, Runtime }
pub enum DiscReason { AttemptDiscarded, Internal, Io, ProtocolViolation, TimedOut, Requested, WsClosed(Option<u16>), Other }
pub enum ConnectFailure { AttemptDiscarded, TimedOut, Other(Box<str>) }
pub enum GwInput { Join { channel: NonZeroU64, self_mute: bool, self_deaf: bool }, Move { channel: NonZeroU64 }, BeginRejoin, Leave,
    SetFlags { self_mute: bool, self_deaf: bool },
    VoiceState { session_id: Box<str>, channel: Option<u64> }, VoiceServer { token: Box<str>, endpoint: Option<Box<str>> },
    ConnectResult { generation: u64, result: Result<(), ConnectFailure> }, DriverConnect { ssrc: u32 },
    DriverReconnect { ssrc: u32 }, DriverDisconnect { kind: DiscKind, reason: Option<DiscReason> },
    RxStalled { since: Instant }, HandlersLost, Deadline, Close(CloseReason) }
pub enum GwAction { Connect { info: songbird::ConnectionInfo, generation: u64 }, SetSendMute(bool), Emit(EventData),
    Control(Control), RxLink { up: bool, ssrc: Option<u32> }, ReinstallDispatch, Teardown(CloseReason), NotifyStatus }
pub struct GatewayParams { pub guild_id: NonZeroU64, pub user_id: NonZeroU64, pub reconnect: bool, pub rejoin_attempts: u8,
    pub auto_recover: bool, pub move_grace: Duration /*5 s*/, pub server_fallback: Duration /*3 s*/,
    pub rejoin_timeout: Duration /*15 s*/, pub stall_recover: Duration /*5 s*/, pub reinstall_interval: Duration /*5 s*/ }
#[derive(Clone)] pub struct StatusSnapshot { pub state: &'static str, pub channel_id: Option<u64>, pub target_channel_id: Option<u64>,
    pub ssrc: Option<u32>, pub self_mute: bool, pub self_deaf: bool, pub rejoin_attempt: u8, pub generation: u64,
    pub close_reason: Option<CloseReason>, pub close_code: Option<u16> }
pub struct GatewayState { /* phase, target, session, server, prev_server, flags, generation, attempt, leave_requested, ssrc, … */ }
impl GatewayState {
    pub fn new(p: GatewayParams) -> Self;
    pub fn step(&mut self, input: GwInput, now: Instant, out: &mut SmallVec<[GwAction; 4]>);   // pure
    pub fn next_deadline(&self) -> Option<Instant>;
    pub fn status(&self) -> StatusSnapshot;
    pub fn next_action_seq(&mut self) -> u64;                       // monotonic; called under L2 (§4.6)
}
pub struct ActionBatch { pub seq: u64, pub actions: SmallVec<[GwAction; 4]> }
pub fn normalize_endpoint(ep: &str) -> Option<String>;

// conn/driver.rs ───────────────────────────────────────────────────────────────────────────────────────
pub(crate) fn build_config(s: &VoiceSettings, reconnect: bool) -> songbird::Config;   // §4.8 (never retry_limit: None)
pub(crate) struct OwnedDriver { d: songbird::Driver, last_connect_gen: u64 }            // NO Clone, NO Deref
impl OwnedDriver {
    pub(crate) fn new(cfg: songbird::Config) -> Self;                                  // under runtime::net().enter()
    /// None if `generation ≤ last_connect_gen` (stale batch, §4.6).
    pub(crate) fn connect(&mut self, info: songbird::ConnectionInfo, generation: u64) -> Option<songbird::driver::Connect>;
    pub(crate) fn mute(&mut self, m: bool);
    pub(crate) fn set_bitrate(&mut self, b: songbird::driver::Bitrate);
    pub(crate) fn play(&mut self, t: songbird::tracks::Track) -> songbird::tracks::TrackHandle;
    pub(crate) fn stop(&mut self);
    pub(crate) fn install_dispatch(&mut self, d: &Dispatch, receive: bool);            // §4.8 list incl. Heartbeat
    pub(crate) fn reinstall_dispatch(&mut self, d: &Dispatch, receive: bool);         // remove_all_global_events + install
    pub(crate) fn teardown(self);                                                      // T2–T4
}   // every method enters runtime::net()

// conn/dispatch.rs ─────────────────────────────────────────────────────────────────────────────────────
#[derive(Clone)] pub(crate) struct Dispatch(Arc<DispatchInner>);   // see §8.2; impl songbird::events::EventHandler
#[derive(Clone)] pub(crate) struct Heartbeat(Arc<DispatchInner>);  // Periodic(1 s) handler; stores ctr.heartbeat only

// conn/worker.rs ───────────────────────────────────────────────────────────────────────────────────────
pub enum WorkerCmd { Rx(RxCmd), AddSource(Weak<dyn LiveSource>), Link { up: bool, ssrc: Option<u32>, seq: u64 } }
// Link with seq ≤ last applied seq is ignored (§4.6)
pub struct BudgetGuard { /* Weak<WorkerShared>, bytes */ }                         // Drop releases budget
pub struct WorkerShared { /* cmds: Mutex<Vec<WorkerCmd>> (L3), notify: Notify, shutdown: Arc<Shutdown>,
                             known_queues: Mutex<Vec<Weak<SubQueue>>>, known_routers: Mutex<Vec<Weak<RouterQueue>>>,
                             budget_used: AtomicUsize, budget_max: usize */ }
impl WorkerShared { pub fn push(&self, c: WorkerCmd); pub fn reserve_budget(self: &Arc<Self>, bytes: usize) -> Result<BudgetGuard, CoreError>;
                    pub fn register_queue(&self, q: &Arc<SubQueue>); pub fn register_router(&self, q: &Arc<RouterQueue>);
                    pub fn close_queues(&self, r: CloseReason);     // T6, synchronous on the caller thread
                    pub fn shutdown(&self); }
pub struct ConnWorker { /* conn: Weak<ConnShared>, pipeline: RxPipeline, house: Housekeeper, rx_in, overflow, clock */ }
impl ConnWorker {
    pub fn step(&mut self, now: Instant) -> Option<Instant>;   // one iteration: cmds, inputs, timers; returns next wake
    pub fn run(self) -> impl Future<Output = ()> + Send;        // select!{shutdown, rx_in.recv, notify, sleep_until(next)}
}

// conn/ops.rs ──────────────────────────────────────────────────────────────────────────────────────────
pub struct TrackInfoData { pub playing: &'static str, pub volume: f32, pub position_ms: u64, pub play_time_ms: u64, pub loops: Option<u32> }
pub enum OpResult { Info(TrackInfoData), Seeked(Duration), Err(CoreError), TimedOut }
pub struct OpRegistry { /* Mutex<Vec<Option<OpSlot>>> ≤ MAX_OPS_PER_CONN */ }
impl OpRegistry { pub fn start<F>(&self, wake: &Arc<WakeHub>, fut: F) -> Result<u32, CoreError>
                      where F: Future<Output = OpResult> + Send + 'static;   // spawned on runtime::rx()
                  pub fn take_results(&self, out: &mut Vec<(u32, OpResult)>); pub fn fail_all(&self); }

// conn/shared.rs: the facade the binding calls (all synchronous, non-blocking) ─────────────────────────
pub struct ConnShared { /* guild_id, user_id, driver: Mutex<Option<OwnedDriver>> (L1), gw: Mutex<GatewayState> (L2),
    pending: Mutex<VecDeque<ActionBatch>> (L2q), executing: AtomicBool,
    wake: Arc<WakeHub>, events: Arc<EventHub>, ctrl: Arc<CtrlQueue>, tracks: Arc<TrackRegistry>, queue: TrackQueue,
    worker: Arc<WorkerShared>, rx_tx: Option<RxSender>, dispatch: Dispatch, sources: Mutex<Vec<Weak<dyn LiveSource>>>,
    tasks: Mutex<Vec<AbortHandle>> (connect/op tasks only), ops: OpRegistry, bots: Mutex<BotTable>,
    closing: Arc<AtomicBool>, send_muted: Arc<AtomicBool>, pid: u32, clock: Clock, stats */ }
impl ConnShared {
    pub fn new(p: ConnParams, bell: Doorbell) -> Result<Arc<Self>, CoreError>;      // Driver + Dispatch + ConnWorker
    #[cfg(feature = "test-hooks")] pub fn new_detached(p: ConnParams, bell: Doorbell) -> Result<(Arc<Self>, ConnWorker), CoreError>;
    // gateway
    pub fn request_join(&self, channel_id: NonZeroU64, self_mute: bool, self_deaf: bool);
    pub fn request_move(&self, channel_id: NonZeroU64);
    pub fn begin_rejoin(&self);
    pub fn request_leave(&self);
    pub fn on_voice_state(&self, session_id: &str, channel_id: Option<u64>);
    pub fn on_voice_server(&self, token: &str, endpoint: Option<&str>);
    pub fn set_self_flags(&self, self_mute: bool, self_deaf: bool);                 // SetFlags + send_muted mirror
    pub fn status(&self) -> StatusSnapshot;
    pub fn timeline_ms(&self) -> u64;
    // signalling to Python
    pub fn take_ready(&self, out: &mut Vec<u8>);
    pub fn take_controls(&self, max: usize, out: &mut Vec<Control>);
    pub fn take_unclassified_users(&self) -> Vec<u64>;
    pub fn set_users_bot(&self, users: &[(u64, bool)]);
    // receive
    pub fn subscribe(&self, spec: SubSpec) -> Result<Subscription, CoreError>;
    pub fn subscribe_router(&self, spec: RouterSpec) -> Result<RouterHandle, CoreError>;
    pub fn subscribe_events(&self, cap: usize, kinds: KindMask) -> Result<Arc<EventQueue>, CoreError>;
    // send
    pub fn open_playback(&self, spec: PlaybackSpec) -> Result<(Arc<PcmQueue>, TrackRef), CoreError>;
    pub fn open_encoded(&self, spec: EncodedSpec) -> Result<(Arc<EncodedShared>, TrackRef), CoreError>;
    pub fn open_opus_stream(&self, spec: OpusStreamSpec) -> Result<(Arc<OpusQueue>, TrackRef), CoreError>;
    pub fn play(&self, src: &SourceSpec, opts: PlayOpts) -> Result<TrackRef, CoreError>;   // MAX_PLAYING_TRACKS_PER_CONN
    pub fn queue(&self) -> &TrackQueue;
    pub fn queue_take_stream(&self, t: &TrackRef) -> Option<Arc<EncodedShared>>;   // lazy Encoded entry became current
    pub fn stop_all(&self);
    pub fn set_bitrate(&self, bps: Option<i32>) -> Result<(), CoreError>;
    // track ops
    pub fn start_track_info(&self, t: &TrackRef, timeout: Duration) -> Result<u32, CoreError>;
    pub fn start_track_seek(&self, t: &TrackRef, pos: Duration, timeout: Duration) -> Result<u32, CoreError>;
    pub fn take_op_results(&self, out: &mut Vec<(u32, OpResult)>);
    // lifecycle
    pub fn close(&self, reason: CloseReason);                                       // §4.7, idempotent
    pub fn is_closed(&self) -> bool;
    pub fn stats(&self) -> Vec<(&'static str, u64)>;
    pub(crate) fn apply(&self, input: GwInput);                                     // §4.6
    pub(crate) fn poll_tracks(&self, now: Instant);                                 // housekeeping for tracks/queue
}
pub fn registry_snapshot() -> Vec<Arc<ConnShared>>;  pub fn shutdown_all(reason: CloseReason);

// tracks/* ─────────────────────────────────────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum TrackEndReason { Finished, Stopped, Error, Disconnected }
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum TrackKind { File, Pcm, Opus, Playback, EncodedStream, OpusStream }
pub struct TrackFlags { pub id: u64, pub kind: TrackKind /* , uuid: OnceLock<u128>, ended: AtomicU8 (final reason),
                         hints: AtomicU8 (DROPPED | EOF_SEEN | DECODE_ERROR), dropped_at_tick: AtomicU64,
                         reported: AtomicBool, conn_closing: Arc<AtomicBool>, error: OnceLock<Box<str>> */ }
impl TrackFlags {
    /// Authoritative (Dispatch End/Error, T8): sets the final reason unless one is already final; overrides hints.
    pub fn resolve(&self, r: TrackEndReason, error: Option<&str>) -> bool;
    pub fn hint(&self, bit: u8);                         // EOF_SEEN / DECODE_ERROR; atomics only (mixer/disposal-safe)
    pub fn ended(&self) -> Option<TrackEndReason>;  pub fn uuid(&self) -> Option<u128>;
}
pub struct DoneGuard(Arc<TrackFlags>);       // Drop: hint(DROPPED) + dropped_at_tick; atomics only; never final
#[derive(Clone)] pub struct TrackRef { pub flags: Arc<TrackFlags>, pub handle: Arc<OnceLock<songbird::tracks::TrackHandle>> }
pub struct PlayOpts { pub volume: f32, pub loops: Option<u32> /* None = infinite */, pub paused: bool }
pub enum PendingChange { Paused(bool), Volume(f32), Loops(Option<u32>) }
pub struct TrackRegistry { /* Mutex<Vec<Weak<TrackFlags>>>, Mutex<HashMap<u128, Weak<TrackFlags>>> */ }
impl TrackRegistry {
    pub fn register(&self, kind: TrackKind) -> (TrackRef, DoneGuard);
    pub fn bind(&self, r: &TrackRef, h: songbird::tracks::TrackHandle);
    pub fn on_track_event(&self, list: &[(&songbird::tracks::TrackState, &songbird::tracks::TrackHandle)]);  // resolve()
    /// Finalises DROPPED-but-unresolved tracks older than 2 ticks (§7.10) and reports newly resolved ones.
    pub fn poll_ended(&self, tick: u64, out: &mut Vec<(u64, TrackEndReason)>);
    pub fn finish_all(&self, r: TrackEndReason);
    pub fn playing(&self) -> usize;                      // unresolved play()/queue tracks, for MAX_PLAYING_TRACKS_PER_CONN
}
pub struct TrackQueue { /* items: Mutex<VecDeque<(SourceSpec, PlayOpts, TrackRef, DoneGuard)>> ≤ MAX_QUEUE_ENTRIES,
                           current, paused */ }
impl TrackQueue {
    pub fn enqueue(&self, src: SourceSpec, opts: PlayOpts, reg: &TrackRegistry) -> Result<TrackRef, CoreError>;
    /// Index 0 = first PENDING entry; the current track is excluded from len/snapshot/dequeue.
    pub fn current(&self) -> Option<TrackRef>;  pub fn snapshot(&self) -> Vec<TrackRef>;  pub fn len(&self) -> usize;
    pub fn skip(&self);  pub fn stop(&self);  pub fn pause(&self);  pub fn resume(&self);  pub fn clear(&self);
    pub fn dequeue(&self, index: usize) -> Option<TrackRef>;
    /// Pending entries: pause/resume/volume/loops update the stored PlayOpts (false = not pending; use the handle).
    pub fn update_pending(&self, t: &TrackRef, c: PendingChange) -> Result<bool, CoreError>;
    pub(crate) fn advance(&self, conn: &ConnShared);
}
```

### 12.4 W3 contracts (receive)

```rust
// rx/raw.rs ────────────────────────────────────────────────────────────────────────────────────────────
pub enum RawVoice { Packet { ssrc: u32, packet: bytes::Bytes, payload_offset: usize, payload_end: usize }, Lost { ssrc: u32 } }
pub struct RawTick { pub arrived: Instant, pub voices: SmallVec<[RawVoice; 8]>, pub silent: SmallVec<[u32; 16]> }
impl RawTick { pub fn from_voice_tick(t: &songbird::events::context_data::VoiceTick, now: Instant) -> Self; }
pub enum RxInput { Tick(RawTick), Map { ssrc: u32, user_id: u64 }, UserLeft { user_id: u64 }, LinkUp { ssrc: Option<u32> }, LinkDown }
pub type RxSender = tokio::sync::mpsc::Sender<RxInput>;

// rx/opus.rs, rx/decoder.rs, rx/speaking.rs ───────────────────────────────────────────────────────────
pub fn extract_opus(pkt: &[u8], payload_offset: usize, payload_end: usize) -> Option<Range<usize>>;   // §8.3
pub fn is_silent_frame(p: &[u8]) -> bool;
pub struct TickFill { pub real_frames: u32, pub concealed_frames: u32 }
pub struct SsrcState { /* opus::Decoder (48k stereo), carry, user, last_seq, last_seen_tick */ }
impl SsrcState { pub fn new() -> Result<Self, CoreError>;
                 pub fn push_packet(&mut self, opus: &[u8], seq: u16, scratch: &mut [f32]) -> Result<usize, CoreError>;
                 pub fn conceal(&mut self, scratch: &mut [f32]);                     // exactly 960 frames
                 pub fn take_tick(&mut self, out: &mut [f32; 1920]) -> TickFill; pub fn reset(&mut self); }
pub struct SpeakingDetector { /* per-user FSM */ }
impl SpeakingDetector { pub fn new(hangover_ticks: u32) -> Self;
    pub fn on_tick(&mut self, user: u64, voiced: bool, concealed: bool, tick: u64, now: Instant, out: &mut Vec<EventData>);
    pub fn user_left(&mut self, user: u64, tick: u64, out: &mut Vec<EventData>); }

// rx/bus.rs, rx/convert.rs, rx/lane.rs, rx/chunk.rs ──────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub enum MixPolicy { Mix, Floor { release_ms: u32 } }
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct MixFilter { pub include: Option<BTreeSet<u64>>, pub exclude: BTreeSet<u64>, pub ignore_bots: bool, pub policy: MixPolicy }
pub struct UserFrames<'a> { pub users: &'a [(u64, &'a [f32; 1920], bool /*voiced*/)], pub unmapped: Option<&'a [f32; 1920]> }
pub struct BusActivity { pub speakers: SmallVec<[u64; 8]>, pub voiced: bool }
pub struct MixBus { /* frame, Limiter, floor state */ }
impl MixBus { pub fn new(f: MixFilter) -> Self;
              pub fn mix(&mut self, frames: &UserFrames<'_>, bots: &HashSet<u64>, out: &mut [f32; 1920]) -> BusActivity; }
#[derive(Clone, PartialEq, Eq, Hash, Debug)] pub enum SourceKey { User(u64), Bus(u32) }
pub struct Converter { /* channel map + TickResampler 48k → rate */ }
impl Converter { pub fn new(rate: SampleRate, channels: u8) -> Result<Self, CoreError>;
                 pub fn process(&mut self, frame48: &[f32; 1920], out: &mut [f32]); }   // out = rate/50 * ch
pub struct ChunkMeta { pub format: Format, pub user_id: Option<u64>, pub timeline_ms: u64, pub frames: u32, pub speaking: bool,
    pub voiced_ms: u32, pub concealed_ms: u32, pub gap_ms: u32, pub speakers: Box<[u64]>, pub level: Level, pub captured_at: Instant }
pub struct ChunkData { pub meta: ChunkMeta, pub data: PcmBuf }   // Send + Sync + RefUnwindSafe (arrow Allocation)
pub struct Lane { /* key, accumulator PcmBuf, subs: Vec<(SubId, Arc<SubQueue>)> */ }
impl Lane { pub fn new(key: SourceKey, fmt: Format, chunk_ms: u32, start_tick: u64) -> Result<Self, CoreError>;
            pub fn feed(&mut self, tick: u64, converted: &[f32], meta: &TickMeta, out: &mut Vec<Arc<ChunkData>>); }

// rx/queue.rs, rx/router.rs ────────────────────────────────────────────────────────────────────────────
pub type SubId = u64;
#[derive(Clone, Copy)] pub enum Overflow { DropOldest, DropNewest, Error }
pub enum RxSource { User(u64), Mixed(MixFilter) }
pub struct SubSpec { pub source: RxSource, pub format: Format, pub chunk_ms: u32, pub max_queue_ms: u32, pub overflow: Overflow }
impl SubSpec { pub fn validate(&self) -> Result<(), CoreError>; pub fn budget_bytes(&self) -> Result<usize, CoreError>; }
pub struct Delivered { pub chunk: Arc<ChunkData>, pub seq: u64, pub start_ms: u64, pub dropped_ms: u64 }
pub struct SubStats { pub queued_chunks: u64, pub queued_ms: u64, pub delivered: u64, pub dropped_chunks: u64, pub dropped_ms: u64, pub overflowed: bool }
pub struct SubQueue { /* inner: Mutex<…> (L4), token: WakeToken, atomics */ }
impl SubQueue {
    pub fn new(spec: &SubSpec, token: WakeToken) -> Arc<Self>;
    pub(crate) fn push(&self, chunk: Arc<ChunkData>);            // applies overflow; rings after unlock if armed
    pub fn try_pop_or_arm(&self) -> Pop<Delivered>;  pub fn try_pop(&self) -> Pop<Delivered>;
    pub fn drain(&self, max: usize, out: &mut Vec<Delivered>) -> Option<CloseReason>;
    pub fn skip_to_live(&self, reset_clock: bool) -> u64;  pub fn origin_timeline_ms(&self) -> Option<u64>;
    pub fn close(&self, r: CloseReason);  pub fn is_closed(&self) -> bool;
    pub fn stats(&self) -> SubStats;  pub fn wake_id(&self) -> WakeId;
    // Pop: queued items, then Closed(Overflow) is reported ONCE as Err(StreamOverflow), then Closed(..) (§8.9)
}
pub enum RxCmd { AddSub { id: SubId, spec: SubSpec, queue: Arc<SubQueue> }, RemoveSub(SubId), SetFilter(SubId, MixFilter),
                 AddRouter { id: u64, spec: RouterSpec, queue: Arc<RouterQueue> }, RemoveRouter(u64), SetBots(Vec<(u64, bool)>) }
pub struct Subscription { pub id: SubId, pub queue: Arc<SubQueue> /* , worker: Weak<WorkerShared>, budget: BudgetGuard */ }
impl Subscription { pub fn set_filter(&self, f: MixFilter) -> Result<(), CoreError>; pub fn close(&self); }   // Drop ⇒ close
pub struct RouterSpec { pub format: Format, pub chunk_ms: u32, pub max_queue_ms: u32, pub overflow: Overflow,
                        pub include: Option<BTreeSet<u64>>, pub ignore_bots: bool, pub close_on_leave: bool }
pub struct RouterQueue { /* Mutex<VecDeque<Subscription>> cap ROUTER_QUEUE_CAP, token */ }
impl RouterQueue { pub fn try_pop_or_arm(&self) -> Pop<Subscription>; pub fn close(&self, close_streams: bool);
                   pub fn dropped(&self) -> u64; pub fn wake_id(&self) -> WakeId; }
pub struct RouterHandle { pub id: u64, pub queue: Arc<RouterQueue> }                 // Drop ⇒ close(true)

// rx/pipeline.rs (synchronous core; unit-testable without runtime or songbird) ─────────────────────────
pub struct RxConfig { pub enabled: bool, pub hangover_ms: u32, pub voice_threshold_dbfs: Option<f32>,
    pub max_decoded_users: usize, pub decode_state_timeout: Duration, pub stall_timeout: Duration, pub t0: Instant }
pub struct RxTimer { pub next_wake: Option<Instant>, pub stalled_since: Option<Instant> }
pub struct RxCounts { /* ticks_real, ticks_synthetic, ticks_skipped, bad_packets, decode_errors, plc_frames, … */ }
pub struct RxPipeline { /* ssrc map, SsrcStates, detectors, buses, converters, lanes, routers, timeline */ }
impl RxPipeline {
    pub fn new(cfg: RxConfig, events: Arc<EventHub>, ctrl: Arc<CtrlQueue>, unclassified: Arc<Mutex<Vec<u64>>>) -> Self;
    pub fn apply(&mut self, cmd: RxCmd);
    pub fn on_input(&mut self, input: RxInput, now: Instant);
    pub fn on_timer(&mut self, now: Instant) -> RxTimer;          // synthesis, pruning, watchdog
    pub fn timeline_ms(&self) -> u64;
    pub fn close_all(&mut self, r: CloseReason);
    pub fn counts(&self) -> RxCounts;
}
```

### 12.5 W4 contracts (send)

```rust
// tx/mod.rs ────────────────────────────────────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)] pub enum CloseMode { Drain, Abort }
pub trait TrackControl: Send + Sync { fn pause(&self); fn play(&self); fn stop(&self); }
impl TrackControl for songbird::tracks::TrackHandle { /* errors ignored (track may have ended) */ }
#[cfg(feature = "test-hooks")]
pub struct FakeTrackControl { /* log: Mutex<Vec<&'static str>>, flags: Arc<TrackFlags> */ }   // §13.2: records calls;
                                                     // stop() resolves the flags as a Dispatch End(Stop) would
pub trait LiveSource: Send + Sync {
    fn id(&self) -> u32;
    fn housekeep(&self, now: Instant, events: &EventHub) -> HouseStatus;   // RX thread; may lock, ring, emit, pause
    fn on_link(&self, up: bool);
    fn close(&self, mode: CloseMode);
}
pub enum HouseStatus { Live, Finished }
pub struct PullReport { pub consumed: u32, pub real: u32 }
pub trait PcmProvider: Send + Sync + 'static {
    fn format(&self) -> Format;
    fn pull(&self, out: &mut [i16]) -> PullReport;       // MIXER THREAD: try_lock only; no alloc/syscall/log
    fn should_end(&self) -> bool;                        // atomics only
    fn generation(&self) -> u64;
    fn volume(&self) -> f32;
    fn seek_to_frame(&self, _frame: u64) -> Option<u64> { None }
}

// tx/queue.rs (+ writer.rs, anchor.rs, tags.rs) ───────────────────────────────────────────────────────
#[derive(Clone, Copy)] pub enum DisconnectPolicy { Buffer, Discard }
pub struct PlaybackSpec { pub format: Format, pub max_buffer_ms: u32, pub prebuffer_ms: u32, pub idle_ms: u32,
    pub auto_pause: bool, pub max_gap_ms: u32, pub output_latency_ms: u32, pub volume: f32, pub when_disconnected: DisconnectPolicy }
impl PlaybackSpec { pub fn validate(&self) -> Result<(), CoreError>; }   // ranges + prebuffer_ms, max_gap_ms ≤ max_buffer_ms/2
pub struct WriteMeta<'a> { pub tag: Option<&'a str>, pub at_ms: Option<i64> }   // segment tag, source-timeline ms
// PcmQueue::write(&self, data: &[u8], meta: WriteMeta) -> Result<WriteOutcome, CoreError>   (raw bytes only)
pub enum WriteOutcome { Accepted { frames: u64, gap_frames: u64 }, Rejected { frames: u64 } }
pub struct ClearOpts { pub reject_late: bool, pub hold_ms: u32, pub reject_until_gap_ms: Option<u32> }
pub struct ItemProgressData { pub tag: Arc<str>, pub written_ms: u64, pub played_ms: u64, pub heard_ms: u64, pub complete: bool }
pub struct ClearSummary { pub discarded_ms: u64, pub played_ms: u64, pub heard_ms: u64, pub position_ms: u64,
                          pub generation: u64, pub items: Vec<ItemProgressData> }
pub struct PlaybackStatsData { pub written_ms: u64, pub played_ms: u64, pub heard_ms: u64, pub buffered_ms: u64,
    pub position_ms: u64, pub underrun_ms: u64, pub gap_filled_ms: u64, pub rejected_ms: u64, pub discarded_ms: u64,
    pub contended_ticks: u64, pub clears: u64, pub tags_evicted: u64, pub idle: bool }
pub struct PcmQueue { /* §7.2 */ }
impl PcmQueue {
    pub fn new(id: u32, spec: &PlaybackSpec, token: WakeToken, clock: Clock, send_muted: Arc<AtomicBool>)
        -> Result<Arc<Self>, CoreError>;                 // paused_idle = true (live sources start paused)
    pub fn write(&self, data: WriteData<'_>, meta: WriteMeta<'_>) -> Result<WriteOutcome, CoreError>;
        // decode + parse outside locks; L6w splice; L6 commit (§7.3)
    pub fn arm_space(&self, nbytes: usize) -> bool;      // true ⇒ already enough space
    pub fn arm_drain(&self) -> bool;  pub fn arm_idle(&self) -> bool;  pub fn arm_end(&self) -> bool;
    pub fn clear(&self, opts: ClearOpts) -> Result<ClearSummary, CoreError>;   // L6 only
    pub fn end_item(&self, tag: &str);                   // closes the tag, sets ended, releases the prebuffer gate
    pub fn pause(&self, paused: bool);
    pub fn set_volume(&self, v: f32) -> Result<(), CoreError>;  pub fn volume(&self) -> f32;
    pub fn item(&self, tag: &str) -> Option<ItemProgressData>;  pub fn stats(&self) -> PlaybackStatsData;
    pub fn is_closed(&self) -> bool;  pub fn wake_id(&self) -> WakeId;
    pub(crate) fn attach_track(&self, t: Box<dyn TrackControl>);
}
impl PcmProvider for PcmQueue {}  impl LiveSource for PcmQueue {}

// tx/reader.rs, tx/decoder.rs, tx/probe.rs ─────────────────────────────────────────────────────────────
pub struct LiveReader { /* p: Arc<dyn PcmProvider>, track: symphonia_core::formats::Track, ts: u64, _done: DoneGuard,
                           mss: Option<MediaSourceStream> (from its own probe; returned by into_inner) */ }
pub struct PcmDecoder { /* p, params: CodecParameters, in_i16, in_f32, out48, resampler: TickResampler, buf: AudioBuffer<f32>, ramps */ }
// impl songbird::input::core::formats::FormatReader for LiveReader; impl songbird::input::core::codecs::Decoder for PcmDecoder
pub fn live_pcm_input(p: Arc<dyn PcmProvider>, done: DoneGuard, backseek: bool) -> Result<songbird::input::Input, CoreError>;
/// One probe per input, on the creating thread (§7.1 rule 6): metadata for `Parsed.meta` + the drained MSS.
pub fn probe_raw_header(channels: u8)
    -> Result<(songbird::input::core::probe::ProbedMetadata, songbird::input::core::io::MediaSourceStream), CoreError>;

// tx/opus_stream.rs, tx/encoded.rs, tx/memory.rs, tx/source.rs ──────────────────────────────────────────
pub struct OpusStreamSpec { pub max_packets: usize, pub volume: f32 }
pub fn validate_opus_packet(p: &[u8]) -> Result<(), CoreError>;          // 1..=1275 B, 960 samples @ 48 kHz
pub struct OpusQueue { /* Mutex<VecDeque<Box<[u8]>>> (L7), silent reserve, token, atomics, send_muted, paused_idle */ }
impl OpusQueue { pub fn new(spec: &OpusStreamSpec, token: WakeToken, send_muted: Arc<AtomicBool>) -> Result<Arc<Self>, CoreError>;
                 pub fn send(&self, packet: &[u8]) -> Result<(), CoreError>; pub fn arm_space(&self) -> bool;
                 pub fn clear(&self) -> u64; pub fn buffered_ms(&self) -> u64; pub fn wake_id(&self) -> WakeId; }
impl LiveSource for OpusQueue {}
pub fn opus_stream_input(q: Arc<OpusQueue>, done: DoneGuard) -> Result<songbird::input::Input, CoreError>;
pub struct EncodedSpec { pub max_buffer_bytes: usize, pub volume: f32 }    // no hint: symphonia 0.5.5 ignores it
pub struct EncodedShared { /* ByteRing (L8 + Condvar), internal Arc<PcmQueue>, space condvar, token */ }
impl EncodedShared { pub fn write(&self, data: &[u8]) -> Result<(), CoreError>; pub fn arm_space(&self, n: usize) -> bool;
                     pub fn finish(&self); pub fn wake_id(&self) -> WakeId; }
impl LiveSource for EncodedShared {}
pub fn spawn_encoded(spec: EncodedSpec, token: WakeToken, done: DoneGuard)
    -> Result<(Arc<EncodedShared>, songbird::input::Input), CoreError>;   // LimitExceeded beyond MAX_ENCODED_WORKERS
pub struct ProbeInfo { pub codec: String, pub sample_rate: Option<u32>, pub channels: Option<u8>, pub duration_ms: Option<u64> }
pub struct EncodedBytes { /* data: Arc<[u8]>, info */ }
impl EncodedBytes { pub fn new(data: Vec<u8>) -> Result<Self, CoreError>;   // probes eagerly; info from the probe result
                    pub fn info(&self) -> &ProbeInfo; pub fn len(&self) -> usize; }
pub fn read_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>, CoreError>;               // called under py.detach
pub struct MemPcm { /* fmt, samples: Arc<[i16]> */ }
impl MemPcm { pub fn from_bytes(fmt: Format, data: &[u8]) -> Result<Self, CoreError>;
              pub fn from_i16(fmt: Format, samples: Vec<i16>) -> Result<Self, CoreError>;
              /// Any rate in SOURCE_RATE_HZ (pcm16le/f32le); resampled once to 48 kHz unless 8/16/24/48 kHz.
              pub fn from_any_rate(rate_hz: u32, channels: u8, enc: Encoding, data: &[u8]) -> Result<Self, CoreError>;
              pub fn duration_ms(&self) -> u64; }
pub struct OpusFrames { /* bytes: Arc<[u8]>, offsets: Arc<[u32]> */ }
impl OpusFrames { pub fn from_packets<'a>(it: impl IntoIterator<Item = &'a [u8]>) -> Result<Self, CoreError>; pub fn len(&self) -> usize; }
#[derive(Clone)] pub struct EncodedFactory { pub spec: EncodedSpec }       // lazy: nothing allocated until current
#[derive(Clone)] pub enum SourceSpec { File(Arc<EncodedBytes>), Pcm(Arc<MemPcm>), Opus(Arc<OpusFrames>), Encoded(EncodedFactory) }
impl SourceSpec { /// Encoded: spawns ByteRing + decode thread now (only called when the entry becomes current).
                  pub fn to_input(&self, done: DoneGuard, wake: &Arc<WakeHub>)   // allocates a token only for Encoded
                      -> Result<(songbird::input::Input, Option<Arc<EncodedShared>>), CoreError>;
                  pub fn kind(&self) -> TrackKind; pub fn seekable(&self) -> bool; }
pub fn supported_codecs() -> Vec<&'static str>;

// tx/housekeep.rs ──────────────────────────────────────────────────────────────────────────────────────
pub struct Housekeeper { /* sources: Vec<Weak<dyn LiveSource>>, link_up */ }
impl Housekeeper { pub fn new() -> Self; pub fn add(&mut self, s: Weak<dyn LiveSource>); pub fn link(&mut self, up: bool);
                   pub fn tick(&mut self, now: Instant, events: &EventHub) -> Option<Instant>; pub fn close_all(&mut self); }
```

### 12.6 Native module surface (binding ↔ Python-layer contract; all methods synchronous)

| Class / function (module `discord.ext.songbird.native`) | Owner | Members |
|---|---|---|
| `VERSION`, `WAKE_CONN`, `WAKE_OPS`, `WAKE_CTRL`, `WAKE_TRACKS` | W1 | constants |
| `supported_codecs()`, `configure_runtime(net_workers, rx_workers)`, `set_log_level(prefix, level)`, `_log_wait(max, timeout_s) -> list[tuple[int, str, str, float]]`, `_log_dropped() -> int`, `_shutdown_all()`, `_disable_after_fork()`, `_debug_live_counts() -> dict[str, int]` | W1 | module functions |
| `AudioFormat` | W1 | §9.2 (native part: ctor, `parse`, properties, `mime_type`, `bytes_for`, `ms_for`, `silence`) |
| `WakeBell(fd: int)` | W1 | adopts the fd; `taken: bool` |
| `VoiceSettings(**fields)` | W2 | validated settings (`VoiceConfig.to_native()`) |
| `Connection(bell: WakeBell, guild_id: int, user_id: int, settings: VoiceSettings, reconnect: bool)` | W2 | `request_join(channel_id, self_mute, self_deaf)`, `request_move(channel_id)`, `begin_rejoin()`, `request_leave()`, `on_voice_state(session_id, channel_id \| None)`, `on_voice_server(token, endpoint \| None)`, `set_self_flags(mute, deaf)`, `status() -> ConnectionStatus`, `timeline_ms`, `take_ready() -> list[int]`, `take_controls() -> list[Control]`, `take_unclassified_users() -> list[int]`, `set_users_bot(list[tuple[int, bool]])`, `subscribe_user(uid, fmt, chunk_ms, max_queue_ms, overflow) -> ReceiveSubscription`, `subscribe_mixed(fmt, chunk_ms, max_queue_ms, overflow, include, exclude, ignore_bots, policy, floor_release_ms)`, `subscribe_router(...) -> RouterSubscription`, `subscribe_events(max_queue, kind_mask) -> EventSubscription`, `open_playback(...) -> Playback`, `open_encoded(...) -> EncodedSink`, `open_opus(...) -> OpusSink`, `play(source, volume, loops, paused) -> NativeTrack`, `enqueue(source, volume, loops, paused) -> NativeTrack`, `enqueue_stream(volume, loops, paused, max_buffer_bytes) -> NativeTrack`, `queue_take_stream(track) -> EncodedSink \| None`, `queue_current()`, `queue_snapshot()`, `queue_skip()`, `queue_stop()`, `queue_pause()`, `queue_resume()`, `queue_clear()`, `queue_dequeue(i)`, `queue_len()`, `stop_all()`, `set_bitrate(bps \| None)`, `start_track_info(track, timeout_ms) -> int`, `start_track_seek(track, seconds, timeout_ms) -> int`, `take_op_results() -> list[tuple[int, TrackInfo \| float \| BaseException]]`, `close(reason: str)`, `closed`, `stats() -> dict[str, int]` |
| `ConnectionStatus`, `Control` (complex enum), `VoiceEvent` (complex enum, `gen_stub_pyclass_complex_enum`), `EventSubscription` (`try_recv`, `read_nowait`, `wake_id`, `dropped`, `close`) | W2 | |
| `NativeTrack` (`id`, `uuid`, `kind`, `ended`, `end_reason`, `end_error`, `pending`, `pause`, `resume`, `stop`, `set_volume`, `set_loops` (pending entries update their stored `PlayOpts`), `__eq__`, `__hash__`), `TrackInfo` | W2 | |
| `ReceiveSubscription` (`try_recv`, `read_nowait`, `wake_id`, `skip_to_live(reset_clock)`, `origin_timeline_ms`, `set_filter`, `stats`, `close`, `closed`, `close_reason`, `read_batch`), `RouterSubscription` (`try_next`, `wake_id`, `dropped`, `close(close_streams)`), `AudioChunk`, `StreamStats`, `ChunkBatch` | W3 | |
| `Playback` (`write(data, tag, at_ms)` (raw buffer; `str` → `TypeError`), `arm_space(nbytes) -> bool`, `arm_drain()`, `arm_idle()`, `arm_end()`, `clear(reject_late, hold_ms, reject_until_gap_ms)`, `end_item(tag)`, `item(tag)`, `pause()`, `resume()`, `volume`, position properties, `stats()`, `close(drain: bool)`, `closed`, `wake_id`, `playback_id`, `track`), `EncodedSink`, `OpusSink`, `ClearResult`, `ItemProgress`, `PlaybackStats`, `AudioFile` (incl. `from_arrow`), `RawPcm` (incl. `from_arrow`, `from_samples`), `OpusPackets` | W4 | |
| `_testing` (feature `test-hooks`): `detached_connection(guild_id, user_id, settings) -> (Connection, Harness)`, `Harness.step(advance_ms)`, `inject_tick(conn, voices: list[tuple[int, bytes \| None]], silent: list[int])`, `inject_map(conn, ssrc, user_id)`, `inject_user_left(conn, user_id)`, `inject_driver_event(conn, kind, reason, code)`, `inject_voice_tick_rtp(conn, list[tuple[int, bytes]])` (full RTP path), `encode_opus(pcm48_stereo: bytes) -> list[bytes]`, `rtp_wrap(opus, seq, ssrc, *, extension=False, padding=0, dave_trailer=False) -> bytes`, `MixerHarness(playback).pull(ticks) -> bytes` (owns the detached reader/decoder; reproduces songbird's `next_packet` → `decode` order and asserts 960 frames), `track_log(track) -> list[str]` (the `FakeTrackControl` call log), `alloc_count() -> int` (allocations **on the calling thread** while the counting guard is active) | W5 | |

**Stubs.** pyo3-stub-gen 0.23.1 rules:

- Every pyclass uses `module = "discord.ext.songbird.native"`.
- `AudioChunk.__buffer__` is declared with `gen_methods_from_python!`.
- `VoiceEvent` and `Control` use `gen_stub_pyclass_complex_enum`.
- `__getbuffer__`/`__releasebuffer__` and the `_testing` items are `#[gen_stub(skip)]`.
- There are no `Awaitable`/`Future` stub types, because every native method is synchronous.
- The Arrow members are compiled in every build and raise without the feature (§2.4), so the default-feature stubs are valid for every build.
- **Old stubs are deleted, not just left behind.** stub_gen writes only the flat `native/__init__.pyi`. Phase 0 runs `git rm -r py-src/discord/ext/songbird/native/{error,model,player,receive}`, so stale 0.5 stubs (e.g. `native.player`) cannot keep satisfying `ty`. The CI drift check regenerates from an empty directory and fails on any change **or untracked file** (§13.7).

### 12.7 Implementation order

| Phase | Work | Exit criterion |
|---|---|---|
| **0** (days 1–2) | **W1:** workspace and both `Cargo.toml`s; `limits`, `error`, `runtime`, `sync`, `clock`, `wake`, `log`, `format` **types**; empty `#[pymodule(gil_used = false)]`; `check_invariants.py`; CI skeleton. **W2–W4:** commit every §12.3–12.5 signature with bodies returning `CoreError::Internal("unimplemented")`. `todo!()` is denied. **W5:** verify the §6 discord.py line references against **2.7.1 from PyPI** (`uv pip download discord.py==2.7.1`), pin `discord.py>=2.7.1,<3`, bump maturin to 1.15.0, write the pure-Python fake of the native contract (`tests/fakes/native_fake.py`). **W1:** `git rm -r py-src/discord/ext/songbird/native/{error,model,player,receive}` (stale 0.5 stub subpackages). | `cargo check --workspace` and `maturin develop` pass; contracts frozen. |
| **1** (weeks 1–2, parallel) | **W1:** doorbell (Unix and Windows), log ring and `_log_wait`, `TickResampler`/`StreamResampler`/`Limiter`/`g711`/`PcmParser`, buffer conversion, stub_gen. **W2:** `gateway.rs` (pure, table tests first), then `driver`/`dispatch`/`worker`/`shared`, `events`/`ctrl`, `tracks`. **W3:** `opus`/`decoder`/`speaking`, then `bus`/`convert`/`lane`/`queue`/`router`, then `pipeline`, then pyclasses. **W4:** `queue`/`writer`/`anchor`/`tags` (pure tests), then `reader`/`decoder`/`probe`, then `memory`/`source`/`opus_stream`/`encoded`, then `housekeep`, then pyclasses. **W5:** Python layer against the fake; `_pump`, `_log`, client; fake discord.py objects; examples. | Every module's unit tests are green; `cargo test -p sbcore` needs no libpython. |
| **2** (week 3) | Integration: W2 wires W3/W4 into `ConnShared`. W5 swaps the fake for the real module built with `--features test-hooks` and runs the full pytest suite on cp314 and cp314t (Linux, macOS; Windows for `test_wake.py`/`test_receive.py`). Live smoke test against a real guild (`DISCORD_BOT_TOKEN`, `CHANNEL_ID`). | All automated tests green; manual echo, bridge and recording runs pass. |
| **3** (week 4) | Hardening: one-hour Rust soak, 100× connect/disconnect leak loop with every `_debug_live_counts()` entry (our `LiveCounter`s, including `Dispatch` and the `SongbirdInput` sentinel, plus `songbird_live_tasks`) back at baseline, free-threading stress, `scripts/smoke.py` in release.yml, stub drift check, docs/migration guide, README/AGENTS.md rewrite. | Release candidate 0.6.0. |

---

## 13. Test plan

Every test below runs **without a Discord connection** unless it is marked **[live]**.

### 13.1 Rust unit tests

These run in `crates/sbcore` with `cargo test -p sbcore --all-features`. There is no pyo3 dependency, so no libpython is needed, and they run inside manylinux containers too.

**Allocation counting is per thread.** The test-only global allocator increments a `thread_local!` counter, and only while a `CountGuard` is alive on that thread. Assertions compare the calling thread's delta. `cargo test` runs tests in parallel, and the NET and RX runtimes allocate concurrently, so a process-wide counter would flake.

| Module | Tests |
|---|---|
| `limits`, `format` | Range boundaries for `u32`/`u64`/`usize`/`f32`/`f64` ranges via `LimitScalar`; NaN and ±inf rejected; `checked_mul` overflow. `Format::new`/`parse` for every valid and invalid combination (G.711 accepted only at 8 kHz mono). `Format::silence` at `SILENCE_MS` max+1 → `Limit`. `PcmParser`/`Carry` carry partial frames across 1-byte writes for every encoding; a stale-generation carry is dropped; f32 clamp/round. |
| `wake` | `notify_take_no_lost_wakeup`: 4 producer threads × 10⁶ notifies vs. a consumer; every set id is observed, and bytes written ≤ wake-ups + 1. The four `WakeHub` operations use `SeqCst` (also grepped by `check_invariants.py`); correctness rests on the total-order argument in §5.4, and the stress test catches implementation slips. `alloc_free_256_ids`. `ring_after_close_is_noop`. `doorbell_eof_on_close` (Unix socketpair). `epipe_does_not_signal` (close the reader, ring 1000×, process survives with SIGPIPE at default). `close_in_forked_child_skips_shutdown` (pid mismatch → no `shutdown()`). |
| `log` | Producer `try_lock` never blocks: a thread holds the ring lock while 8 threads log, and each log call takes < 50 µs. Drop counting. `wait` returns on push and on shutdown. `SHUTTING_DOWN` short-circuits. |
| `dsp::g711` | All 65 536 encodes and 256 decodes match a port of Sun g711.c bit for bit. Silence bytes. Round-trip SNR ≥ 38 dB at −20 dBFS. |
| `dsp::resample` | For every pair in {8, 16, 24, 48} kHz²: **exactly** `to/50` frames per call over 180 000 ticks (no drift); `delay_frames` ≤ 4.3 ms; 1 kHz passband within 0.2 dB; aliases/images ≤ −120 dB; `reset()` reproduces a fresh instance; **zero allocations** after construction (per-thread counting guard). `StreamResampler` 44.1 → 48 kHz length accounting. |
| `dsp::limiter`, `dsp::mix` | A +4 dBFS input peaks ≤ −1.00 dBFS; unity gain again after 1 s; identical-channel downmix preserves level. |
| `rx::opus` | `extract_opus` on plain, extension, padding, DAVE-leftover (real davey supplemental block), fake `FA FA` (kept) and garbage packets. Fuzz: 10⁶ random packets with a fixed-seed xorshift, no panic, no out-of-bounds access. |
| `rx::decoder` | Concealment is exactly 960 frames. 20/40/60/120 ms packets normalise to 960 frames per tick with the correct carry. Carry overflow drops the oldest data and counts it. A sequence jump > 10 resets the decoder. |
| `rx::speaking` | Start on the first voiced tick; stop after the hangover; `SILENT_FRAME` is not voiced; ≤ 3 concealed ticks keep a spurt alive; `user_left` emits Stopped then Left. |
| `rx::bus`, `rx::lane` | Include/exclude/bots; unmapped audio only when `include` is `None`; floor hold and release; lanes emit chunk sizes exact for every rate × channels × encoding × `chunk_ms` ∈ {10, 20, 30, 40, 100, 1000}; timeline-aligned boundaries; two subscribers share the same `Arc` (`Arc::ptr_eq`); per-user chunks contain only that user's samples (owned length == payload). |
| `rx::queue`, `rx::router` | `drop_oldest`/`drop_newest`/`error` semantics; `dropped_ms` lands on the chunk right after the hole; arm/notify race (2 threads × 10⁵); `close` wakes; capacity ≤ `min(bound, 16)` until needed; `skip_to_live`; router cap and `close_on_leave`. |
| `rx::pipeline` (synthetic `RxInput`, real Opus from `songbird::driver::opus::Encoder`) | Continuous output with no ticks (synthesis from `t0`, before the first tick); 100 ms trigger; `gap_ms`. A burst after a stall: **silent** real ticks are skipped and **voiced ticks are never dropped**. Users are decoded only when needed. `set_filter` re-homes without a gap or duplicate. Watchdog reports `stalled_since` after `stall_timeout`. Removing a subscription frees its lane/converter/bus (`counts()`). **Soak:** 180 000 ticks (1 h) with 3 subscriptions and no consumer; Σ queue bytes ≤ budget at every tick. |
| `conn::gateway` | Table-driven: every row of §4.5, **plus an enumeration of every phase × input pair** asserting that each pair not in the table yields no actions and no phase change. Both update orders; `endpoint=None`; 3 s server fallback; `Move` from Connected, Joining and Connecting; a 4014 `DriverDisconnect` arriving in Joining (old WS after a move) and in Rejoining changes no phase; `VoiceServer` change while Connecting → gen+1; 4014 followed by a move / a kick / grace expiry (with and without reconnect); 4006 rejoin up to the limit; 4016/4017 close without rejoin; `DriverDisconnect{Reconnect}` while Connected; stale `ConnectResult` ignored; `BeginRejoin` and AwaitingRejoin → Rejoining clear `server`, and a `VoiceState` alone in Rejoining issues no `Connect`; Rejoining ignores `VoiceState(None)`; `SetFlags` in every open phase; `Leave` + `VoiceState(None)` → Requested; RxStalled → `ReinstallDispatch` rate-limited, rejoin at 5 s. |
| `conn::shared` / `driver` (tokio, real Driver, no network) | `ConnShared::new` → play a live track (parked mixer) → `close()`. Then `live_counts()` for `Dispatch` (proves the events task exited on Poison), `SongbirdInput`/`LiveReader`/`DoneGuard` (prove mixer-side track teardown), `ConnWorker` and `PcmQueue` reach 0 within `driver_timeout + 1 s`, and `scheduler().live_tasks()` returns to baseline. `total_tasks()` is **not** asserted (§11.3). `close` is idempotent, safe from 8 threads concurrently and from a non-runtime thread. **T6 is synchronous:** right after `close()` returns, every `SubQueue`/router/`EventQueue` is closed, and `Disconnected{will_retry:false}` is the last event, even while the RX runtime is blocked by a test barrier. **Self-deadlock regression:** a terminal transition triggered inside `Dispatch::act` and inside a Python-thread call completes (5 s watchdog). **Apply ordering:** 3 threads × 10⁴ interleaved `apply()` calls; the `Connect` generations reaching the driver and the `RxLink` values reaching the worker are monotonic and end at the final state. `build_config` never yields `retry_limit: None` and sets `preallocated_tracks(48)`. A heartbeat gap > 3 s triggers one rate-limited `ReinstallDispatch` with `receive=false`. |
| `tracks` | **Priority resolution:** a `DoneGuard` drop followed within 2 ticks by `End(PlayMode::End)` → `finished`, by `Errored` → `error`, by `End(Stop)` → `stopped`; a drop with no event → `stopped` after the grace period (or `finished` with `EOF_SEEN`, `error` with `DECODE_ERROR`, `disconnected` while closing); T8 resolves leftovers as `disconnected`. The queue passes only the current entry to songbird; index 0 is the first pending entry; `__getitem__`/`dequeue` bounds; pending-entry `PendingChange`s apply when the entry starts; `seek`/`info` on a pending entry → `Track`; `stop()` = clear + skip; the 10 001st entry → `Limit`; a lazy `Encoded` entry spawns no thread until current; the 33rd playing track → `Limit`. |
| `tx::queue`/`writer`/`anchor`/`tags` (manual clock) | All-or-nothing `PlaybackFull` (carry restored); `space_wanted ≤ cap`. Odd and partial-frame writes. A rejected write consumes nothing (the carry is kept); a write longer than the cap → `Limit` **before** parsing (counting allocator: no allocation). Two threads writing one stream: carry snapshot retry, no torn frame. `clear()` and `close()` complete while another thread is inside a 4 MiB write's parse (they never take L6w). **Gap-fill regression:** 1000 ms played, then 250 ms idle with auto-pause, then a write at `at_ms = 2500` arriving at wall `t0 + 2500` inserts ≤ 20 ms; the same write arriving at `t0 + 1100` inserts 1400 ± 20 ms. Late writes append and re-anchor and never trim. Beyond `max_gap` re-anchors; `gap + frames > cap` re-anchors instead of raising; `i64::MIN`/`i64::MAX` `at_ms` never panic. Gap silence excluded from tag `written`. `PlaybackSpec::validate` rejects `prebuffer_ms` or `max_gap_ms` > `max_buffer_ms/2`. **Prebuffer gate:** opens at `prebuffer_frames`; a segment shorter than `prebuffer_ms` plays after `prebuffer_ticks`; `drain()`/`end_item()` open it at once. **`clear()`:** capacity freed immediately with no pull in between (parked); **an underrun mid-segment (≥ 1 tick with an empty queue, tag still open) still yields that tag in `items`, and its late writes are rejected**; tags close at Active → Idle, so a later clear does not report them; `end_item` → `complete` once played; `played ≤ written`; `reject_late`; `reject_until_gap_ms` rejects contiguous timed writes and accepts the first one after the gap; `hold_ms`; generation bump drops the carry. **Tag overflow:** the 257th tag evicts the oldest closed tag, else the least-recently-written open one, counted in `tags_evicted`. **heard_ms:** after the last pull and an idle-pause, `heard_ms` and `item().heard_ms` reach `played_ms` once `output_latency_ms` of wall time has passed. `when_disconnected="discard"`. |
| `tx::reader`/`decoder` (MixerHarness: `next_packet` → `decode` in songbird's order) | Always 960 frames at 48 kHz for every input rate and channel count; the packet buffer is empty; **zero allocations per pull** (per-thread counting guard); underrun gives zeros and counts; a held L6 gives silence and `contended += 1`; **`send_muted` holds the position** (nothing consumed, `played_ms` unchanged); EOF only after Drain+empty or Abort; prebuffer gate; 2.5 ms declick after clear; 5 ms volume ramp; a panic inside `pull` gives silence then EOF (no unwind); `Drop` sets `reader_gone`; the reader's `into_inner` returns its probe's MSS; `Role` debug assertion fires if `lock_checked` is called on the mixer. |
| `tx::opus_stream`/`memory`/`encoded`/`source` | Silent-frame reserve used on underrun (no allocation while the reserve is non-empty); an Opus stream idle for `idle_ms` pauses its track; packet validation; seek by index; `MemPcm` seek resets the resampler; `MemPcm::from_any_rate` at 44.1 kHz/22.05 kHz/192 kHz gives the expected 48 kHz length and a 1 kHz tone within 0.2 dB; a WAV pushed in random-sized pieces decodes; the encoded track stays paused until its first decoded frame; the decode thread exits on `close()` mid-stream; the 17th encoded stream gets `LimitExceeded`; `EncodedBytes` probe errors. |

### 13.2 Test hooks (`--features test-hooks`, module `native._testing`)

- **`detached_connection(...)`** builds a full `ConnShared` with **no songbird Driver** and a `ManualClock`. Its `Harness.step(advance_ms)` runs `ConnWorker::step` synchronously. Tests are therefore deterministic and need no real time.
  - **No Driver, so no songbird `TrackHandle` and no `End` event.** In detached mode, `open_playback`/`open_encoded`/`open_opus`/`play`/queue advance attach a **`FakeTrackControl`** instead of calling `driver.play`. It records `pause`/`play`/`stop` (readable through `track_log(track)`), and its `stop()` resolves the `TrackFlags` exactly as a `Dispatch` `End(Stop)` would. The songbird `Input` built for the track is parked in the harness. `MixerHarness` takes ownership of a playback's reader/decoder pair and drives it. Dropping the pair (on EOF, `stop()` or `close()`) runs the real `DoneGuard` path, so `TrackHandle.stop()`, `wait_ended()`, `stop_all()` and the reason-priority rules are testable without songbird.
- **Injection.** `inject_tick` / `inject_map` / `inject_user_left` / `inject_driver_event` feed the **same** `RxSender` and `apply()` paths that `Dispatch` uses. Only songbird's UDP layer is bypassed.
  - `inject_voice_tick_rtp` takes full RTP packets built with `rtp_wrap(…, dave_trailer=True)`, so `extract_opus` is exercised end to end.
- **`MixerHarness(playback).pull(n)`** drives the real `LiveReader` + `PcmDecoder` pair exactly as songbird's `mix_symph_indiv` does, and returns the 48 kHz f32 output.
- **`alloc_count()`** exposes the per-thread counting allocator (§13.1), which is installed only under `test-hooks`. It counts only on the calling thread, only inside a counting guard.

### 13.3 Python tests

These use pytest 9.1.1 with pytest-asyncio 1.4.1 (`asyncio_mode = "auto"`) and run on cp314 and cp314t (`PYTHON_GIL=0`).

| File | Covers |
|---|---|
| `tests/conftest.py` | `FakeClient` (`user.id`), `FakeGuild` (`change_voice_state` records every op 4; `get_member`, `get_channel`), `FakeChannel` (`members`, `_get_voice_client_key`). A `connected_vc` fixture drives the voice hooks against a detached connection and injects `DriverConnect`. |
| `test_formats.py` | Validation, `parse`, the neutral presets (exactly the exported set), `bytes_per_ms`, `silence` (600 000 ms accepted, 600 001 → `LimitExceeded`), `mime_type` (incl. stereo), eq/hash; every `listen*` method and `open_playback` default to `PCM16_48K_STEREO`. |
| `test_connect.py` | **Exactly one op 4 carrying `self_mute`/`self_deaf`, and none before it.** On a directly constructed client, `set_self_mute(True)` before `connect(self_mute=False)` sends nothing and the join op 4 carries `self_mute=True` (explicit wins). `set_self_mute` during `connecting` sends nothing until `connected`, then exactly one op 4 with the new flags. Timeout → `ConnectTimeout` (`reason == "timeout"`, `timeout` set), `isinstance(e, asyncio.TimeoutError)`, client unregistered, best-effort leave sent; a second `channel.connect()` does not raise "Already connected". **Timeout → immediate reconnect: the recorded op 4 order is `None` (the stale leave) then the new join**, even when the fake gateway delays the leave by 1 s. `disconnect()` during `connect()` → `ConnectFailed("requested")` and only one leave op 4. **Cancellation** of `connect()` gives the same cleanup. `ConnectFailed` on terminal failure. `disconnect()` is idempotent and calls `cleanup()` even when op 4 raises. `move_to` waits for the new channel. Kick (`VoiceState(None)`) → `cleanup()` + `Disconnected`. A 4014 then a move keeps the connection. `Control.Rejoin` sends op 4 `None` then op 4 `(channel, flags)`. `reconnect()`. `user_id == client.user.id`. |
| `test_receive.py` | N injected ticks give exactly N × 20 / `chunk_ms` chunks, including silence; exact byte lengths for every format; `speaking`/`speakers`/`dropped_ms`/`gap_ms`/`timeline_ms`; synthesis while no ticks arrive (manual clock); `skip_to_live` (with `reset_clock=True` the next chunk has `start_ms == seq == 0`; `origin_timeline_ms` follows); `overflow="error"` makes `async for` raise `StreamOverflow` once after the queued chunks; `set_include` at runtime without a gap; floor policy; `UserStreams` yields and ends on `UserLeft`; `memoryview(c).format == "h"`; `np.shares_memory(np.frombuffer(c, "<i2"), memoryview(c))`; `bytes(c) == c.to_bytes()`; the default format is Discord's native 48 kHz stereo PCM16 (`listen()`, `listen_user()`, `listen_mixed()`, `listen_users()`; 3840-byte 20 ms chunks); a chunk export outlives `stream.close()`; Arrow capsules round-trip through pyarrow 26 and arro3 0.9.1. |
| `test_cancellation.py` | 10 000 × `asyncio.wait_for(anext(stream), tiny)` interleaved with injections: every chunk is received exactly once (by `seq`). The same for `await playback.write()` under `TaskGroup` cancellation, which never writes partially. |
| `test_playback.py` | **An opened but unwritten playback produces no pulls** (`track_log` shows it created paused, nothing else) **and the first write resumes it within one tick.** `open_playback()` defaults to `PCM16_48K_STEREO` (stereo mixer output); `write`/`write_nowait` with bytes/bytearray/memoryview; a `str` raises `TypeError` and consumes nothing; odd byte counts; `PlaybackFull` then `await write()` completes as the harness pulls; `clear()` within one pulled tick; `ClearResult.items[*].heard_ms ≤ written_ms`; a tag that underran mid-stream is still in `items` and its late writes are rejected; `end_item` → `complete`; `reject_late`; `reject_until_gap_ms` with contiguous timed (`at_ms`) writes; `hold_ms`; `at_ms` gap fill including the idle-pause regression with the manual clock; `prebuffer_ms` with a segment shorter than the prebuffer; `set_self_mute(True)` holds `played_ms` and `drain()` waits; `drain()`; `wait_idle()`; `aclose(drain=True)`; `PlaybackStarted`/`PlaybackIdle` events; writes after close raise `StreamClosed`; bytearray/memoryview/numpy int16/`array('h')` accepted for pcm16le; **numpy float32, `array('f')` and `'>i2'` buffers into pcm16le raise `InvalidAudio`**; float32 accepted for f32le. |
| `test_sources.py` | `AudioFile` (WAV fixture; probe metadata; `SourceError` on garbage; `from_path` missing → `OSError`; `load`; `from_arrow` with `pa.array(wav, pa.uint8())` and a one-value `pa.binary()` array); `RawPcm`; `RawPcm.from_samples` at 44 100 Hz stereo float32 (0.5 `RawPCMInput` parity) and from an Arrow Float32 array; `OpusPackets` validation (a 480-sample packet is rejected); `TrackHandle.wait_ended()` resolves after `stop()`, after `stop_all()`, after `Connection.close()` (`disconnected`), and after a 2 s loop stall with 2 000 tracks ending (level-triggered); `seek(-1)`/`seek(nan)`/`seek(inf)` → `LimitExceeded`; `TrackQueue` advance, skip, `stop()`, index 0 = first pending entry, `IndexError`; `enqueue(loops=, paused=)`; `set_volume`/`set_loops`/`pause` on a pending entry apply when it starts; `enqueue_stream(async_gen)` is fed only once current. |
| `test_events.py` | Kind filter; `Lagged` after overflow; `match VoiceEvent.SpeakingStarted(user_id=u)`; `Disconnected(will_retry=False)` is last. |
| `test_wake.py` | Doorbell end to end on the running loop. **Runs on Windows with the Proactor loop in CI.** A failing `Connection(...)` after `WakeBell` closes the fd exactly once (fd count is checked). |
| `test_logging.py` | Records from a Rust thread reach `logging` with **no connection open**; the `songbird-log` thread exits at `_shutdown_all`; the token never appears in captured logs. |
| `test_examples.py` | Imports every example (each guards `bot.run` with `if __name__ == "__main__":`, so importing neither raises `KeyError` nor starts a bot). Runs `record.record()` against a detached connection, and checks that it returns when the connection closes without `!stop`. Runs `echo.echo()` against a detached connection: only voiced chunks are written back (the silence fill is not), the playback uses the default `PCM16_48K_STEREO`, it plays through `MixerHarness`, and `echo()` returns on `!stop` and when the connection closes. Runs `websocket_bridge.bridge(vc, url, input_format=PCM16_16K)` against a local fake **binary echo** WebSocket server (websockets 17.3): every upstream frame is one binary chunk of exactly `bytes_for(chunk_ms)` bytes and no text frame is sent; echoed frames are written to the playback; the server's `"clear"` text frame clears it (`clears == 1`, `discarded_ms > 0`) and later frames still play; the bridge returns `"websocket_closed"` when the server closes and `"stopped"` when its `stop` event is set. |

### 13.4 Leak tests (`tests/test_leaks.py`)

1. **Five cycles of the full lifecycle.** Each cycle uses a real Driver and no network:
   1. The test opens a local TCP listener that accepts and never answers, so a voice-WS attempt against it stays pending. Connecting to a closed port such as `127.0.0.1:9` would instead fail at once and reach `closed` before the test can act.
   2. It starts `client.connect(timeout=60, reconnect=False)` as a task, with `driver_timeout_s=60`, and feeds `VoiceState` + `VoiceServer(endpoint=<listener>)` through the fake guild. The state becomes `connecting`.
   3. While `connecting`, it opens a listen stream, a playback, an event stream and a track (`play(RawPcm(...))`). Streams are valid in every non-closed state.
   4. It calls `disconnect()`, and asserts that the connect task raises `ConnectFailed` with `reason == "requested"` and that exactly one leave op 4 was recorded.
   5. It deletes everything and calls `gc.collect()`, then checks:
      - every `weakref.ref` is dead (client, connection, streams, chunks, playback, track handles);
      - every `native._debug_live_counts()` entry, including `Dispatch`, `SongbirdInput`, `DoneGuard` and `songbird_live_tasks`, is back to baseline within `driver_timeout + 1 s`. Poison waits behind the pending attempt, so the test closes the listener and its accepted sockets to end the attempt sooner. `total_tasks()` is not checked (§11.3).
2. **Abandoned client.** A client dropped without `disconnect()` triggers the weakref callback and teardown, and the counts return to baseline.
3. **Unclosed stream.** A stream dropped without `close()` leaves the worker's subscription list after one tick, and its budget is released.
4. **Closed loop.** Closing the event loop with a live connection makes the pump's `finally` tear it down.
5. **User-built cycle.** A cycle `stream → callback → stream` is collected.
6. **Repeated cycles.** 100 connect/disconnect cycles grow RSS by < 5 MB.

### 13.5 Bounds, stress and soak

- **`test_bounds.py`.** Each case runs **in a subprocess**, and the test asserts the exit code is 0, never 134:
  - `max_queue_ms=10**15`, `max_buffer_ms=10**12`, `chunk_ms=0`/`1001`, `max_packets=10**9`, `max_buffer_bytes=2**62`, `max_decoded_users=10**9`;
  - `PCM16_48K.silence(10**12)`, `F32_48K_STEREO.silence(10**10)`, `silence(-1)`;
  - `seek(-1)`, `seek(float("nan"))`, `seek(float("inf"))`, `seek(1e300)`;
  - `write(bytes(200 << 20))` against a 100 ms buffer (rejected before parsing, no RSS spike);
  - `open_playback(max_buffer_ms=100, prebuffer_ms=1000)` and `open_playback(max_buffer_ms=100, max_gap_ms=10_000)`;
  - `RawPcm.from_samples(b"", 7_999, 2)` and `RawPcm.from_samples(b"", 192_001, 2)`;
  - `driver_retries=0` (rejected, with a message explaining that 0 would mean infinite in songbird);
  - 65 subscriptions, 17 live sources, 33 concurrent `play()` tracks, 10 001 queue entries, and a budget overflow.

  All of these raise `LimitExceeded`/`ValueError` with no RSS spike. µ-law at 16 kHz raises `InvalidAudio`.
- **`test_threads.py`** (3.14t focus, 10 s, also run on GIL builds):
  - `sys._is_gil_enabled()` stays `False` after importing the package, numpy and pyarrow;
  - 8 threads run `write_nowait`/`clear`/`stats` while another thread pulls through `MixerHarness`, and the §7.7 invariants hold;
  - 4 threads `read_nowait` one stream, and each chunk is delivered exactly once;
  - a `close()` from a foreign thread wakes the loop waiter;
  - no deadlock (watchdog);
  - `loop.call_later` jitter p99 < 5 ms while 20 synthetic speakers are being decoded.
- **`test_shutdown.py`.** A subprocess creates connections and exits without disconnecting. It must exit with code 0, with no "Fatal Python error" and no "Future exception was never retrieved".
- **Rust soak (phase 3).** One hour of the pipeline (§13.1) and one hour of `LiveReader` pulls with random writes and clears.

### 13.6 Manual live matrix [live]

Run before release with `examples/` against a real guild, with a DAVE-enabled client and 2 users:

- join / leave / rejoin;
- moderator move;
- kick;
- channel deletion;
- server region change (4015 resume);
- a network outage longer than 10 s (rejoin);
- a 30-minute `websocket_bridge.py` session against a local echo server (latency histogram from `captured_at`);
- interruption: a `"clear"` frame silences playback within 40 ms, and `clear().items` reports the heard amount;
- `echo.py` with two speakers;
- music queue;
- recording, with all WAV files aligned.

### 13.7 CI changes

- **`ci.yml`:**
  - `cargo fmt --check`;
  - `cargo clippy --workspace --all-features --all-targets -D warnings`, identical in the justfile;
  - `cargo test -p sbcore --all-features`;
  - `cargo test -p sbcore --no-default-features --features codec-minimal`;
  - `python scripts/check_invariants.py`;
  - `maturin develop --features test-hooks`, then `pytest` on {3.14, 3.14t} × {ubuntu, macos}, plus `windows-latest` running `test_wake.py` and `test_receive.py`;
  - a stub drift check that also catches deleted and untracked stubs: `rm -rf py-src/discord/ext/songbird/native && cargo run --bin stub_gen && test -z "$(git status --porcelain --untracked-files=all -- py-src/discord/ext/songbird/native)"`;
  - `ruff check`, `ruff format --diff`, `ty check` (covers `examples/`; every example needs only `discord.py` and, for the bridge, `websockets`, both in the dev group).
- **`release.yml`.** The three duplicated smoke one-liners are replaced by `python scripts/smoke.py`:

  ```python
  import io, wave
  import discord.ext.songbird as sb
  f = sb.AudioFormat.parse("pcm16_24k"); assert f.bytes_per_ms == 48 and sb.PCM16_24K == f
  assert sb.AudioFormat.parse("pcm16_48k_stereo") == sb.PCM16_48K_STEREO
  sb.RawPcm(bytes(960), sb.PCM16_24K); sb.OpusPackets([b"\xf8\xff\xfe"])
  buf = io.BytesIO(); w = wave.open(buf, "wb"); w.setnchannels(1); w.setsampwidth(2); w.setframerate(24000)
  w.writeframes(bytes(4800)); w.close(); sb.AudioFile(buf.getvalue())
  assert "opus" in sb.supported_codecs() and not hasattr(sb, "SupportedCodec")
  assert not hasattr(sb.native, "_testing"); sb.native._debug_live_counts()
  ```

  The sdist job still reinstalls with `--no-default-features --features codec-minimal`, which also proves the build works without `arrow`.

---

## 14. Migration notes from 0.5.0 (breaking changes)

1. **Packaging.**
   - Version 0.6.0; `requires-python >= 3.14` is unchanged.
   - `discord.py>=2.7.1,<3` is now a runtime dependency.
   - `pyarrow`, `arro3-*`, `ty`, `audioop-lts` and `typing-extensions` are no longer installed. Use the extras `[arrow]` / `[arro3]`.
   - The build backend is maturin only, and the PyPy classifier is removed.
2. **`SongbirdImpl` is removed.** `SongbirdClient` is a pure-Python `VoiceProtocol` that composes `native.Connection`. Configure it with `SongbirdClient.with_config(VoiceConfig(...))` or `songbird.connect(channel, config=...)`. `update_hook`, `update_server` and `update_state` are internal now; overriding `update_hook` no longer has any effect.
3. **Connect, disconnect and move.**
   - `connect` raises `ConnectTimeout` (a `TimeoutError`) on timeout (it was `PyJoinError`), or `ConnectFailed`. **Any** failure, including cancellation, unregisters the client.
   - The mute/deaf flags travel in the single join op 4. There are no pre-join updates.
   - `disconnect(force=...)` honours `force`, always calls `cleanup()`, and is idempotent.
   - `move_to(channel, timeout=30)` waits until the move completes and updates `.channel`. `move_to(None)` disconnects.
   - New: `reconnect()`, `wait_connected()`, `status`.
   - `SongbirdImpl.stop()` becomes `stop_all()` (it also closes live streams).
4. **Mute and deaf.** `deafen()`/`mute()`/`is_deaf()`/`is_mute()` become `await set_self_deaf()`/`await set_self_mute()` and the `self_deaf`/`self_mute` properties. They never raise when not connected. A value set before `connect()` (on a directly constructed client) wins over `connect()`'s arguments. A value set while connecting is sent in one op 4 as soon as the connection is up (§6.7). While self-muted, live streams hold their position instead of being consumed silently.
5. **Identity.** The bot user id now comes from `client.user.id`, not `application_id`.
6. **Receive is rewritten.**
   - Removed: `listen(sink)`, `SinkBase`, `BufferSink`, `StreamSink`, `Stream`, `VoiceKey`, `VoiceTick`, `PyAsyncIterator`, and the per-tick Arrow `RecordBatch` (`key_kind`, `key_id`, `speaking`, `pcm` at 48 kHz stereo).
   - Use `listen_user()` / `listen_mixed()` / `listen_users()` / `listen()`. They yield `AudioChunk` (buffer protocol) in your format — by default Discord's native 48 kHz stereo PCM16, the format 0.5 delivered — and `events()` provides speaking, user and connection events.
   - Streams are continuous and silence-filled. They wait while empty instead of ending, and they are bounded, so overflow drops audio and reports it as `dropped_ms`.
   - Arrow is opt-in: `chunk.__arrow_c_array__`, `stream.read_batch()`.
7. **Inputs are renamed or replaced:**

   | 0.5.0 | 0.6.0 |
   |---|---|
   | `AudioInput(array)` (pyarrow) | `AudioFile.from_arrow(array)` (UInt8 or one-value Binary array; pyarrow arrays are not buffers). For bytes-like data: `AudioFile(buffer)` / `AudioFile.from_path()` / `await AudioFile.load()` (probed eagerly; there is no `hint`, because symphonia 0.5 ignores it) |
   | `RawPCMInput(float32_array, sample_rate=…, channels=…)` (any rate) | `RawPcm.from_samples(array_or_buffer, sample_rate, channels)` (any rate 8–192 kHz, resampled once at construction). For 8/16/24/48 kHz data in a stream format: `RawPcm(buffer, AudioFormat(...))` (any supported encoding) or `RawPcm.from_arrow()` |
   | `StreamInput(reader)` played now | `vc.open_encoded_stream()` + `await stream.feed_from(reader)` |
   | `vc.enqueue(Track(StreamInput(reader)))` | `vc.queue.enqueue_stream(reader)` (fed and decoded only once it becomes current) |
   | `OpusPacketInput(frames)` | `OpusPackets(packets)` / `OpusPackets.from_arrow()` |
   | `OpusPacketStreamInput(max_packets)` + `send`/`close` | `vc.open_opus_stream(max_packets=)` → `send`/`send_nowait`/`aclose` (underrun → silence; never blocks the mixer) |
   | `InputBase`, `Compose` protocol | removed |
   | — | **new:** `vc.open_playback(fmt)` → `PlaybackStream` for realtime PCM written as it arrives (default format 48 kHz stereo PCM16) |

8. **Tracks.**
   - The `Track` builder is removed. Use `vc.play(source, volume=, loops=, paused=)` or `vc.enqueue(source, volume=, loops=, paused=)`. `play` and `enqueue` are **synchronous** and return a `TrackHandle`; they used to return eager `asyncio.Future`s that were mistyped as coroutines. At most 32 tracks may play at once per connection (`LimitExceeded`).
   - `TrackHandle.seek(timedelta)` becomes `await seek(seconds)`.
   - `play()` becomes `resume()`.
   - `enable_loop`/`disable_loop`/`loop_for` become `set_loops(n | None)`.
   - New: `wait_ended()`, `info()`, `ended`, `end_reason`, `set_volume`, `id`, `uuid`, `__eq__`/`__hash__`.
9. **Queue.** `Queue` becomes `vc.queue: TrackQueue`. Queued tracks are no longer parked inside songbird, and the queue holds at most 10 000 entries.

   | 0.5.0 `Queue` | 0.6.0 `TrackQueue` |
   |---|---|
   | `current()` (method) | `current` (property) |
   | `tracks()` (current first) | `[q.current, *q]`: iteration, `len()` and indexing now cover **pending entries only** (index 0 = next up) |
   | `__getitem__` returning `None` past the end | `IndexError`, so iteration terminates |
   | `__len__` (current + pending) | `len(q)` = pending entries |
   | `is_empty()` | `q.current is None and not q` |
   | `stop()` (stop current, clear queue) | `stop()` (same semantics: `clear()` then `skip()`) |
   | `dequeue(i)` | `dequeue(i)` (pending index; the entry ends with `stopped`) |
   | `skip()`, `pause()`, `resume()` | unchanged |
   | `Track(...).volume(v)` / `.pause()` before enqueueing | `enqueue(src, volume=v, paused=True, loops=…)`; `set_volume`/`set_loops`/`pause`/`resume` also work on pending entries |

   Advancing is not gapless: expect about 20–60 ms between entries (§7.10).
10. **Errors.**
    - `PySongbirdError` → `SongbirdError`, `PyJoinError` → `ConnectFailed`, `PyControlError` → `TrackError`. `PyPlayerError` is removed.
    - The old names remain as deprecated aliases (with a `DeprecationWarning`) for one release.
    - New: `ConnectTimeout`, `NotConnected`, `ConnectionClosed`, `StreamClosed`, `StreamOverflow`, `PlaybackFull`, `ReceiveDisabled`, `InvalidAudio`, `LimitExceeded`, `SourceError`.
11. **Module layout.**
    - `discord.ext.songbird.native` is a single flat module; the submodules `model`, `player`, `receive` and `error` are gone.
    - The hand-written top-level `__init__.pyi` is removed (the Python layer is typed directly).
    - Logger names change from `discord_ext_songbird_native.*` to `discord.ext.songbird.*`.
12. **Defaults.**
    - Receive uses `DecodeMode::Decrypt` plus our own decoder, so DAVE audio is undistorted and packet-loss concealment is exactly 20 ms.
    - The jitter buffer defaults to 60 ms (it was 100 ms); `VoiceConfig.low_latency()` gives 40 ms.
    - Only subscribed users are decoded, and `receive=False` disables decryption entirely.
    - Every receive stream and `open_playback` default to `PCM16_48K_STEREO` (Discord's native format); pass another `AudioFormat` or spec string to have the library convert.
13. **Examples and docs are rewritten.**
    - `basic.py` (correct channel count), `send_stream.py` and `receive_stream.py` are replaced by `echo.py` (receive + playback streaming), `websocket_bridge.py` (a generic raw-PCM WebSocket bridge), `record.py` and `music.py`.
    - README, `docs/send.md`, `docs/receive.md` and AGENTS.md are rewritten. AGENTS.md now has the correct commands (`uv run --no-sync maturin develop`), the correct artifact name, and `CHANNEL_ID` for the examples.

---

## 15. Defect traceability

### 15.1 Defects and how this design makes them impossible

| Defect | 0.5.0 cause | Construction in 0.6.0 that rules it out | Guarding test |
|---|---|---|---|
| **1. Reference-cycle leak** | `Shard::Generic(Arc::new(VoiceUpdater(slf.into_py_any(py))))` stored in songbird `Call.ws`, never cleared. `disconnect` only called `leave()`, so the Driver was never Poisoned. | There is no `Call`, `Shard` or `VoiceUpdate`, and op 4 is sent by Python itself (§6). **sbcore has no pyo3 dependency**, so nothing songbird holds can contain a Python reference (I1, compile-time). Binding pyclasses hold no `Py` fields (`check_invariants.py`). The single `OwnedDriver` (no `Clone`/`Deref`) is taken and torn down exactly once: `remove_all_global_events` → `stop` → `leave` → drop/Poison (T2–T4). That teardown is reachable from `disconnect`, `cleanup`, `Connection` dealloc, the pump weakref and `finally`, terminal states, connect aborts and atexit. | `test_leaks.py` (weakrefs, `_debug_live_counts` incl. the `Dispatch` and `SongbirdInput` sentinels and `SCHEDULER.live_tasks()`), `conn::shared` Rust test |
| **2. Sinks cannot be unregistered; duplicate `listen`** | One `add_global_event` per sink; songbird's untimed `Event::Cancel` is a no-op (`events/store.rs:127-143`); `stop()` only set a flag | **One `Dispatch` per event type**, registered at Driver creation, always returning `None`, never relying on `Cancel`. Subscriptions live in our worker registry, keyed by a fresh `SubId` per call (duplicates are impossible). `close()`/`Drop` remove a subscription, and so does the per-tick `is_closed()` sweep, which does not depend on message delivery. | `test_leaks.py` (3), `rx::pipeline` sweep test |
| **3. Unbounded `BufferSink`; iterator ends when empty** | `max_duration_secs=None` default; VoiceTick every 20 ms; `__anext__` stopped on empty | Every `SubQueue` is bounded (default 2 s, cap 60 s) under a per-connection budget (default 64 MiB), with drop-oldest and counters. Iterators **wait** and end only on close or disconnect. An unconsumed or dropped stream is unregistered. | `rx::pipeline` 1 h soak, `test_bounds.py` |
| **4. `StreamSink` broadcast ring pinned by an idle `_rx`; `retain` never delivered** | `tokio::sync::broadcast` semantics | **No `broadcast` anywhere** (grep in `check_invariants.py`). Each subscriber has its own lazily grown bounded queue. "Retain" is replaced by working `max_queue_ms` buffering plus `skip_to_live()`. | `rx::queue` tests |
| **5. A per-user array pins every speaker's PCM** | Zero-copy slice of the tick's shared buffer | Each `ChunkData` owns a `PcmBuf` holding **only its lane's source**. Arrow export wraps that chunk's own `Arc`, and `ChunkBatch` concatenates owned bytes. | `rx::lane` owned-length test, `test_receive.py` Arrow tests |
| **6. User durations drive eager allocation, aborting the process** | `VecDeque::with_capacity(secs*50)`, `broadcast::channel(secs*50)` | `limits::check` runs before arithmetic, then `checked_mul`, raising `LimitExceeded`. This also covers the one-shot helpers (`AudioFormat.silence(ms)`, `seek(seconds)`, write length). Capacities start at `min(bound, 16)` and grow lazily, so no `with_capacity(user_value)` exists. Large copies use `try_reserve` → `MemoryError`. | `test_bounds.py` (subprocess, exit code ≠ 134) |
| **7. Redundant copies** | Each sink rebuilt its batch; `StreamInput` extracted to a `Vec` and then copied again | `Dispatch` clones `Bytes` handles only. One Opus decode per needed SSRC, one mix per bus, one resample per (source, rate, channels), one encode per lane, and an `Arc<ChunkData>` shared by subscribers. Python reads it through the buffer protocol without copying. Send path: `bytes` is borrowed, there is one parse into the queue, and the mixer does memcpy plus resample. | allocation-counting tests, `Arc::ptr_eq` lane test |
| **8. Deadlock**: `blocking_lock` on the loop vs. a tokio Mutex held across `await update_hook` (which needs the same loop) | tokio Mutex held across a Python await | No tokio mutexes, no `blocking_*`, no lock across `.await`, Python calls or syscalls (I7). Every native method is synchronous. **Rust never awaits Python** (pull model). L2 actions execute after unlock, in step order, through one executor (§4.6), so terminal transitions cannot self-deadlock and actions cannot be reordered. Writers parse outside every lock, and `clear()`/`close()` never wait for a writer (§7.3). Logging never blocks (`try_lock`). | clippy `await_holding_lock`, `check_invariants.py` greps, the §13.1 self-deadlock regression, `test_threads.py` |
| **9. `blocking_recv()` on the shared mixer thread** | `OpusPacketStreamInput::next_packet` waited for the producer; `StreamInput` blocked through `AsyncAdapterStream` | Every mixer-side reader and decoder uses atomics and `try_lock` only. Underrun gives PCM zeros or `SILENT_FRAME`. A packet is **always 960 frames, never 0** (`mix_logic.rs:151` spin) and never `Err` before close+drain. There is no allocation on the PCM path, everything runs under `catch_unwind`, and there are no wake-ups/flume/`TrackHandle` calls on the mixer (housekeeper instead). Encoded streams decode on dedicated threads (≤ 16 per process). At most 32 memory/file tracks play per connection, and `preallocated_tracks(48)` keeps track addition from allocating on the mixer. | MixerHarness tests (zero allocations, contention → silence), `Role` debug assertions, `test_bounds.py` |
| **Other: deafen/mute before join** sent op 4 with `channel_id=None` before `join()` | `Call::deafen`/`mute` always send op 4 (`handler.rs:165-169,371-378`) | No `Call`. A single join op 4 carries `(channel, self_mute, self_deaf)`. `set_self_*` sends op 4 only while connected; otherwise it stores the flag, which wins over `connect()`'s arguments if set first and is flushed in one op 4 on reaching `connected` (§6.7). | `test_connect.py` (op 4 count, order and contents) |
| **Other: `reconnect`/`force` ignored** | Only logged | `reconnect` maps to `driver_retry` (never `None`) plus gateway-level rejoin. `force` suppresses gateway errors in `disconnect`, and teardown always runs. | `conn::gateway` tables, `test_connect.py` |
| **Other: `examples/receive_stream.py` API drift** (`speaking_keys()` on a RecordBatch) | Examples were never executed | Examples are rewritten against the typed API, imported and executed in CI (detached connection, fake WebSocket), and type-checked by `ty`. The stub drift check is in CI. | `test_examples.py`, CI |
| **Other: `Compose::create_async` was `unimplemented!()`/`todo!()`** | Lazy compose placeholders reachable from songbird | There is no `Compose` implementation at all: every input passes `None`. `clippy::todo`/`unimplemented`/`panic`/`unwrap_used` are denied in sbcore. | clippy in CI |

### 15.2 Further 0.5.0 issues (inventory) and songbird hazards

| Issue | Resolution |
|---|---|
| Connect timeout surfaced as `PyJoinError`, so discord.py's cleanup never ran and "Already connected" followed | `ConnectTimeout(ConnectFailed, TimeoutError)`, and `connect()` itself cleans up on any `BaseException` (§6.2) |
| `cleanup()` skipped when native disconnect raised | `cleanup()` runs in `finally`; `disconnect` never raises for gateway errors |
| `move_to` returned before the join completed; `self.channel` not updated | `move_to` awaits `connected` on the new channel; `on_voice_state_update` and `ChannelMoved` update `.channel` |
| `application_id` used as the bot user id | `client.user.id` |
| Future-returning methods typed as `Coroutine`, breaking `create_task` | Native methods are synchronous; public awaitables are real `async def` |
| `Queue.__getitem__` returned `None` (infinite iteration); `VoiceKey` without eq/hash; `PySongbirdError` stub without a base class | `IndexError`; `VoiceKey` removed (plain `int` ids); exceptions defined in Python with correct MROs |
| songbird packet-loss concealment length wrong outside 48 kHz stereo; DAVE leftover fed to the decoder (#315) | `DecodeMode::Decrypt`, our own exact-960-frame concealment, isolated and fuzzed `extract_opus` with a TOC check (§8.3–8.4) |
| songbird's narrow internal resampler for non-48 kHz sources | Every live source reaches songbird at 48 kHz through our `TickResampler` (§7.1) |
| A handler panic wipes all global handlers (RebuildInterconnect); UDP-rx death is silent | `catch_unwind` in `Dispatch`; stall watchdog → `ReinstallDispatch` (rate-limited) → `ReceiveStalled` → auto rejoin (§8.8) |
| Zombie retry after `leave()`; a superseded `Connect` hangs | One Driver per connection, `leave()` only during teardown, then Poison. Connect futures carry a generation and run under a timeout. |
| `TrackHandle` commands and `get_info` hang while the mixer is parked | `info`/`seek` go through `OpRegistry` with timeouts; `PlaybackStream` volume is applied in our decoder |
| songbird counts frames, not content: an unpaused live track of zeros is encoded and sent every 20 ms (`mixer/mod.rs:529-566`) | Live sources start paused and resume on the first write; idle auto-pause for PCM and Opus streams (§4.9, §7.5) |
| Self-mute zeroes the mix only after pulling every track (`mixer/mod.rs:537`) | `send_muted` gate: live sources hold their position while muted (§7.8) |
| Tracks are disposed (inputs dropped) before `End`/`Error` reach global handlers (`mixer/mod.rs:466-493`) | Priority resolution with delayed finalisation instead of first-writer-wins (§7.10) |
| `Scheduler::total_tasks()` is never decremented when a parked mixer is culled (`idle.rs:102,146`) | Leak oracle is `live_tasks()` plus our own `LiveCounter` sentinels (§11.3) |
| The runner reconnects whenever `conn.info != info` (`tasks/mod.rs:78`), so out-of-order `Connect`s end on stale info | Ordered action executor + `last_connect_gen` (§4.6) |
| The old voice WS's 4014 is reported during a move unless a newer attempt already started (`tasks/mod.rs:117-145`) | Explicit no-phase-change rows for Joining/Connecting/AwaitingRejoin/Rejoining (§4.5) |
| `restart_volatile_internals` starts an events runner with no global handlers, also when `receive=False` | `Periodic(1 s)` heartbeat from live and parked mixers → `ReinstallDispatch` (§8.8) |
| `preallocated_tracks` defaults to 1 (reallocation on the mixer) | `preallocated_tracks(48)` (§4.8) |
| `ProbedMetadata` is not `Clone`; `FormatReader::into_inner` needs an MSS; hints are ignored by `Probe::format` and `LiveInput::promote` | One probe per input on the creating thread; no `hint` parameters (§7.1, §7.9) |
| `PlayoutBuffer` holds up to 64 slots per SSRC regardless of the playout length (`playout_buffer.rs:90`) | Budgeted in §10 R1 |

### 15.3 Index of blocking review findings

| Blocking finding | Where it is resolved |
|---|---|
| Never run Teardown/`close()` while holding L2; `close()` checks its flag before any lock | §4.6 apply discipline, §4.7 T0, I5, §13.1 regression test |
| Logging never takes a blocking lock; no logging under REGISTRY; delivery independent of connections | §11.2 (`try_lock` ring and waiter, Python log thread), §4.10 (REGISTRY snapshot then unlock) |
| Mixer: no per-tick allocation; pause/play, doorbell and notifies off the mixer | §7.1 (empty packet + preallocated decoder), §7.4, §7.5 housekeeper, §5.2 |
| No `cast_slice_box` across alignments; never 0 frames | §7.1 rule 7, §7.4 |
| Complete, compilable dependency matrix (`nonmax`, no ArrayVec, exact builder types) | §2.2 (`nonmax 0.5.5`, `smallvec`), §4.8 (`NonZeroU8`, `u8`, `Duration`, `Option<Duration>`) |
| `connect()` catches `BaseException`; timeouts are a `TimeoutError` subclass; `disconnect` idempotent | §6.2, §6.5, §9.9 |
| Re-register Dispatch after interconnect rebuilds (rate-limited watchdog) | §8.8, §4.5 (`RxStalled` → `ReinstallDispatch`) |
| Retry mapping never yields `None` (infinite) | §4.8, §6.8, `driver_retries` 1–10 (§9.3) |
| Correct TrackHandle/End semantics; `SetTrack(None)` drops inline | §7.10, §4.7 T3, §5.2 (Drop impls are atomic-only) |
| Arc-backed chunk payloads for Arrow export | §8.7 `ChunkData`, §8.12 |
| Doorbell: `MSG_NOSIGNAL`/`SO_NOSIGPIPE`; close the fd on construction failure; Windows Proactor tested | §5.4, §13.3 `test_wake.py` |
| Release buffers and finish extraction before Rust locks | I8, §7.3 |
| Receive DSP off songbird's net workers | §5.1 (separate RX runtime) |
| Blocking work off the loop (`from_path` detached); bounded blocking pool | §7.9 (`py.detach`, `load()`), dedicated decode threads (≤ 16 per process) |
| No pyo3-async-runtimes / no Py futures in tokio tasks | §2.3, I1, I2, §5.4 |
| Mixer reader contract (960 frames @ 48 kHz, no `Err` while open, EOF after close+drain, `catch_unwind`) | §7.1 |
| Wall-clock gap filling; never trim speech; anchor reset on `clear()`; regression test | §7.6, §13.1 |
| State machine matches songbird's real event flow (internal resumes, `DriverDisconnect{Reconnect}`, 4014 grace, stale quash) | §4.5, §6.8 |
| No resampler tail across gaps/clear/idle; tags exclude gap silence | §7.2 (input-rate storage, mixer-side resampling), §7.3d, §7.7 |
| Streams/playback/events work in joining/connecting/reconnecting; `set_self_*` before connect stores the flag | §9.4 (only `closed` raises), §6.7 |
| Naming: async `write` plus sync `write_nowait`; real coroutines | §9.7, §5.5 |
| Odd byte counts carried, all-or-nothing `PlaybackFull` (text input superseded: `str` → `TypeError`, Decision 2026-10-27) | §7.3 |
| `clear()` frees capacity immediately (even parked), per-tag played/heard, late rejection, timeline-based rejection for timed writes (`reject_until_gap_ms`, `hold_ms` as fallback) | §7.7 |
| Receive timeline never drops voiced ticks; synthesises silence before the first tick and during outages | §8.8 |
| Latency budget defaults; documented `clear()` and `played_ms` semantics | §3.3, §7.7, §8.1, `VoiceConfig.low_latency()` |
| Every Driver call inside the runtime; Scheduler/DisposalThread created once | §4.8, §5.1 |
| `payload_end_pad` is an END index; DAVE strip isolated, fuzzed, TOC-checked | §8.3 |
| No API songbird cannot back (`latency` removed); `get_info` timeout | §6.9, §7.10 |
| Python-thread lock acquisition on 3.14t is provably safe | §5.3 (invariant stated and enforced structurally) |
| Stubs: `__buffer__`, complex enums, drift check, sdist without `arrow`, smoke script, pyo3-free Rust tests, maturin 1.15.0 | §12.6, §13.7, §2.5 |
| Re-verify discord.py references against 2.7.1 and pin it | Header note, §6 (verified at 2.7.0a5363), §12.7 phase 0 (W5 re-checks 2.7.1), §2.5 |
| Bound blocking-pool usage | §5.1 (`max_blocking_threads` 16/4), §7.9 (≤ 16 decode threads per process) |

### 15.4 Index of second-round review findings

| Finding | Where it is resolved |
|---|---|
| `clear()` missed open items with an empty queue; `complete`/`open` undefined; tag overflow undefined | §7.7 (open-tag lifetime, `end_item`, items = open + recently pulled, eviction rule), §9.7, §13.1 |
| Unwritten live sources transmitted silence | §4.9, §7.3e, §7.5, §7.9 (start paused; Opus idle pause), §13.3 |
| `silence(ms)` sized an allocation from a user number; unbounded compensation in the example | §9.2, §10 T14, §12.2 `SILENCE_MS`, §9.13 (`write_silence` in 1 s pieces), §13.5 |
| Missing state-machine rows (`Move`, `SetFlags`, 4014 outside Connected, Connecting + `VoiceServer`) and no default rule | §4.5 (new rows, default no-op rule), §13.1 (phase × input enumeration) |
| `RawPCMInput` at arbitrary rates lost | §7.9, §9.8 `RawPcm.from_samples`, §1.2, §14 |
| Queue features lost (queued live streams, `loops`/`paused`, pending-entry control, index semantics, `stop`) | §7.10, §9.4, §9.8, §12.3, §12.5 `SourceSpec::Encoded`, §14 item 9 |
| `AudioInput(pyarrow.Array)` migration did not work | §7.9, §8.12, §9.8 `AudioFile.from_arrow`, §14 |
| Examples not importable or not runnable against a fake server; SDK dependencies in examples | §9.10 rule 6, §9.11–§9.13 (no SDKs; the bridge is raw `websockets`), G6, §13.3, §13.7 |
| Leak fixture never reached `connecting`; detached tests had no track control; global allocation counter | §13.2 (`FakeTrackControl`, harness-owned readers), §13.4, §13.1 (per-thread counting) |
| Stale stub subpackages; feature-dependent stubs | §2.4, §12.6, §12.7 phase 0, §13.7 |
| A bridge example blocked waiting for a handshake message; `hold_ms` dropped new replies | §9.12 (no handshake: the uplink starts at once), §7.7 `reject_until_gap_ms` |
| No mapping from a consumer's stream offsets to speakers after `skip_to_live` | §8.9 (`reset_clock`, `origin_timeline_ms`), §9.5 |
| Stale abort-leave op 4 could follow a retry's join | §6.2 (`_pending_leaves`), §13.3 |
| Missing input checks (buffer format codes, write cap before parsing, seek panics, anchor overflow) | §7.3, §7.6, §7.10, §11.1, §13.5 |
| Prebuffer gate never released; prebuffer > buffer | §7.4, §12.2 cross-field rule, §13.1 |
| Unbounded `play()` tracks and queue entries | I6, §7.10, §10 T13, §12.2, §13.5 |
| CtrlQueue drop policy; `TrackEnded`/`Rejoin` could be lost | §6.4 (slots, `WAKE_TRACKS`), §12.3 `CtrlQueue` |
| Self-mute consumed playback as played/heard | §7.8 (`send_muted` gate), §7.4 |
| `StreamOverflow` vs. `StopAsyncIteration`; `ConnectTimeout` constructor | §8.9, §9.5, §9.9, §11.1 |
| Pre-connect flags overwritten by `connect()` arguments | §6.2, §6.7, §13.3 |
| T6 depended on an aborted worker's exit path | §4.3, §4.7 T6/T7, §12.3 `close_queues`/`close_all(last)` |
| Concurrent `apply()` could reorder actions | §4.6 (ordered executor), §5.3 L2 → L2q, §13.1 |
| First-writer-wins mislabelled track ends | §7.10 (priority resolution), §12.3 `TrackFlags`, §13.1 |
| `total_tasks()` is not a valid leak oracle | §11.3, G1, §12.7, §13.4 |
| `AudioStream{input, hint}` does not compile; hints are ignored | §7.9, §9.8, §12.5 (hint parameters removed) |
| `ChannelId::new` does not exist | §4.5 (`From<NonZeroU64>`), §12.3 |
| `check` bound did not compile for `usize`/floats; `extract_opus` indexing | §12.2 (`LimitScalar`, `LimitNum`), §8.3, §9.9 |
| `ProbedMetadata` reuse impossible; `into_inner` unspecified | §7.1 rule 6, §12.5 `probe_raw_header`, §10 T4 |
| WakeHub relied on ordering the model does not guarantee | §5.4 (`SeqCst` on all four operations, enforced by `check_invariants.py`), §13.1 |
| Parsing under L6w | §7.3, §5.3 |
| Gap larger than the buffer could never be written | §7.3c, §7.6, §12.2 cross-field rule |
| `heard_ms` froze while paused | §7.5 pull epoch, §7.7 |
| Lost global handlers undetected with `receive=False` | §4.8, §8.2, §8.8 (heartbeat) |
| Rejoin reused a revoked token; 4014 during rejoin undefined | §4.5 (Rejoining clears `server`; new rows) |
| Fork child could shut the parent's doorbell | §4.7 T9, §4.10, §12.2 `WakeHub.pid` |
| R1 understated; live mixers stay live with open streams | §10 R1, §10 live-worker scaling note, §7.5 |
| `preallocated_tracks` default 1 | §4.8 |
| "Gapless" queue overstated | §7.10, §9.8, §14 |
| Example hangs (`record.py` stopper, a closed WebSocket in the bridge) | §9.12, §9.13, §13.3 |
| `smallvec` pinned to an old version | §2.2 (`=1.16.4`) |
