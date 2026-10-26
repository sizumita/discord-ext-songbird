"""discord.py voice backed by songbird (Rust).

The public API of the package. ``discord.ext.songbird.native`` is internal: its classes are
re-exported here where documented.

Quick start
-----------
>>> from discord.ext import songbird
>>> vc = await songbird.connect(member.voice.channel, config=songbird.VoiceConfig.low_latency())  # doctest: +SKIP
>>> async with vc.listen_mixed() as mic, vc.open_playback() as out:  # 48 kHz stereo PCM16  # doctest: +SKIP
...     async for chunk in mic:
...         out.write_nowait(chunk)
"""

from __future__ import annotations

import atexit
import os
from typing import Any, Final

import discord

from . import _log, errors, native
from ._log import set_log_levels
from .client import SongbirdClient, connect
from .config import VoiceConfig, configure_runtime, supported_codecs
from .errors import (
    ConnectFailed,
    ConnectionClosed,
    ConnectTimeout,
    InvalidAudio,
    LimitExceeded,
    NotConnected,
    PlaybackFull,
    ReceiveDisabled,
    SongbirdError,
    SourceError,
    StreamClosed,
    StreamOverflow,
    TrackError,
)
from .formats import (
    ALAW_8K,
    F32_48K,
    F32_48K_STEREO,
    MULAW_8K,
    PCM16_8K,
    PCM16_16K,
    PCM16_24K,
    PCM16_48K,
    PCM16_48K_STEREO,
    AudioFormat,
)
from .formats import (
    Encoding as Encoding,
)
from .formats import (
    FormatLike as FormatLike,
)
from .playback import ClearResult, EncodedStream, ItemProgress, OpusPacketStream, PlaybackStats, PlaybackStream
from .streams import (
    AudioChunk,
    AudioStream,
    ChunkBatch,
    EventStream,
    StreamStats,
    UserStreams,
    VoiceEvent,
)
from .streams import (
    MixPolicy as MixPolicy,
)
from .streams import (
    Overflow as Overflow,
)
from .tracks import (
    AudioFile,
    OpusPackets,
    RawPcm,
    Source,
    TrackEnd,
    TrackHandle,
    TrackInfo,
    TrackQueue,
)
from .tracks import (
    load_audio_file as load_audio_file,
)

__author__ = "sizumita"
__version__: Final[str] = native.VERSION

type UserLike = int | discord.abc.Snowflake
"""A user id, or a ``Member``/``User``/``Object``, wherever a user is expected."""

ConnectionStatus = native.ConnectionStatus

__all__ = [
    "SongbirdClient",
    "connect",
    "VoiceConfig",
    "ConnectionStatus",
    "configure_runtime",
    "set_log_levels",
    "supported_codecs",
    "AudioFormat",
    "PCM16_8K",
    "PCM16_16K",
    "PCM16_24K",
    "PCM16_48K",
    "PCM16_48K_STEREO",
    "F32_48K",
    "F32_48K_STEREO",
    "MULAW_8K",
    "ALAW_8K",
    "AudioChunk",
    "AudioStream",
    "UserStreams",
    "StreamStats",
    "ChunkBatch",
    "VoiceEvent",
    "EventStream",
    "PlaybackStream",
    "PlaybackStats",
    "ClearResult",
    "ItemProgress",
    "EncodedStream",
    "OpusPacketStream",
    "AudioFile",
    "RawPcm",
    "OpusPackets",
    "Source",
    "TrackHandle",
    "TrackQueue",
    "TrackInfo",
    "TrackEnd",
    "SongbirdError",
    "NotConnected",
    "ConnectFailed",
    "ConnectTimeout",
    "ConnectionClosed",
    "StreamClosed",
    "StreamOverflow",
    "PlaybackFull",
    "ReceiveDisabled",
    "InvalidAudio",
    "LimitExceeded",
    "SourceError",
    "TrackError",
]


def _atexit() -> None:
    # Close every live connection and stop log forwarding after a final drain (§4.10, §11.2).
    try:
        native._shutdown_all()
    finally:
        _log.stop(timeout=0.5)


def _after_fork_in_child() -> None:
    native._disable_after_fork()
    _log._after_fork_in_child()


atexit.register(_atexit)
if hasattr(os, "register_at_fork"):
    os.register_at_fork(after_in_child=_after_fork_in_child)
_log.start()


def __getattr__(name: str) -> Any:
    # Deprecated 0.5 aliases (PySongbirdError, PyJoinError, PyControlError) for one release.
    return errors._deprecated_alias(__name__, name)
