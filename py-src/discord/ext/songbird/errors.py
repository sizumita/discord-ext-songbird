"""Exception hierarchy of ``discord.ext.songbird`` (§9.9, §11.1).

The classes are defined in Python so that they can inherit from both a library base class and a
builtin (``TimeoutError``, ``ValueError``). The native module imports them lazily and raises them
with the positional arguments documented on each class; the documented attributes are also set on
the instance afterwards, so constructing them directly works the same way.

The 0.5 names ``PySongbirdError``, ``PyJoinError`` and ``PyControlError`` remain available for one
release as deprecated aliases (accessing them emits a ``DeprecationWarning``).
"""

from __future__ import annotations

import warnings
from typing import Any

__all__ = [
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


class SongbirdError(Exception):
    """Base class of every exception raised by ``discord.ext.songbird``."""


class NotConnected(SongbirdError):
    """A connection-level method was used before ``connect()`` created the connection."""


class ConnectFailed(SongbirdError):
    """The voice connection could not be established, or ended terminally while connecting.

    Parameters
    ----------
    reason : str
        Close reason, e.g. ``"connect_failed"``, ``"kicked"``, ``"requested"``, ``"timeout"``.
    code : int or None
        Discord voice close code, if one caused the failure.
    """

    reason: str
    code: int | None

    def __init__(self, reason: str, code: int | None = None) -> None:
        super().__init__(reason if code is None else f"{reason} ({code})")
        self.reason = reason
        self.code = code

    def __reduce__(self) -> tuple[Any, ...]:
        return (type(self), (self.reason, self.code))


class ConnectTimeout(ConnectFailed, TimeoutError):
    """``connect()`` (or ``move_to()``/``reconnect()``) did not complete within ``timeout`` seconds.

    A subclass of ``TimeoutError`` (``asyncio.TimeoutError``), so discord.py's cleanup path runs.
    ``reason`` is ``"timeout"`` and ``code`` is ``None``.

    Parameters
    ----------
    timeout : float
        The timeout that expired, in seconds.
    """

    timeout: float

    def __init__(self, timeout: float) -> None:
        super().__init__("timeout", None)
        self.timeout = timeout

    def __reduce__(self) -> tuple[Any, ...]:
        return (type(self), (self.timeout,))


class ConnectionClosed(SongbirdError):
    """A connection-level method was used after the connection closed.

    Parameters
    ----------
    reason : str
        Why the connection closed (``"requested"``, ``"kicked"``, ``"lost"``, …).
    """

    reason: str

    def __init__(self, reason: str = "closed") -> None:
        super().__init__(f"connection closed: {reason}")
        self.reason = reason

    def __reduce__(self) -> tuple[Any, ...]:
        return (type(self), (self.reason,))


class StreamClosed(SongbirdError):
    """A read or write on a closed stream, playback or subscription.

    Async iterators end with ``StopAsyncIteration`` instead.

    Parameters
    ----------
    reason : str
        ``"closed"``, ``"connection_closed"``, ``"user_left"``, ``"overflow"``,
        ``"internal_error"``, …
    """

    reason: str

    def __init__(self, reason: str = "closed") -> None:
        super().__init__(f"stream closed: {reason}")
        self.reason = reason

    def __reduce__(self) -> tuple[Any, ...]:
        return (type(self), (self.reason,))


class StreamOverflow(StreamClosed):
    """Raised once, after the queued chunks, by a receive stream opened with ``overflow="error"``.

    ``async for`` raises it too (it does not end silently); afterwards the stream is closed.
    """

    def __init__(self, reason: str = "overflow") -> None:
        super().__init__(reason)


class PlaybackFull(SongbirdError):
    """A ``*_nowait`` write found no space; nothing was consumed.

    Parameters
    ----------
    needed_ms : int
        Milliseconds of space the write needs (including any gap it inserts).
    free_ms : int
        Milliseconds of space currently free.
    """

    needed_ms: int
    free_ms: int

    def __init__(self, needed_ms: int, free_ms: int) -> None:
        super().__init__(f"playback buffer full: {needed_ms} ms needed, {free_ms} ms free")
        self.needed_ms = needed_ms
        self.free_ms = free_ms

    def __reduce__(self) -> tuple[Any, ...]:
        return (type(self), (self.needed_ms, self.free_ms))


class ReceiveDisabled(SongbirdError):
    """``listen_*`` on a connection configured with ``VoiceConfig(receive=False)``."""


class InvalidAudio(SongbirdError, ValueError):
    """Invalid audio data or format: bad format combination, a buffer whose format code does not
    match the stream encoding, a wrong Opus frame size, a bad tag, …"""


class LimitExceeded(SongbirdError, ValueError):
    """A value is outside its documented range or a cap was reached.

    Parameters
    ----------
    what : str
        Name of the checked quantity.
    value : int or float
        The rejected value.
    minimum, maximum : int or float
        The allowed range (inclusive).
    """

    what: str
    value: int | float
    minimum: int | float
    maximum: int | float

    def __init__(self, what: str, value: int | float, minimum: int | float, maximum: int | float) -> None:
        super().__init__(f"{what}={value!r} is outside [{minimum!r}, {maximum!r}]")
        self.what = what
        self.value = value
        self.minimum = minimum
        self.maximum = maximum

    def __reduce__(self) -> tuple[Any, ...]:
        return (type(self), (self.what, self.value, self.minimum, self.maximum))


class SourceError(SongbirdError):
    """Encoded audio could not be recognised or decoded."""


class TrackError(SongbirdError):
    """A track operation failed (songbird ``ControlError``, seek/loops on a live source, seek/info on
    a queue entry that has not started)."""


_DEPRECATED: dict[str, tuple[type[SongbirdError], str]] = {
    "PySongbirdError": (SongbirdError, "SongbirdError"),
    "PyJoinError": (ConnectFailed, "ConnectFailed"),
    "PyControlError": (TrackError, "TrackError"),
}


def _deprecated_alias(module: str, name: str) -> type[SongbirdError]:
    try:
        cls, new = _DEPRECATED[name]
    except KeyError:
        raise AttributeError(f"module {module!r} has no attribute {name!r}") from None
    warnings.warn(
        f"{module}.{name} is deprecated and will be removed in 0.7; use {new}",
        DeprecationWarning,
        stacklevel=3,
    )
    return cls


def __getattr__(name: str) -> type[SongbirdError]:
    return _deprecated_alias(__name__, name)
