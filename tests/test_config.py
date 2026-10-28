"""VoiceConfig, runtime and module-level helpers (§9.3), plus the exception hierarchy (§9.9)."""

from __future__ import annotations

import asyncio
import dataclasses
import pickle
import warnings
from typing import Any

import pytest
from discord.ext import songbird
from discord.ext.songbird import (
    ConnectFailed,
    ConnectTimeout,
    InvalidAudio,
    LimitExceeded,
    PlaybackFull,
    SongbirdError,
    StreamClosed,
    StreamOverflow,
    VoiceConfig,
    errors,
)


def test_defaults() -> None:
    cfg = VoiceConfig()
    assert cfg.receive is True
    assert cfg.playout_buffer_ms == 60
    assert cfg.mix_mode == "stereo"
    assert cfg.driver_retries == 5
    assert cfg.to_native().receive is True


def test_low_latency_and_replace() -> None:
    cfg = VoiceConfig.low_latency()
    assert (cfg.playout_buffer_ms, cfg.mix_mode) == (40, "mono")
    cfg2 = VoiceConfig.low_latency(receive=False, mix_mode="stereo")
    assert cfg2.receive is False and cfg2.mix_mode == "stereo"
    assert cfg.replace(rejoin_attempts=0).rejoin_attempts == 0
    with pytest.raises(LimitExceeded):
        cfg.replace(rejoin_attempts=11)


def test_frozen() -> None:
    cfg = VoiceConfig()
    with pytest.raises(dataclasses.FrozenInstanceError):
        cfg.receive = False  # type: ignore


@pytest.mark.parametrize(
    ("field", "value", "exc"),
    [
        ("playout_buffer_ms", 10, LimitExceeded),
        ("playout_buffer_ms", 220, LimitExceeded),
        ("playout_buffer_ms", 30, InvalidAudio),
        ("playout_spike_ms", 50, InvalidAudio),
        ("decode_state_timeout_s", 0.5, LimitExceeded),
        ("decode_state_timeout_s", float("nan"), LimitExceeded),
        ("speaking_hangover_ms", 10, LimitExceeded),
        ("voice_threshold_dbfs", 3.0, LimitExceeded),
        ("max_decoded_users", 10**9, LimitExceeded),
        ("max_receive_mb", 0, LimitExceeded),
        ("stall_timeout_ms", 100, LimitExceeded),
        ("bitrate", 1_000, LimitExceeded),
        ("driver_timeout_s", 61.0, LimitExceeded),
        ("driver_retries", 0, LimitExceeded),
        ("driver_retries", 11, LimitExceeded),
        ("rejoin_attempts", -1, LimitExceeded),
        ("mix_mode", "quad", ValueError),
    ],
)
def test_validation(field: str, value: object, exc: type[Exception]) -> None:
    with pytest.raises(exc) as ei:
        kwargs: dict[str, Any] = {field: value}
        VoiceConfig(**kwargs)
    assert isinstance(ei.value, ValueError)


def test_driver_retries_zero_explains() -> None:
    with pytest.raises(LimitExceeded) as ei:
        VoiceConfig(driver_retries=0)
    assert ei.value.what == "driver_retries"
    assert ei.value.minimum == 1


def test_supported_codecs() -> None:
    codecs = songbird.supported_codecs()
    assert "opus" in codecs
    assert all(isinstance(c, str) for c in codecs)
    assert not hasattr(songbird, "SupportedCodec")


def test_configure_runtime_validates() -> None:
    with pytest.raises(LimitExceeded):
        songbird.configure_runtime(net_workers=0)
    with pytest.raises(LimitExceeded):
        songbird.configure_runtime(rx_workers=9)


def test_set_log_levels_validates() -> None:
    songbird.set_log_levels(library="INFO", songbird="WARNING")
    songbird.set_log_levels(library=10, songbird=30)
    with pytest.raises(TypeError):
        songbird.set_log_levels(library=None)  # type: ignore


def test_all_exports_exist() -> None:
    for name in songbird.__all__:
        assert hasattr(songbird, name), name
    assert songbird.__version__ == songbird.native.VERSION


