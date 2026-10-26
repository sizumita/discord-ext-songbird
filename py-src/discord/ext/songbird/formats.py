"""Raw-audio formats and presets (§9.2).

:class:`AudioFormat` is the native, immutable format class (little-endian, interleaved;
``sample_rate`` ∈ {8000, 16000, 24000, 48000}, ``channels`` ∈ {1, 2}, ``"mulaw"``/``"alaw"`` require
8000 Hz mono). Anywhere a format is expected, a string accepted by :meth:`AudioFormat.parse` works
too (``"pcm16_24k"``, ``"pcm16_48k_stereo"``, ``"f32_48k"``, ``"mulaw"``, ``"pcm16le/24000/1"``).

:data:`PCM16_48K_STEREO` is Discord's native format and the default of every ``listen*`` stream and
of ``open_playback``: audio in that format passes through without resampling or channel conversion.
"""

from __future__ import annotations

from typing import Final, Literal

from . import native

__all__ = [
    "AudioFormat",
    "Encoding",
    "FormatLike",
    "PCM16_8K",
    "PCM16_16K",
    "PCM16_24K",
    "PCM16_48K",
    "PCM16_48K_STEREO",
    "F32_48K",
    "F32_48K_STEREO",
    "MULAW_8K",
    "ALAW_8K",
    "as_format",
]

AudioFormat = native.AudioFormat

type Encoding = Literal["pcm16le", "f32le", "mulaw", "alaw"]
type FormatLike = AudioFormat | str
"""An :class:`AudioFormat` or a specification string for :meth:`AudioFormat.parse`."""

PCM16_8K: Final[AudioFormat] = AudioFormat(8_000, 1, "pcm16le")
PCM16_16K: Final[AudioFormat] = AudioFormat(16_000, 1, "pcm16le")
PCM16_24K: Final[AudioFormat] = AudioFormat(24_000, 1, "pcm16le")
PCM16_48K: Final[AudioFormat] = AudioFormat(48_000, 1, "pcm16le")
PCM16_48K_STEREO: Final[AudioFormat] = AudioFormat(48_000, 2, "pcm16le")
"""Discord's native format (48 kHz stereo PCM16LE): the default of receive streams and playback."""
F32_48K: Final[AudioFormat] = AudioFormat(48_000, 1, "f32le")
F32_48K_STEREO: Final[AudioFormat] = AudioFormat(48_000, 2, "f32le")
MULAW_8K: Final[AudioFormat] = AudioFormat(8_000, 1, "mulaw")
ALAW_8K: Final[AudioFormat] = AudioFormat(8_000, 1, "alaw")


def as_format(fmt: FormatLike) -> AudioFormat:
    """Return ``fmt`` as an :class:`AudioFormat` (strings go through :meth:`AudioFormat.parse`).

    Raises
    ------
    InvalidAudio
        If a string is not a recognised specification.
    TypeError
        If ``fmt`` is neither an ``AudioFormat`` nor a ``str``.
    """
    if isinstance(fmt, AudioFormat):
        return fmt
    if isinstance(fmt, str):
        return AudioFormat.parse(fmt)
    raise TypeError(f"expected an AudioFormat or a format string, got {type(fmt).__name__}")
