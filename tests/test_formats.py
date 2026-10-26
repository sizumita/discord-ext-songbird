"""AudioFormat validation, parsing, presets and helpers (§9.2, §13.3 ``test_formats.py``)."""

from __future__ import annotations

import base64
from typing import Any

import pytest
from discord.ext import songbird
from discord.ext.songbird import AudioFormat, InvalidAudio, LimitExceeded
from discord.ext.songbird.formats import as_format

PRESETS = {
    "PCM16_8K": (8_000, 1, "pcm16le"),
    "PCM16_16K": (16_000, 1, "pcm16le"),
    "PCM16_24K": (24_000, 1, "pcm16le"),
    "PCM16_48K": (48_000, 1, "pcm16le"),
    "PCM16_48K_STEREO": (48_000, 2, "pcm16le"),
    "F32_48K": (48_000, 1, "f32le"),
    "F32_48K_STEREO": (48_000, 2, "f32le"),
    "MULAW_8K": (8_000, 1, "mulaw"),
    "ALAW_8K": (8_000, 1, "alaw"),
}


@pytest.mark.parametrize(("name", "spec"), PRESETS.items())
def test_presets(name: str, spec: tuple[int, int, Any]) -> None:
    fmt = getattr(songbird, name)
    assert (fmt.sample_rate, fmt.channels, fmt.encoding) == spec
    assert AudioFormat(*spec) == fmt
    assert AudioFormat.parse(name) == fmt
    assert AudioFormat.parse(fmt.mime_type) == fmt


def test_provider_presets() -> None:
    assert songbird.GPT_LIVE == songbird.PCM16_24K
    assert songbird.OPENAI_REALTIME == songbird.PCM16_24K
    assert songbird.GEMINI_LIVE_INPUT == songbird.PCM16_16K
    assert songbird.GEMINI_LIVE_OUTPUT == songbird.PCM16_24K


@pytest.mark.parametrize(
    ("spec", "expected"),
    [
        ("pcm16_24k", (24_000, 1, "pcm16le")),
        ("pcm16_48k_stereo", (48_000, 2, "pcm16le")),
        ("pcm16_48k_mono", (48_000, 1, "pcm16le")),
        ("f32_48k", (48_000, 1, "f32le")),
        ("mulaw", (8_000, 1, "mulaw")),
        ("alaw", (8_000, 1, "alaw")),
        ("g711_ulaw", (8_000, 1, "mulaw")),
        ("pcm16le/24000/1", (24_000, 1, "pcm16le")),
        ("pcm16le/48000/2", (48_000, 2, "pcm16le")),
        ("  PCM16_16K ", (16_000, 1, "pcm16le")),
        ("audio/pcm;rate=24000", (24_000, 1, "pcm16le")),
        ("audio/pcmu", (8_000, 1, "mulaw")),
    ],
)
def test_parse(spec: str, expected: tuple[int, int, str]) -> None:
    f = AudioFormat.parse(spec)
    assert (f.sample_rate, f.channels, f.encoding) == expected


@pytest.mark.parametrize(
    "spec",
    ["pcm16", "pcm16_12k", "pcm16_44.1k", "pcm16_24", "pcm16_24k_quad", "pcm16le/24000/3", "mulaw_16k", "nope", ""],
)
def test_parse_rejects(spec: str) -> None:
    with pytest.raises(InvalidAudio):
        AudioFormat.parse(spec)


@pytest.mark.parametrize(
    "args",
    [(12_000, 1, "pcm16le"), (44_100, 2, "pcm16le"), (24_000, 3, "pcm16le"), (16_000, 1, "mulaw"), (8_000, 2, "alaw")],
)
def test_constructor_rejects(args: tuple[int, int, Any]) -> None:
    with pytest.raises(InvalidAudio) as ei:
        AudioFormat(*args)
    assert isinstance(ei.value, ValueError)


def test_unknown_encoding() -> None:
    with pytest.raises(ValueError):
        AudioFormat(24_000, 1, "opus")  # type: ignore


def test_defaults_and_aliases() -> None:
    assert AudioFormat() == songbird.PCM16_24K
    assert AudioFormat(8_000, 1, "ulaw") == songbird.MULAW_8K  # type: ignore
    assert AudioFormat(48_000, 1, "float32") == songbird.F32_48K  # type: ignore


