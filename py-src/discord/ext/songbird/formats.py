"""Raw-audio formats and presets (§9.2).

:class:`AudioFormat` is the native, immutable format class (little-endian, interleaved;
``sample_rate`` ∈ {8000, 16000, 24000, 48000}, ``channels`` ∈ {1, 2}, ``"mulaw"``/``"alaw"`` require
8000 Hz mono). Anywhere a format is expected, a string accepted by :meth:`AudioFormat.parse` works
too (``"pcm16_24k"``, ``"pcm16_48k_stereo"``, ``"f32_48k"``, ``"mulaw"``, ``"pcm16le/24000/1"``).
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
    "GPT_LIVE",
    "OPENAI_REALTIME",
    "GEMINI_LIVE_INPUT",
    "GEMINI_LIVE_OUTPUT",
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
F32_48K: Final[AudioFormat] = AudioFormat(48_000, 1, "f32le")
F32_48K_STEREO: Final[AudioFormat] = AudioFormat(48_000, 2, "f32le")
MULAW_8K: Final[AudioFormat] = AudioFormat(8_000, 1, "mulaw")
ALAW_8K: Final[AudioFormat] = AudioFormat(8_000, 1, "alaw")

GPT_LIVE: Final[AudioFormat] = PCM16_24K
"""``gpt-live-1`` default: PCM16LE 24 kHz mono, one format for input and output."""
OPENAI_REALTIME: Final[AudioFormat] = PCM16_24K
"""OpenAI Realtime (``audio/pcm`` at 24 kHz)."""
GEMINI_LIVE_INPUT: Final[AudioFormat] = PCM16_16K
"""Gemini Live input: PCM16LE 16 kHz mono."""
GEMINI_LIVE_OUTPUT: Final[AudioFormat] = PCM16_24K
"""Gemini Live output: PCM16LE 24 kHz mono."""


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
