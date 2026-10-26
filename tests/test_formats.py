"""AudioFormat validation, parsing, presets and helpers (§9.2, §13.3 ``test_formats.py``)."""

from __future__ import annotations

import inspect
from typing import Any

import pytest
from discord.ext import songbird
from discord.ext.songbird import AudioFormat, InvalidAudio, LimitExceeded, SongbirdClient
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
    assert AudioFormat.parse(f"{fmt.encoding}/{fmt.sample_rate}/{fmt.channels}") == fmt


def test_only_neutral_presets_are_exported() -> None:
    exported = {name for name in songbird.__all__ if isinstance(getattr(songbird, name), AudioFormat)}
    assert exported == set(PRESETS)
    assert set(songbird.formats.__all__) >= set(PRESETS)


@pytest.mark.parametrize("method", ["listen", "listen_user", "listen_mixed", "listen_users", "open_playback"])
def test_default_format_is_discord_native(method: str) -> None:
    default = inspect.signature(getattr(SongbirdClient, method)).parameters["format"].default
    assert default == songbird.PCM16_48K_STEREO
    assert (default.sample_rate, default.channels, default.encoding) == (48_000, 2, "pcm16le")


@pytest.mark.parametrize(
    ("spec", "expected"),
    [
        ("pcm16_24k", (24_000, 1, "pcm16le")),
        ("pcm16_48k_stereo", (48_000, 2, "pcm16le")),
        ("pcm16_48k_mono", (48_000, 1, "pcm16le")),
        ("f32_48k", (48_000, 1, "f32le")),
        ("mulaw", (8_000, 1, "mulaw")),
        ("alaw", (8_000, 1, "alaw")),
        ("pcm16le/24000/1", (24_000, 1, "pcm16le")),
        ("pcm16le/48000/2", (48_000, 2, "pcm16le")),
        ("  PCM16_16K ", (16_000, 1, "pcm16le")),
    ],
)
def test_parse(spec: str, expected: tuple[int, int, str]) -> None:
    f = AudioFormat.parse(spec)
    assert (f.sample_rate, f.channels, f.encoding) == expected


@pytest.mark.parametrize(
    "spec",
    [
        "pcm16",
        "pcm16_12k",
        "pcm16_44.1k",
        "pcm16_24",
        "pcm16_24k_quad",
        "pcm16le/24000/3",
        "mulaw_16k",
        "nope",
        "",
        # Only this library's own spellings: no MIME types, no protocol-specific aliases.
        "audio/pcm;rate=24000",
        "audio/L16;rate=16000",
        "audio/pcmu",
        "g711_ulaw",
        "g711_alaw",
    ],
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


def test_rate_and_channels_are_required() -> None:
    with pytest.raises(TypeError):
        AudioFormat()  # type: ignore
    with pytest.raises(TypeError):
        AudioFormat(48_000)  # type: ignore
    assert AudioFormat(48_000, 2) == songbird.PCM16_48K_STEREO


def test_encoding_aliases() -> None:
    assert AudioFormat(8_000, 1, "ulaw") == songbird.MULAW_8K  # type: ignore
    assert AudioFormat(8_000, 1, "pcma") == songbird.ALAW_8K  # type: ignore
    with pytest.raises(InvalidAudio):
        AudioFormat(8_000, 1, "g711_ulaw")  # type: ignore
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
def test_silence_length(fmt: AudioFormat, ms: int) -> None:
    assert len(fmt.silence(ms)) == fmt.bytes_for(ms)


def test_silence_stereo_limit() -> None:
    with pytest.raises(LimitExceeded):
        songbird.F32_48K_STEREO.silence(10**10)


def test_no_mime_type() -> None:
    # Protocols spell formats differently; build the string from sample_rate/channels/encoding.
    assert not hasattr(songbird.PCM16_48K_STEREO, "mime_type")


def test_eq_hash_immutable() -> None:
    a = AudioFormat(24_000, 1, "pcm16le")
    assert a == songbird.PCM16_24K
    assert hash(a) == hash(songbird.PCM16_24K)
    assert a != songbird.PCM16_16K
    assert {a: 1}[songbird.PCM16_24K] == 1
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