def test_documented_names_are_in_all() -> None:
    """Every name the README, the user docs and the examples use from the package is in
    ``__all__`` (``from discord.ext.songbird import *``, doc tools), not only importable."""
    import pathlib
    import re

    root = pathlib.Path(__file__).resolve().parent.parent
    files = [root / "README.md", *(root / "docs").glob("*.md"), *(root / "examples").rglob("*.py")]
    used: set[str] = set()
    for path in files:
        if path.name == "design.md":  # the design document also names internals
            continue
        text = path.read_text(encoding="utf-8")
        used.update(re.findall(r"\bsongbird\.([A-Za-z_]\w*)", text))
        for m in re.finditer(r"from discord\.ext\.songbird import (?:\(([^)]*)\)|([^\n]+))", text):
            names = m.group(1) if m.group(1) is not None else m.group(2).split("#")[0]
            used.update(n for n in re.findall(r"[A-Za-z_]\w*", names) if n != "as")
    # Submodules (and logger-name fragments such as ``discord.ext.songbird.songbird``) are not names.
    modules = {"native", "songbird", "errors", "streams", "tracks", "playback", "client", "config", "formats"}
    assert "load_audio_file" in used
    missing = sorted(used - modules - set(songbird.__all__))
    assert not missing, missing
    namespace: dict[str, object] = {}
    exec("from discord.ext.songbird import *", namespace)
    assert callable(namespace["load_audio_file"])


# ── errors ────────────────────────────────────────────────────────────────────────────────────────


def test_hierarchy() -> None:
    assert issubclass(ConnectTimeout, ConnectFailed)
    assert issubclass(ConnectTimeout, TimeoutError)
    assert issubclass(ConnectTimeout, asyncio.TimeoutError)
    assert issubclass(StreamOverflow, StreamClosed)
    assert issubclass(InvalidAudio, ValueError) and issubclass(InvalidAudio, SongbirdError)
    assert issubclass(LimitExceeded, ValueError) and issubclass(LimitExceeded, SongbirdError)
    for name in errors.__all__:
        assert issubclass(getattr(errors, name), SongbirdError)


def test_constructor_contract() -> None:
    e = ConnectFailed("kicked", 4014)
    assert (e.reason, e.code, str(e)) == ("kicked", 4014, "kicked (4014)")
    assert str(ConnectFailed("timeout")) == "timeout"
    t = ConnectTimeout(5.0)
    assert (t.reason, t.code, t.timeout) == ("timeout", None, 5.0)
    assert isinstance(t, asyncio.TimeoutError)
    p = PlaybackFull(120, 40)
    assert (p.needed_ms, p.free_ms) == (120, 40)
    lim = LimitExceeded("ms", 10, 0, 5)
    assert (lim.what, lim.value, lim.minimum, lim.maximum) == ("ms", 10, 0, 5)
    assert StreamClosed("user_left").reason == "user_left"
    assert StreamOverflow().reason == "overflow"
    assert errors.ConnectionClosed("kicked").reason == "kicked"


@pytest.mark.parametrize(
    "exc",
    [
        ConnectFailed("lost", None),
        ConnectTimeout(1.5),
        PlaybackFull(1, 2),
        LimitExceeded("x", 1.5, 0.0, 1.0),
        StreamClosed("closed"),
        StreamOverflow(),
        errors.ConnectionClosed("requested"),
    ],
)
def test_pickle(exc: Exception) -> None:
    back = pickle.loads(pickle.dumps(exc))
    assert type(back) is type(exc)
    assert str(back) == str(exc)
    assert back.__dict__ == exc.__dict__


@pytest.mark.parametrize(
    ("old", "new"),
    [("PySongbirdError", SongbirdError), ("PyJoinError", ConnectFailed), ("PyControlError", errors.TrackError)],
)
def test_deprecated_aliases(old: str, new: type) -> None:
    with pytest.warns(DeprecationWarning, match=old):
        assert getattr(songbird, old) is new
    with pytest.warns(DeprecationWarning):
        assert getattr(errors, old) is new


def test_no_other_aliases() -> None:
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        with pytest.raises(AttributeError):
            _ = songbird.PyPlayerError
        with pytest.raises(AttributeError):
            _ = songbird.SongbirdImpl
