mod handler;
pub mod sink;
pub(crate) mod tick;

pub use handler::HandlerWrapper;

pyo3_stub_gen::module_doc!(
    "discord.ext.songbird.native.receive",
    r#"
Voice receive primitives for discord-ext-songbird.

This module provides sink-based APIs to consume decoded PCM audio from voice
connections. The primary entry point is `BufferSink`.

Classes
-------
BufferSink
    Buffering sink that yields `VoiceTick` snapshots.
VoiceTick
    Per-tick snapshot of speaking and silent sources.
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
sink = receive.BufferSink(max_in_seconds=5)
vc.listen(sink)

async for tick in sink:
    pcm = tick.get(receive.VoiceKey.User(user_id))
    if pcm is not None:
        handle_pcm(pcm)
```

Notes
-----
PCM is returned as `pyarrow.Int16Array`. Use `VoiceTick.is_silent()` to
distinguish silent from missing keys.
"#,
);