def test_sizes() -> None:
    assert songbird.PCM16_24K.bytes_per_ms == 48
    assert songbird.PCM16_24K.sample_width == 2
    assert songbird.PCM16_24K.frame_bytes == 2
    assert songbird.F32_48K_STEREO.bytes_per_ms == 384
    assert songbird.F32_48K_STEREO.frame_bytes == 8
    assert songbird.MULAW_8K.bytes_per_ms == 8
    assert songbird.MULAW_8K.sample_width == 1
    assert songbird.PCM16_24K.bytes_for(40) == 1_920
    assert songbird.PCM16_24K.ms_for(4_800) == pytest.approx(100.0)
    with pytest.raises(LimitExceeded):
        songbird.PCM16_24K.bytes_for(-1)


def test_silence() -> None:
    assert songbird.PCM16_24K.silence(10) == bytes(480)
    assert songbird.MULAW_8K.silence(1) == b"\xff" * 8
    assert songbird.ALAW_8K.silence(1) == b"\xd5" * 8
    assert songbird.F32_48K.silence(0) == b""
    assert len(songbird.MULAW_8K.silence(600_000)) == 4_800_000


@pytest.mark.parametrize("ms", [600_001, -1, 10**12])
def test_silence_limits(ms: int) -> None:
    with pytest.raises(LimitExceeded) as ei:
        songbird.PCM16_48K.silence(ms)
    e = ei.value
    assert isinstance(e, ValueError)
    assert e.what == "ms"
    assert e.value == ms
    assert (e.minimum, e.maximum) == (0, 600_000)


@pytest.mark.parametrize("fmt", [songbird.PCM16_24K, songbird.MULAW_8K, songbird.ALAW_8K, songbird.F32_48K_STEREO])
@pytest.mark.parametrize("ms", [0, 1, 7, 40])
def test_silence_base64(fmt: AudioFormat, ms: int) -> None:
    assert fmt.silence_base64(ms) == base64.b64encode(fmt.silence(ms)).decode()


def test_silence_base64_limit() -> None:
    with pytest.raises(LimitExceeded):
        songbird.F32_48K_STEREO.silence_base64(10**10)


def test_mime_type() -> None:
    assert songbird.PCM16_24K.mime_type == "audio/pcm;rate=24000"
    assert songbird.MULAW_8K.mime_type == "audio/pcmu"
    assert songbird.ALAW_8K.mime_type == "audio/pcma"
    assert songbird.F32_48K.mime_type == "audio/x-f32le;rate=48000"
    assert songbird.GEMINI_LIVE_INPUT.mime_type == "audio/pcm;rate=16000"


def test_openai_format() -> None:
    assert songbird.GPT_LIVE.openai_format == {"type": "audio/pcm", "rate": 24_000}
    assert songbird.PCM16_16K.openai_format == {"type": "audio/pcm", "rate": 16_000}
    assert songbird.MULAW_8K.openai_format == {"type": "audio/pcmu"}
    assert songbird.ALAW_8K.openai_format == {"type": "audio/pcma"}
    for fmt in (songbird.F32_48K, songbird.PCM16_48K, songbird.PCM16_48K_STEREO):
        with pytest.raises(InvalidAudio):
            _ = fmt.openai_format


def test_eq_hash_immutable() -> None:
    a = AudioFormat(24_000, 1, "pcm16le")
    assert a == songbird.PCM16_24K
    assert hash(a) == hash(songbird.PCM16_24K)
    assert a != songbird.PCM16_16K
    assert {a: 1}[songbird.GPT_LIVE] == 1
    with pytest.raises(AttributeError):
        a.sample_rate = 8_000  # type: ignore
    assert "24000" in repr(a)


def test_as_format() -> None:
    assert as_format("pcm16_24k") == songbird.PCM16_24K
    assert as_format(songbird.MULAW_8K) is songbird.MULAW_8K
    with pytest.raises(TypeError):
        as_format(24_000)  # type: ignore
    with pytest.raises(InvalidAudio):
        as_format("pcm16_99k")
