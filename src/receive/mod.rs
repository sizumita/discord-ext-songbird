mod handler;
mod identity;
pub mod sink;
pub(crate) mod tick;

pub use handler::HandlerWrapper;
pub(crate) use identity::{VoiceIdentityMap, VoiceIdentityTracker};

pyo3_stub_gen::module_doc!(
    "discord.ext.songbird.native.receive",
    r#"
Voice receive primitives for discord-ext-songbird.

This module provides sink-based APIs to consume decoded PCM audio from voice
connections. The primary entry point is `BufferSink`.

Classes
-------
BufferSink
    Buffering sink that yields Arrow `RecordBatch` snapshots.
StreamSink
    Streaming sink that yields Arrow `RecordBatch` snapshots.
Stream
    Async stream handle returned by `StreamSink.stream()`.
VoiceTick
    Compatibility wrapper for per-tick helpers.
VoiceKey
    Identifier for a voice source (user ID or SSRC).
SinkBase
    Internal sink base class.

Examples
--------
```python
from discord.ext import songbird
from discord.ext.songbird import receive

vc = await channel.connect(cls=songbird.SongbirdClient)
sink = receive.BufferSink(max_duration_secs=5)
vc.listen(sink)

async for batch in sink:
    handle_batch(batch)
```

Notes
-----
Receive iterators return `pyarrow.RecordBatch` values with `key_kind`,
`key_id`, `speaking`, and `pcm` columns. Per-key convenience iterators still
return `pyarrow.Int16Array | None`.
"#,
);
