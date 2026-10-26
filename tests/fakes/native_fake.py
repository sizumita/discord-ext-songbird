"""Pure-Python fake of ``discord.ext.songbird.native`` (the §12.6 contract), including
``native._testing`` (§13.2).

It lets the Python layer (client, pump, streams, playback, tracks) be tested without the Rust build.
It models the *observable* contract of the native module — signatures, error types, wake-up and
arming protocol, state machine, timeline, queue bounds and overflow policies — not its DSP: Opus is
replaced by a toy codec (``_testing.encode_opus`` packets carry 8 kHz µ-law), resampling is naive
and the mixer limiter is a clamp.

Install it with ``install()`` *before* ``discord.ext.songbird`` is imported (``tests/conftest.py``
does this unless ``SONGBIRD_TEST_NATIVE=real``).
"""

from __future__ import annotations

import array
import dataclasses
import math
import os
import socket
import struct
import sys
import threading
import time
import types
import weakref
from collections import deque
from collections.abc import Callable, Iterable
from typing import Any

VERSION = "0.6.0"
WAKE_CONN = 0
WAKE_OPS = 1
WAKE_CTRL = 2
WAKE_TRACKS = 3

_TICK_S = 0.020
_MAX_WAKE_IDS = 256
_SILENT_FRAME = b"\xf8\xff\xfe"
_FAKE_OPUS_TAG = b"\xfcSBF"
_STATE = {"forked": False, "shutting_down": False, "runtime_started": False}
_LIVE: dict[str, int] = {
    "ConnShared": 0,
    "OwnedDriver": 0,
    "ConnWorker": 0,
    "Dispatch": 0,
    "SubQueue": 0,
    "EventQueue": 0,
    "PcmQueue": 0,
    "LiveReader": 0,
    "DoneGuard": 0,
    "SongbirdInput": 0,
    "SsrcState": 0,
    "TrackFlags": 0,
    "DecodeThread": 0,
}
_LIVE_LOCK = threading.Lock()
_max = max
_min = min


def _live(name: str, delta: int) -> None:
    with _LIVE_LOCK:
        _LIVE[name] = _LIVE.get(name, 0) + delta


# ── errors (imported lazily, like the native EXC_TYPES cache) ──────────────────────────────────────


def _errors() -> Any:
    from discord.ext.songbird import errors

    return errors


def _limit(what: str, value: Any, lo: Any, hi: Any) -> Exception:
    return _errors().LimitExceeded(what, value, lo, hi)


def _check(what: str, value: Any, lo: Any, hi: Any) -> Any:
    if isinstance(value, bool) or not isinstance(value, int | float):
        raise TypeError(f"{what} must be a number, got {type(value).__name__}")
    if not (lo <= value <= hi):  # NaN is never in range
        raise _limit(what, value, lo, hi)
    return value


def _check_int(what: str, value: Any, lo: int, hi: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise TypeError(f"{what} must be an int, got {type(value).__name__}")
    return _check(what, value, lo, hi)


def _invalid(msg: str) -> Exception:
    return _errors().InvalidAudio(msg)


def _ensure_not_forked() -> None:
    if _STATE["forked"]:
        raise _errors().SongbirdError("discord.ext.songbird is unusable after fork()")


# ── G.711 (Sun g711.c) ─────────────────────────────────────────────────────────────────────────────

_BIAS = 0x84
_CLIP = 32635


def _ulaw_encode(pcm: int) -> int:
    sign = 0x80 if pcm < 0 else 0
    if sign:
        pcm = -pcm
    pcm = min(pcm, _CLIP) + _BIAS
    exponent = max(0, min(7, (pcm >> 7).bit_length() - 1))
    mantissa = (pcm >> (exponent + 3)) & 0x0F
    return ~(sign | (exponent << 4) | mantissa) & 0xFF


def _ulaw_decode(u: int) -> int:
    u = ~u & 0xFF
    t = (((u & 0x0F) << 3) + _BIAS) << ((u & 0x70) >> 4)
    return (_BIAS - t) if u & 0x80 else (t - _BIAS)


_SEG_END = (0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF, 0x3FFF, 0x7FFF)


def _alaw_encode(pcm: int) -> int:
    pcm >>= 3
    if pcm >= 0:
        mask = 0xD5
    else:
        mask = 0x55
        pcm = -pcm - 1
    seg = next((i for i, end in enumerate(_SEG_END) if pcm <= end), 8)
    if seg >= 8:
        return 0x7F ^ mask
    aval = seg << 4
    aval |= (pcm >> 1) & 0x0F if seg < 2 else (pcm >> seg) & 0x0F
    return aval ^ mask


def _alaw_decode(a: int) -> int:
    a ^= 0x55
    t = (a & 0x0F) << 4
    seg = (a & 0x70) >> 4
    if seg == 0:
        t += 8
    elif seg == 1:
        t += 0x108
    else:
        t = (t + 0x108) << (seg - 1)
    return t if a & 0x80 else -t


_ULAW_DECODE = [_ulaw_decode(i) for i in range(256)]
_ALAW_DECODE = [_alaw_decode(i) for i in range(256)]


# ── formats ────────────────────────────────────────────────────────────────────────────────────────

_ENCODINGS = {
    "pcm16le": "pcm16le",
    "pcm16": "pcm16le",
    "pcm_s16le": "pcm16le",
    "s16le": "pcm16le",
    "int16": "pcm16le",
    "f32le": "f32le",
    "f32": "f32le",
    "pcm_f32le": "f32le",
    "float32": "f32le",
    "mulaw": "mulaw",
    "ulaw": "mulaw",
    "pcmu": "mulaw",
    "alaw": "alaw",
    "pcma": "alaw",
}
_WIDTH = {"pcm16le": 2, "f32le": 4, "mulaw": 1, "alaw": 1}
_RATES = (8_000, 16_000, 24_000, 48_000)


def _bad_spec(spec: str) -> Exception:
    return _invalid(
        f"unrecognised audio format {spec!r}; expected e.g. 'pcm16_48k_stereo', 'pcm16_16k', 'f32_48k', "
        "'mulaw', 'alaw' or 'pcm16le/48000/2'"
    )


class AudioFormat:
    """Immutable raw-audio format (fake)."""

    __slots__ = ("_rate", "_channels", "_encoding")

    def __init__(self, sample_rate: int, channels: int, encoding: str = "pcm16le") -> None:
        _ensure_not_forked()
        enc = _ENCODINGS.get(encoding) if isinstance(encoding, str) else None
        if enc is None:
            raise _invalid(f"unknown encoding {encoding!r}; expected pcm16le, f32le, mulaw or alaw")
        if sample_rate not in _RATES:
            raise _invalid(f"sample_rate must be 8000, 16000, 24000 or 48000 (got {sample_rate})")
        if channels not in (1, 2):
            raise _invalid(f"channels must be 1 or 2 (got {channels})")
        if enc in ("mulaw", "alaw") and (sample_rate != 8_000 or channels != 1):
            raise _invalid("mulaw/alaw (G.711) require 8000 Hz mono")
        object.__setattr__(self, "_rate", int(sample_rate))
        object.__setattr__(self, "_channels", int(channels))
        object.__setattr__(self, "_encoding", enc)

    def __setattr__(self, name: str, value: Any) -> None:
        raise AttributeError("AudioFormat is immutable")

    @classmethod
    def parse(cls, spec: str) -> AudioFormat:
        _ensure_not_forked()
        s = spec.strip().lower()
        if s.upper() in _PRESETS:
            return _PRESETS[s.upper()]
        enc = _ENCODINGS.get(s)
        if enc in ("mulaw", "alaw"):
            return cls(8_000, 1, enc)
        if "/" in s:
            parts = s.split("/")
            if len(parts) not in (2, 3) or _ENCODINGS.get(parts[0]) is None:
                raise _bad_spec(spec)
            try:
                rate = int(parts[1].strip())
                ch = int(parts[2].strip()) if len(parts) == 3 else 1
            except ValueError:
                raise _bad_spec(spec) from None
            return cls(rate, ch, _ENCODINGS[parts[0]])
        parts = s.split("_")
        if len(parts) not in (2, 3) or _ENCODINGS.get(parts[0]) is None or not parts[1].endswith("k"):
            raise _bad_spec(spec)
        try:
            khz = int(parts[1][:-1])
        except ValueError:
            raise _bad_spec(spec) from None
        ch = {"mono": 1, "stereo": 2}.get(parts[2]) if len(parts) == 3 else 1
        if ch is None:
            raise _bad_spec(spec)
        return cls(khz * 1_000, ch, _ENCODINGS[parts[0]])

    @property
    def sample_rate(self) -> int:
        return self._rate

    @property
    def channels(self) -> int:
        return self._channels

    @property
    def encoding(self) -> str:
        return self._encoding

    @property
    def sample_width(self) -> int:
        return _WIDTH[self._encoding]

    @property
    def frame_bytes(self) -> int:
        return self.sample_width * self._channels

    @property
    def bytes_per_ms(self) -> int:
        return self._rate // 1_000 * self.frame_bytes

    def bytes_for(self, ms: int) -> int:
        _check_int("ms", ms, 0, 2**32 - 1)
        return ms * self.bytes_per_ms

    def ms_for(self, nbytes: int) -> float:
        _check_int("nbytes", nbytes, 0, 2**63 - 1)
        return nbytes / self.bytes_per_ms

    def _silence_byte(self) -> int:
        return {"mulaw": 0xFF, "alaw": 0xD5}.get(self._encoding, 0)

    def silence(self, ms: int) -> bytes:
        _check_int("ms", ms, 0, 600_000)
        return bytes([self._silence_byte()]) * (ms * self.bytes_per_ms)

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, AudioFormat):
            return NotImplemented
        return (self._rate, self._channels, self._encoding) == (other._rate, other._channels, other._encoding)

    def __hash__(self) -> int:
        return hash((self._rate, self._channels, self._encoding))

    def __repr__(self) -> str:
        return f"AudioFormat(sample_rate={self._rate}, channels={self._channels}, encoding='{self._encoding}')"

    def __reduce__(self) -> Any:
        return (AudioFormat, (self._rate, self._channels, self._encoding))


_PRESETS = {
    "PCM16_8K": AudioFormat(8_000, 1, "pcm16le"),
    "PCM16_16K": AudioFormat(16_000, 1, "pcm16le"),
    "PCM16_24K": AudioFormat(24_000, 1, "pcm16le"),
    "PCM16_48K": AudioFormat(48_000, 1, "pcm16le"),
    "PCM16_48K_STEREO": AudioFormat(48_000, 2, "pcm16le"),
    "F32_48K": AudioFormat(48_000, 1, "f32le"),
    "F32_48K_STEREO": AudioFormat(48_000, 2, "f32le"),
    "MULAW_8K": AudioFormat(8_000, 1, "mulaw"),
    "ALAW_8K": AudioFormat(8_000, 1, "alaw"),
}


def _as_format(fmt: Any) -> AudioFormat:
    if isinstance(fmt, AudioFormat):
        return fmt
    if isinstance(fmt, str):
        return AudioFormat.parse(fmt)
    raise TypeError(f"expected an AudioFormat, got {type(fmt).__name__}")


def _encode_samples(fmt: AudioFormat, samples: Iterable[int]) -> bytes:
    """Encode int16 samples (interleaved) in ``fmt``'s encoding."""
    if fmt.encoding == "pcm16le":
        a = array.array("h", samples)
        if sys.byteorder == "big":
            a.byteswap()
        return a.tobytes()
    if fmt.encoding == "f32le":
        a = array.array("f", (s / 32768.0 for s in samples))
        if sys.byteorder == "big":
            a.byteswap()
        return a.tobytes()
    enc = _ulaw_encode if fmt.encoding == "mulaw" else _alaw_encode
    return bytes(enc(s) for s in samples)


def _decode_samples(fmt: AudioFormat, data: bytes) -> list[int]:
    """Whole frames of ``data`` → interleaved int16 samples."""
    if fmt.encoding == "pcm16le":
        a = array.array("h")
        a.frombytes(data)
        if sys.byteorder == "big":
            a.byteswap()
        return a.tolist()
    if fmt.encoding == "f32le":
        a = array.array("f")
        a.frombytes(data)
        if sys.byteorder == "big":
            a.byteswap()
        return [max(-32768, min(32767, round(x * 32768.0))) if math.isfinite(x) else 0 for x in a]
    table = _ULAW_DECODE if fmt.encoding == "mulaw" else _ALAW_DECODE
    return [table[b] for b in data]


# ── buffer extraction (convert.rs rules) ───────────────────────────────────────────────────────────

_RAW_CODES = {"B", "b", "c"}
_PCM16_CODES = {"h", "<h", "=h", "@h"}
_F32_CODES = {"f", "<f", "=f", "@f"}


def _buffer_bytes(
    data: Any, encoding: str | None, max_bytes: tuple[str, int] | Callable[[int], None] | None = None
) -> bytes:
    """Like the native ``extract_audio``/``extract_bytes``: the size is checked against ``max_bytes``
    (``(what, max)`` → ``LimitExceeded``, or a callable raising its own error) before copying."""

    def check(n: int) -> None:
        if callable(max_bytes):
            max_bytes(n)
        elif max_bytes is not None:
            _check_int(max_bytes[0], n, 0, max_bytes[1])

    if isinstance(data, bytes):
        check(len(data))
        return data
    try:
        mv = memoryview(data)
    except TypeError:
        raise TypeError(f"expected a bytes-like object, got {type(data).__name__}") from None
    with mv:
        if not mv.c_contiguous:
            raise _invalid("buffers must be C-contiguous")
        code = mv.format
        allowed = code.lstrip("<=@") in _RAW_CODES or (
            (encoding == "pcm16le" and code in _PCM16_CODES) or (encoding == "f32le" and code in _F32_CODES)
        )
        if not allowed:
            raise _invalid(f"buffer format {code!r} does not match the stream encoding {encoding}")
        check(mv.nbytes)
        return mv.tobytes()


def _check_chunk_format(data: Any, fmt: AudioFormat) -> None:
    """An ``AudioChunk`` must be written to a stream of its own format."""
    if isinstance(data, AudioChunk) and data.format != fmt:
        got = data.format
        raise _invalid(
            f"AudioChunk format {got.encoding}/{got.sample_rate}/{got.channels} does not match the "
            f"stream format {fmt.encoding}/{fmt.sample_rate}/{fmt.channels}"
        )


# ── Opus TOC (RFC 6716 §3.1) ──────────────────────────────────────────────────────────────────────


def _opus_samples(packet: bytes) -> int | None:
    if not packet:
        return None
    toc = packet[0]
    config = toc >> 3
    if config < 12:
        dur = (10, 20, 40, 60)[config % 4]
    elif config < 16:
        dur = (10, 20)[config % 2]
    else:
        dur = (2.5, 5, 10, 20)[config % 4]
    code = toc & 3
    if code == 0:
        frames = 1
    elif code in (1, 2):
        frames = 2
    else:
        if len(packet) < 2:
            return None
        frames = packet[1] & 0x3F
    return int(frames * dur * 48)


def _opus_size(n: int) -> None:
    if not 1 <= n <= 1275:
        raise _invalid(f"Opus packets must be 1..=1275 bytes (got {n})")


def _validate_opus(packet: bytes) -> None:
    _opus_size(len(packet))
    if _opus_samples(packet) != 960:
        raise _invalid("Opus packets must contain exactly 960 samples (20 ms at 48 kHz)")


# ── wake hub / doorbell ────────────────────────────────────────────────────────────────────────────


class _Doorbell:
    def __init__(self, sock: socket.socket | None) -> None:
        self._sock = sock
        self._closed = False

    def ring(self) -> None:
        if self._sock is None or self._closed:
            return
        try:
            self._sock.send(b"\x01")
        except OSError:  # EAGAIN (a byte is pending) or EPIPE (Python is gone)
            pass

    def shutdown(self) -> None:
        if self._sock is None or self._closed:
            return
        self._closed = True
        try:
            self._sock.shutdown(socket.SHUT_WR)
        except OSError:
            pass

    def close(self) -> None:
        self._closed = True
        if self._sock is not None:
            try:
                self._sock.close()
            except OSError:
                pass


class WakeBell:
    """Owner of the doorbell socket (fake)."""

    def __init__(self, fd: int) -> None:
        _ensure_not_forked()
        if not isinstance(fd, int) or fd < 0:
            raise OSError(9, "Bad file descriptor")
        try:
            sock = socket.socket(fileno=fd)
        except ValueError as e:
            raise OSError(9, str(e)) from None
        sock.setblocking(False)
        self._bell: _Doorbell | None = _Doorbell(sock)
        self._lock = threading.Lock()

    @property
    def taken(self) -> bool:
        return self._bell is None

    def _take(self) -> _Doorbell | None:
        with self._lock:
            bell, self._bell = self._bell, None
            return bell

    def __del__(self) -> None:
        bell = getattr(self, "_bell", None)
        if bell is not None:
            bell.close()


class _WakeHub:
    def __init__(self, bell: _Doorbell) -> None:
        self._bell = bell
        self._lock = threading.Lock()
        self._bits: set[int] = set()
        self._used: set[int] = {0, 1, 2, 3}
        self._pending = False
        self._closed = False

    def alloc(self) -> int:
        with self._lock:
            for i in range(4, _MAX_WAKE_IDS):
                if i not in self._used:
                    self._used.add(i)
                    return i
        raise _limit("wake_ids", _MAX_WAKE_IDS + 1, 0, _MAX_WAKE_IDS)

    def free(self, wake_id: int) -> None:
        with self._lock:
            self._used.discard(wake_id)

    def notify(self, wake_id: int) -> None:
        with self._lock:
            self._bits.add(wake_id)
            ring = not self._pending and not self._closed
            self._pending = True
        if ring:
            self._bell.ring()

    def take_ready(self) -> list[int]:
        with self._lock:
            self._pending = False
            ids = sorted(self._bits)
            self._bits.clear()
            return ids

    def close(self) -> None:
        with self._lock:
            if self._closed:
                return
            self._closed = True
        self._bell.shutdown()


# ── settings and small value types ─────────────────────────────────────────────────────────────────


class VoiceSettings:
    """Validated per-connection settings (fake of ``native.VoiceSettings``)."""

    def __init__(
        self,
        *,
        receive: bool = True,
        playout_buffer_ms: int = 60,
        playout_spike_ms: int = 60,
        decode_state_timeout_s: float = 60.0,
        speaking_hangover_ms: int = 200,
        voice_threshold_dbfs: float | None = None,
        max_decoded_users: int = 64,
        max_receive_mb: int = 64,
        stall_timeout_ms: int = 2_000,
        mix_mode: str = "stereo",
        bitrate: int | None = None,
        softclip: bool = True,
        driver_timeout_s: float = 10.0,
        driver_retries: int = 5,
        rejoin_attempts: int = 3,
        auto_recover: bool = True,
    ) -> None:
        _ensure_not_forked()
        _check_int("playout_buffer_ms", playout_buffer_ms, 20, 200)
        _check_int("playout_spike_ms", playout_spike_ms, 0, 200)
        for what, ms in (("playout_buffer_ms", playout_buffer_ms), ("playout_spike_ms", playout_spike_ms)):
            if ms % 20:
                raise _invalid(f"{what} must be a multiple of 20 ms (got {ms})")
        _check("decode_state_timeout_s", decode_state_timeout_s, 1.0, 600.0)
        _check_int("speaking_hangover_ms", speaking_hangover_ms, 20, 5_000)
        if voice_threshold_dbfs is not None:
            _check("voice_threshold_dbfs", voice_threshold_dbfs, -100.0, 0.0)
        _check_int("max_decoded_users", max_decoded_users, 1, 256)
        _check_int("max_receive_mb", max_receive_mb, 1, 1_024)
        _check_int("stall_timeout_ms", stall_timeout_ms, 500, 30_000)
        if mix_mode not in ("mono", "stereo"):
            raise ValueError(f"mix_mode must be 'mono' or 'stereo' (got {mix_mode!r})")
        if bitrate is not None:
            _check_int("bitrate", bitrate, 6_000, 510_000)
        _check("driver_timeout_s", driver_timeout_s, 1.0, 60.0)
        _check_int("driver_retries", driver_retries, 1, 10)
        _check_int("rejoin_attempts", rejoin_attempts, 0, 10)
        self._receive = bool(receive)
        self.hangover_ms = speaking_hangover_ms
        self.voice_threshold_dbfs = voice_threshold_dbfs
        self.max_receive_bytes = max_receive_mb << 20
        self.stall_timeout_ms = stall_timeout_ms
        self.rejoin_attempts = rejoin_attempts
        self.auto_recover = auto_recover

    @property
    def receive(self) -> bool:
        return self._receive

    def __repr__(self) -> str:
        return f"VoiceSettings(receive={self._receive})"


@dataclasses.dataclass(frozen=True)
class ConnectionStatus:
    state: str
    channel_id: int | None
    target_channel_id: int | None
    ssrc: int | None
    self_mute: bool
    self_deaf: bool
    rejoin_attempt: int
    generation: int
    close_reason: str | None
    close_code: int | None
    timeline_ms: int


class Control:
    """A request from Rust to the Python pump (fake complex enum)."""

    __slots__ = ()


class VoiceEvent:
    """A voice event (fake complex enum)."""

    __slots__ = ()


def _variant(base: type, name: str, *fields: str) -> type:
    cls = dataclasses.make_dataclass(name, [(f, Any) for f in fields], bases=(base,), frozen=True, slots=True)
    cls.__qualname__ = f"{base.__name__}.{name}"
    cls.__module__ = __name__
    setattr(base, name, cls)
    return cls


_variant(Control, "ChannelMoved", "channel_id")
_variant(Control, "ClassifyUsers")
_variant(Control, "Rejoin", "attempt", "channel_id")
_variant(Control, "Closed", "reason")

_EVENT_NAMES = (
    ("Connected", ("channel_id", "ssrc", "timeline_ms")),
    ("Reconnected", ("channel_id", "ssrc", "gap_ms")),
    ("Disconnected", ("reason", "code", "will_retry")),
    ("UserMapped", ("user_id", "ssrc")),
    ("UserLeft", ("user_id",)),
    ("SpeakingStarted", ("user_id", "timeline_ms", "captured_at")),
    ("SpeakingStopped", ("user_id", "timeline_ms", "duration_ms")),
    ("PlaybackStarted", ("playback_id",)),
    ("PlaybackIdle", ("playback_id", "played_ms")),
    ("PlaybackEnded", ("playback_id", "reason")),
    ("TrackEnded", ("track_id", "reason")),
    ("ReceiveStalled", ("since_ms",)),
    ("Lagged", ("dropped",)),
)
_EVENT_BITS: dict[type, int] = {}
for _i, (_name, _fields) in enumerate(_EVENT_NAMES):
    _EVENT_BITS[_variant(VoiceEvent, _name, *_fields)] = 1 << _i
_ALL_KINDS = (1 << len(_EVENT_NAMES)) - 1
_EV: Any = VoiceEvent
_CTL: Any = Control


@dataclasses.dataclass(frozen=True)
class StreamStats:
    queued_chunks: int
    queued_ms: int
    delivered_chunks: int
    dropped_chunks: int
    dropped_ms: int
    overflowed: bool


@dataclasses.dataclass(frozen=True)
class TrackInfo:
    playing: str
    volume: float
    position_ms: int
    play_time_ms: int
    loops: int | None


@dataclasses.dataclass(frozen=True)
class SegmentProgress:
    tag: str
    written_ms: int
    played_ms: int
    heard_ms: int
    complete: bool


class ClearResult:
    def __init__(
        self,
        discarded_ms: int,
        played_ms: int,
        heard_ms: int,
        position_ms: int,
        generation: int,
        segments: tuple[SegmentProgress, ...],
    ) -> None:
        self.discarded_ms = discarded_ms
        self.played_ms = played_ms
        self.heard_ms = heard_ms
        self.position_ms = position_ms
        self.generation = generation
        self.segments = segments

    def segment(self, tag: str) -> SegmentProgress | None:
        return next((i for i in self.segments if i.tag == tag), None)

    def __repr__(self) -> str:
        return f"ClearResult(discarded_ms={self.discarded_ms}, segments={self.segments!r})"


@dataclasses.dataclass(frozen=True)
class PlaybackStats:
    written_ms: int
    played_ms: int
    heard_ms: int
    buffered_ms: int
    underrun_ms: int
    gap_filled_ms: int
    rejected_ms: int
    discarded_ms: int
    contended_ticks: int
    clears: int
    tags_evicted: int


# ── clocks ─────────────────────────────────────────────────────────────────────────────────────────


class _Clock:
    """Seconds; real (``time.monotonic``) or manual (advanced by the harness)."""

    def __init__(self, manual: bool) -> None:
        self.manual = manual
        self._base = time.monotonic()
        self._offset = 0.0

    def now(self) -> float:
        if self.manual:
            return self._base + self._offset
        return time.monotonic()

    def advance(self, seconds: float) -> None:
        self._offset += seconds

    @property
    def elapsed(self) -> float:
        return self._offset if self.manual else time.monotonic() - self._base


# ── tracks ─────────────────────────────────────────────────────────────────────────────────────────

_PLAYING_CAP = 32
_QUEUE_CAP = 10_000
_OPS_CAP = 64


class _Track:
    def __init__(self, conn: _Conn, kind: str, *, source: Any = None, opts: tuple[float, Any, bool] = (1.0, 0, False)):
        self.conn = weakref.ref(conn)
        self.id = conn._next_track_id()
        self.kind = kind
        self.source = source
        self.volume, self.loops, self.paused = opts
        self.ended: str | None = None
        self.error: str | None = None
        self.reported = False
        self.pending = False
        self.uuid: str | None = None
        self.log: list[str] = []
        self.has_control = False
        self.lazy_stream: tuple[int, float] | None = None  # (max_buffer_bytes, volume) of enqueue_stream
        self.sink: Any = None
        self.taken = False
        _live("TrackFlags", 1)

    def __del__(self) -> None:
        _live("TrackFlags", -1)

    def attach_control(self, initially_paused: bool) -> None:
        self.has_control = True
        self.pending = False
        self.uuid = f"00000000-0000-4000-8000-{self.id:012x}"
        if initially_paused:
            self.paused = True

    def record(self, call: str) -> None:
        if len(self.log) < 4096:
            self.log.append(call)

    def resolve(self, reason: str, error: str | None = None) -> bool:
        if self.ended is not None:
            return False
        self.ended = reason
        self.error = error
        return True


class NativeTrack:
    """The native half of ``TrackHandle`` (fake). Equality is per track."""

    __slots__ = ("_t",)

    def __init__(self, t: _Track) -> None:
        self._t = t

    @property
    def id(self) -> int:
        return self._t.id

    @property
    def uuid(self) -> str | None:
        return self._t.uuid

    @property
    def kind(self) -> str:
        return self._t.kind

    @property
    def ended(self) -> bool:
        return self._t.ended is not None

    @property
    def end_reason(self) -> str | None:
        return self._t.ended

    @property
    def end_error(self) -> str | None:
        return self._t.error

    @property
    def pending(self) -> bool:
        return self._t.pending and self._t.ended is None

    def _conn(self) -> _Conn | None:
        return self._t.conn()

    def pause(self) -> None:
        t = self._t
        if t.ended is not None:
            return
        if t.pending:
            t.paused = True
            return
        t.paused = True
        t.record("pause")
        if (c := self._conn()) is not None:
            c._on_track_paused(t, True)

    def resume(self) -> None:
        t = self._t
        if t.ended is not None:
            return
        if t.pending:
            t.paused = False
            return
        t.paused = False
        t.record("play")
        if (c := self._conn()) is not None:
            c._on_track_paused(t, False)

    def stop(self) -> None:
        t = self._t
        if t.ended is not None:
            return
        c = self._conn()
        if t.pending:
            if c is not None:
                c._queue_remove(t)
            t.resolve("stopped")
            return
        t.record("stop")
        t.resolve("stopped")
        if c is not None:
            c._on_track_stopped(t)

    def set_volume(self, volume: float) -> None:
        _check("volume", volume, 0.0, 4.0)
        self._t.volume = float(volume)

    def set_loops(self, count: int | None) -> None:
        if self._t.kind in ("playback", "encoded_stream", "opus_stream"):
            raise _errors().TrackError("set_loops is not supported for live sources")
        if count is not None:
            _check_int("loops", count, 0, 2**32 - 2)
        self._t.loops = count

    def __eq__(self, other: object) -> bool:
        return isinstance(other, NativeTrack) and other._t is self._t

    def __hash__(self) -> int:
        return hash(id(self._t))

    def __repr__(self) -> str:
        return f"NativeTrack(id={self._t.id}, kind={self._t.kind!r}, ended={self._t.ended!r})"


# ── sources ────────────────────────────────────────────────────────────────────────────────────────


def _probe(data: bytes) -> tuple[str, int | None, int | None, int | None]:
    """(codec, sample_rate, channels, duration_ms) or SourceError."""
    if len(data) >= 12 and data[:4] == b"RIFF" and data[8:12] == b"WAVE":
        pos, rate, ch, width, codec = 12, None, None, None, None
        while pos + 8 <= len(data):
            cid, size = data[pos : pos + 4], struct.unpack_from("<I", data, pos + 4)[0]
            body = data[pos + 8 : pos + 8 + size]
            if cid == b"fmt " and len(body) >= 16:
                fmt_tag, ch, rate, _br, _align, bits = struct.unpack_from("<HHIIHH", body)
                width = bits // 8
                codec = {1: f"pcm_s{bits}le", 3: f"pcm_f{bits}le", 6: "pcm_alaw", 7: "pcm_mulaw"}.get(fmt_tag)
            elif cid == b"data" and rate and ch and width and codec:
                frames = min(size, len(data) - pos - 8) // (width * ch)
                return codec, rate, ch, frames * 1000 // rate
            pos += 8 + size + (size & 1)
        raise _errors().SourceError("malformed WAV file")
    if data[:4] == b"OggS":
        return "vorbis", None, None, None
    if data[:4] == b"fLaC":
        return "flac", None, None, None
    if data[:3] == b"ID3" or (len(data) > 1 and data[0] == 0xFF and data[1] & 0xE0 == 0xE0):
        return "mp3", None, None, None
    raise _errors().SourceError("unrecognised audio format (no format reader matched the content)")


def _arrow_bytes(array_obj: Any, *, allow_binary_scalar: bool) -> tuple[str, bytes]:
    try:
        import pyarrow as pa
    except ImportError:  # pragma: no cover - the dev group installs pyarrow
        raise _errors().SongbirdError("built without the 'arrow' feature") from None
    if not hasattr(array_obj, "__arrow_c_array__"):
        raise TypeError("expected an Arrow array (an object implementing __arrow_c_array__)")
    arr = array_obj if isinstance(array_obj, pa.Array) else pa.array(array_obj)
    if arr.null_count:
        raise _invalid("Arrow arrays must not contain nulls")
    t = arr.type
    if pa.types.is_binary(t) or pa.types.is_large_binary(t):
        if allow_binary_scalar:
            if len(arr) != 1:
                raise _invalid("a Binary array must hold exactly one value")
            return "binary", arr[0].as_py()
        return "binary", b""
    kinds = {pa.uint8(): "uint8", pa.int16(): "int16", pa.float32(): "float32"}
    kind = kinds.get(t)
    if kind is None:
        raise _invalid(f"unsupported Arrow type {t}")
    width = {"uint8": 1, "int16": 2, "float32": 4}[kind]
    buf = arr.buffers()[1]
    start = arr.offset * width
    return kind, buf.to_pybytes()[start : start + len(arr) * width]


class AudioFile:
    """Encoded audio held in memory (fake: probes WAV headers; Ogg/FLAC/MP3 by magic)."""

    def __init__(self, data: Any) -> None:
        _ensure_not_forked()
        raw = _buffer_bytes(data, None, ("source_bytes", 1 << 30))
        codec, rate, ch, dur = _probe(raw)
        self._data = raw
        self._info = (codec, rate, ch, dur)

    @classmethod
    def from_path(cls, path: Any, *, max_bytes: int = 256 << 20) -> AudioFile:
        _check_int("max_bytes", max_bytes, 1, 1 << 30)
        size = os.stat(path).st_size
        if size > max_bytes:
            raise _limit("file_bytes", size, 0, max_bytes)
        with open(path, "rb") as f:
            return cls(f.read())

    @classmethod
    def from_arrow(cls, array: Any) -> AudioFile:
        kind, data = _arrow_bytes(array, allow_binary_scalar=True)
        if kind not in ("uint8", "binary"):
            raise _invalid("AudioFile.from_arrow expects a UInt8 array or a one-value Binary array")
        return cls(data)

    @property
    def nbytes(self) -> int:
        return len(self._data)

    @property
    def codec(self) -> str:
        return self._info[0]

    @property
    def sample_rate(self) -> int | None:
        return self._info[1]

    @property
    def channels(self) -> int | None:
        return self._info[2]

    @property
    def duration_ms(self) -> int | None:
        return self._info[3]


class RawPcm:
    """Raw PCM/G.711 in memory (fake)."""

    def __init__(self, data: Any, format: Any) -> None:
        _ensure_not_forked()
        fmt = _as_format(format)
        if isinstance(data, str):
            raise TypeError("audio must be a bytes-like object, not str")
        _check_chunk_format(data, fmt)
        raw = _buffer_bytes(data, fmt.encoding, ("source_bytes", 1 << 30))
        frames = len(raw) // fmt.frame_bytes
        self._fmt = fmt
        self._frames = frames
        self._rate = fmt.sample_rate

    @classmethod
    def from_arrow(cls, array: Any, format: Any) -> RawPcm:
        fmt = _as_format(format)
        kind, data = _arrow_bytes(array, allow_binary_scalar=False)
        expected = {"pcm16le": "int16", "f32le": "float32"}.get(fmt.encoding)
        if kind not in ("uint8", expected):
            raise _invalid(f"an Arrow {kind} array does not match encoding {fmt.encoding}")
        return cls(data, fmt)

    @classmethod
    def from_samples(cls, data: Any, sample_rate: int, channels: int = 2, encoding: str = "f32le") -> RawPcm:
        _check_int("sample_rate", sample_rate, 8_000, 192_000)
        _check_int("channels", channels, 1, 2)
        if encoding not in ("pcm16le", "f32le"):
            raise _invalid("from_samples encodings are pcm16le and f32le")
        if not isinstance(data, bytes | bytearray | memoryview) and hasattr(data, "__arrow_c_array__"):
            kind, raw = _arrow_bytes(data, allow_binary_scalar=False)
            if kind != {"pcm16le": "int16", "f32le": "float32"}[encoding]:
                raise _invalid(f"an Arrow {kind} array does not match encoding {encoding}")
        else:
            if isinstance(data, str):
                raise TypeError("audio must be a bytes-like object, not str")
            raw = _buffer_bytes(data, encoding, ("source_bytes", 1 << 30))
        width = _WIDTH[encoding]
        frames = len(raw) // (width * channels)
        stored = sample_rate if sample_rate in _RATES else 48_000
        self = cls.__new__(cls)
        self._fmt = AudioFormat(stored, channels, encoding)
        self._frames = frames if stored == sample_rate else round(frames * stored / sample_rate)
        self._rate = stored
        return self

    @property
    def format(self) -> AudioFormat:
        return self._fmt

    @property
    def duration_ms(self) -> int:
        return self._frames * 1000 // self._rate


class OpusPackets:
    """Pre-encoded 20 ms Opus packets (fake)."""

    def __init__(self, packets: Iterable[Any]) -> None:
        _ensure_not_forked()
        out: list[bytes] = []
        for p in packets:
            b = _buffer_bytes(p, None, _opus_size)
            _validate_opus(b)
            out.append(b)
            if len(out) > 1_000_000:
                raise _limit("opus_packets", len(out), 0, 1_000_000)
        self._packets = out

    @classmethod
    def from_arrow(cls, array: Any) -> OpusPackets:
        import pyarrow as pa

        arr = array if isinstance(array, pa.Array) else pa.array(array)
        if not (pa.types.is_binary(arr.type) or pa.types.is_large_binary(arr.type)):
            raise _invalid("OpusPackets.from_arrow expects a Binary/LargeBinary array")
        if arr.null_count:
            raise _invalid("Arrow arrays must not contain nulls")
        return cls(v.as_py() for v in arr)

    def __len__(self) -> int:
        return len(self._packets)

    @property
    def duration_ms(self) -> int:
        return 20 * len(self._packets)


# ── receive ────────────────────────────────────────────────────────────────────────────────────────

_OVERFLOWS = ("drop_oldest", "drop_newest", "error")


class AudioChunk:
    """One received chunk (fake): read-only buffer with format 'h' | 'f' | 'B'."""

    __slots__ = (
        "_data",
        "_fmt",
        "_meta",
        "_seq",
        "_start_ms",
        "_dropped_ms",
        "__weakref__",
    )

    def __init__(self, data: bytes, fmt: AudioFormat, meta: dict[str, Any], seq: int, start_ms: int, dropped_ms: int):
        self._data = data
        self._fmt = fmt
        self._meta = meta
        self._seq = seq
        self._start_ms = start_ms
        self._dropped_ms = dropped_ms

    def __buffer__(self, flags: int, /) -> memoryview:
        code: Any = {"pcm16le": "h", "f32le": "f"}.get(self._fmt.encoding, "B")
        return memoryview(self._data).cast(code)

    def __release_buffer__(self, view: memoryview, /) -> None:
        view.release()

    @property
    def format(self) -> AudioFormat:
        return self._fmt

    @property
    def user_id(self) -> int | None:
        return self._meta["user_id"]

    @property
    def seq(self) -> int:
        return self._seq

    @property
    def start_ms(self) -> int:
        return self._start_ms

    @property
    def end_ms(self) -> int:
        return self._start_ms + self.duration_ms

    @property
    def duration_ms(self) -> int:
        return self._meta["frames"] * 1000 // self._fmt.sample_rate

    @property
    def frames(self) -> int:
        return self._meta["frames"]

    @property
    def start_sample(self) -> int:
        return self._start_ms * self._fmt.sample_rate // 1000

    @property
    def timeline_ms(self) -> int:
        return self._meta["timeline_ms"]

    @property
    def captured_at(self) -> float:
        return self._meta["captured_at"]

    @property
    def speaking(self) -> bool:
        return self._meta["voiced_ms"] > 0

    @property
    def speakers(self) -> frozenset[int]:
        return self._meta["speakers"]

    @property
    def voiced_ms(self) -> int:
        return self._meta["voiced_ms"]

    @property
    def concealed_ms(self) -> int:
        return self._meta["concealed_ms"]

    @property
    def gap_ms(self) -> int:
        return self._meta["gap_ms"]

    @property
    def dropped_ms(self) -> int:
        return self._dropped_ms

    @property
    def rms_dbfs(self) -> float:
        return self._meta["rms_dbfs"]

    @property
    def peak(self) -> float:
        return self._meta["peak"]

    def __len__(self) -> int:
        return len(self._data)

    def to_bytes(self) -> bytes:
        return bytes(self._data)

    def __arrow_c_array__(self, requested_schema: object | None = None) -> tuple[object, object]:
        import pyarrow as pa

        t = {"pcm16le": pa.int16(), "f32le": pa.float32()}.get(self._fmt.encoding, pa.uint8())
        width = {"pcm16le": 2, "f32le": 4}.get(self._fmt.encoding, 1)
        arr = pa.Array.from_buffers(t, len(self._data) // width, [None, pa.py_buffer(self._data)])
        return arr.__arrow_c_array__(requested_schema)

    def __repr__(self) -> str:
        return f"AudioChunk(user_id={self.user_id}, seq={self._seq}, start_ms={self._start_ms}, bytes={len(self)})"


class ChunkBatch:
    def __init__(self, rows: list[AudioChunk]) -> None:
        self._rows = rows

    @property
    def num_rows(self) -> int:
        return len(self._rows)

    def __len__(self) -> int:
        return len(self._rows)

    def _batch(self) -> Any:
        import pyarrow as pa

        r = self._rows
        return pa.record_batch(
            {
                "user_id": pa.array([c.user_id for c in r], pa.uint64()),
                "seq": pa.array([c.seq for c in r], pa.uint64()),
                "start_ms": pa.array([c.start_ms for c in r], pa.int64()),
                "timeline_ms": pa.array([c.timeline_ms for c in r], pa.int64()),
                "duration_ms": pa.array([c.duration_ms for c in r], pa.uint32()),
                "speaking": pa.array([c.speaking for c in r], pa.bool_()),
                "dropped_ms": pa.array([c.dropped_ms for c in r], pa.uint32()),
                "pcm": pa.array([c.to_bytes() for c in r], pa.binary()),
            }
        )

    def __arrow_c_schema__(self) -> object:
        return self._batch().schema.__arrow_c_schema__()

    def __arrow_c_array__(self, requested_schema: object | None = None) -> tuple[object, object]:
        return self._batch().__arrow_c_array__(requested_schema)

    def to_pyarrow(self) -> Any:
        return self._batch()


class _SubQueue:
    def __init__(
        self, conn: _Conn, source: tuple[str, Any], fmt: AudioFormat, chunk_ms: int, max_queue_ms: int, overflow: str
    ):
        self.conn = conn
        self.source = source  # ("user", uid) | ("mixed", filter dict)
        self.fmt = fmt
        self.chunk_ms = chunk_ms
        self.max_chunks = -(-max_queue_ms // chunk_ms)
        self.overflow = overflow
        self.chunk_frames = fmt.sample_rate * chunk_ms // 1000
        self.chunk_bytes = self.chunk_frames * fmt.frame_bytes
        self.budget = self.max_chunks * (self.chunk_bytes + 160)
        self.wake_id = conn.wake.alloc()
        self.lock = threading.Lock()
        self.items: deque[tuple[bytes, dict[str, Any], int, int, int]] = deque()
        self.armed = False
        self.closed: str | None = None
        self.overflow_reported = False
        self.origin: int | None = None
        self.pending_drop_ms = 0
        self.delivered = 0
        self.dropped_chunks = 0
        self.dropped_ms_total = 0
        # lane state
        self.lane_pos: int | None = None  # next chunk start sample (timeline-aligned)
        self.acc: list[int] = []
        self.acc_meta: dict[str, Any] | None = None
        self.pending_filter: dict[str, Any] | None = None
        _live("SubQueue", 1)

    def __del__(self) -> None:
        _live("SubQueue", -1)

    def push(self, data: bytes, meta: dict[str, Any]) -> None:
        notify = False
        with self.lock:
            if self.closed is not None:
                return
            if self.origin is None:
                self.origin = meta["timeline_ms"]
            start_ms = meta["timeline_ms"] - self.origin
            seq = start_ms // self.chunk_ms
            dur = meta["frames"] * 1000 // self.fmt.sample_rate
            if len(self.items) >= self.max_chunks:
                if self.overflow == "drop_oldest":
                    old = self.items.popleft()
                    lost = old[4] + dur
                    self.dropped_chunks += 1
                    self.dropped_ms_total += dur
                    if self.items:
                        d, m, s, st, dm = self.items[0]
                        self.items[0] = (d, m, s, st, dm + lost)
                    else:
                        self.pending_drop_ms += lost
                elif self.overflow == "drop_newest":
                    self.pending_drop_ms += dur
                    self.dropped_chunks += 1
                    self.dropped_ms_total += dur
                    return
                else:
                    self.closed = "overflow"
                    notify = self.armed
                    self.armed = False
                    self.dropped_chunks += 1
                    self.dropped_ms_total += dur
            if self.closed is None:
                self.items.append((data, meta, seq, start_ms, self.pending_drop_ms))
                self.pending_drop_ms = 0
                notify = self.armed
                self.armed = False
        if notify:
            self.conn.wake.notify(self.wake_id)

    def pop(self, arm: bool) -> AudioChunk | None:
        with self.lock:
            if self.items:
                data, meta, seq, start_ms, dropped = self.items.popleft()
                self.delivered += 1
                return AudioChunk(data, self.fmt, meta, seq, start_ms, dropped)
            if self.closed is not None:
                if self.closed == "overflow" and not self.overflow_reported:
                    self.overflow_reported = True
                    raise _errors().StreamOverflow("overflow")
                raise _errors().StreamClosed(self.closed)
            if arm:
                self.armed = True
            return None

    def close(self, reason: str) -> None:
        notify = False
        with self.lock:
            if self.closed is None:
                self.closed = reason
                notify = True
        if notify:
            self.conn.wake.notify(self.wake_id)

    def stats(self) -> StreamStats:
        with self.lock:
            n = len(self.items)
            return StreamStats(
                queued_chunks=n,
                queued_ms=n * self.chunk_ms,
                delivered_chunks=self.delivered,
                dropped_chunks=self.dropped_chunks,
                dropped_ms=self.dropped_ms_total,
                overflowed=self.closed == "overflow",
            )


class ReceiveSubscription:
    """The native half of ``AudioStream`` (fake)."""

    def __init__(self, q: _SubQueue) -> None:
        self._q = q

    @property
    def wake_id(self) -> int:
        return self._q.wake_id

    @property
    def origin_timeline_ms(self) -> int | None:
        return self._q.origin

    @property
    def closed(self) -> bool:
        return self._q.closed is not None

    @property
    def close_reason(self) -> str | None:
        return self._q.closed

    @property
    def format(self) -> AudioFormat:
        return self._q.fmt

    @property
    def user_id(self) -> int | None:
        return self._q.source[1] if self._q.source[0] == "user" else None

    @property
    def chunk_ms(self) -> int:
        return self._q.chunk_ms

    def try_recv(self) -> AudioChunk | None:
        return self._q.pop(arm=True)

    def read_nowait(self) -> AudioChunk | None:
        return self._q.pop(arm=False)

    def skip_to_live(self, reset_clock: bool = True) -> int:
        with self._q.lock:
            ms = len(self._q.items) * self._q.chunk_ms
            self._q.items.clear()
            if reset_clock:
                self._q.origin = None
                self._q.pending_drop_ms = 0
            return ms

    def set_filter(
        self,
        include: Iterable[int] | None,
        exclude: Iterable[int],
        ignore_bots: bool = True,
        policy: str = "mix",
        floor_release_ms: int = 600,
    ) -> None:
        if self._q.source[0] != "mixed":
            raise ValueError("set_filter applies to mixed streams only")
        _check_int("floor_release_ms", floor_release_ms, 0, 2**32 - 1)
        if policy not in ("mix", "floor"):
            raise ValueError(f"policy must be 'mix' or 'floor' (got {policy!r})")
        self._q.pending_filter = _mix_filter(include, exclude, ignore_bots, policy, floor_release_ms)

    def stats(self) -> StreamStats:
        return self._q.stats()

    def close(self) -> None:
        self._q.close("closed")

    def read_batch(self, max_chunks: int = 50) -> ChunkBatch | None:
        _check_int("max_chunks", max_chunks, 1, 10_000)
        rows: list[AudioChunk] = []
        while len(rows) < max_chunks:
            c = self._q.pop(arm=False)
            if c is None:
                break
            rows.append(c)
        return ChunkBatch(rows) if rows else None

    def __del__(self) -> None:
        q = getattr(self, "_q", None)
        if q is not None:
            q.close("closed")


def _mix_filter(include: Any, exclude: Any, ignore_bots: bool, policy: str, floor_release_ms: int) -> dict[str, Any]:
    return {
        "include": None if include is None else frozenset(include),
        "exclude": frozenset(exclude),
        "ignore_bots": bool(ignore_bots),
        "policy": policy,
        "floor_release_ms": floor_release_ms,
        "holder": None,
        "holder_quiet_ms": 0,
    }


class _RouterQueue:
    def __init__(self, conn: _Conn, spec: dict[str, Any]) -> None:
        self.conn = conn
        self.spec = spec
        self.wake_id = conn.wake.alloc()
        self.items: deque[ReceiveSubscription] = deque()
        self.streams: dict[int, _SubQueue] = {}
        self.armed = False
        self.closed = False
        self.dropped = 0
        self.lock = threading.Lock()

    def push(self, sub: ReceiveSubscription) -> None:
        notify = False
        dropped = None
        with self.lock:
            if self.closed:
                return
            if len(self.items) >= 64:
                dropped = self.items.popleft()
                self.dropped += 1
            self.items.append(sub)
            notify, self.armed = self.armed, False
        if dropped is not None:
            dropped._q.close("closed")
        if notify:
            self.conn.wake.notify(self.wake_id)

    def close(self, close_streams: bool) -> None:
        with self.lock:
            if self.closed:
                return
            self.closed = True
            pending = list(self.items)
            self.items.clear()
            streams = list(self.streams.values())
        if close_streams:
            for q in streams:
                q.close("closed")
        for s in pending:
            s._q.close("closed")
        self.conn.wake.notify(self.wake_id)


class RouterSubscription:
    """The native half of ``UserStreams`` (fake)."""

    def __init__(self, r: _RouterQueue) -> None:
        self._r = r

    @property
    def wake_id(self) -> int:
        return self._r.wake_id

    @property
    def dropped(self) -> int:
        return self._r.dropped

    def try_next(self) -> ReceiveSubscription | None:
        with self._r.lock:
            if self._r.items:
                return self._r.items.popleft()
            if self._r.closed:
                raise _errors().StreamClosed("closed")
            self._r.armed = True
            return None

    def close(self, close_streams: bool = True) -> None:
        self._r.close(close_streams)

    def __del__(self) -> None:
        r = getattr(self, "_r", None)
        if r is not None:
            r.close(True)


class _EventQueue:
    def __init__(self, conn: _Conn, cap: int, mask: int) -> None:
        self.conn = conn
        self.cap = cap
        self.mask = mask
        self.wake_id = conn.wake.alloc()
        self.items: deque[Any] = deque()
        self.lag = 0
        self.dropped = 0
        self.armed = False
        self.closed = False
        self.lock = threading.Lock()
        _live("EventQueue", 1)

    def __del__(self) -> None:
        _live("EventQueue", -1)

    def push(self, ev: Any, force: bool = False) -> None:
        if not force and not (_EVENT_BITS[type(ev)] & self.mask):
            return
        with self.lock:
            if self.closed:
                return
            if len(self.items) >= self.cap and not force:
                self.items.popleft()
                self.lag += 1
                self.dropped += 1
            self.items.append(ev)
            notify, self.armed = self.armed, False
        if notify:
            self.conn.wake.notify(self.wake_id)

    def pop(self, arm: bool) -> Any:
        with self.lock:
            if self.lag and self.items:
                n, self.lag = self.lag, 0
                return _EV.Lagged(n)
            if self.items:
                return self.items.popleft()
            if self.closed:
                raise _errors().StreamClosed("connection_closed" if self.conn.closing else "closed")
            if arm:
                self.armed = True
            return None

    def close(self) -> None:
        with self.lock:
            if self.closed:
                return
            self.closed = True
        self.conn.wake.notify(self.wake_id)


class EventSubscription:
    """A bounded (drop-oldest) event queue (fake)."""

    def __init__(self, q: _EventQueue) -> None:
        self._q = q

    @property
    def wake_id(self) -> int:
        return self._q.wake_id

    @property
    def dropped(self) -> int:
        return self._q.dropped

    def try_recv(self) -> Any:
        return self._q.pop(arm=True)

    def read_nowait(self) -> Any:
        return self._q.pop(arm=False)

    def close(self) -> None:
        self._q.close()

    def __del__(self) -> None:
        q = getattr(self, "_q", None)
        if q is not None:
            q.close()


# ── send: PcmQueue ─────────────────────────────────────────────────────────────────────────────────


class _Tag:
    __slots__ = ("written", "played", "last_pull_t", "last_write", "open", "ended")

    def __init__(self, seq: int) -> None:
        self.written = 0
        self.played = 0
        self.last_pull_t: float | None = None
        self.last_write = seq
        self.open = True
        self.ended = False


class _PcmQueue:
    _next_id = 1

    def __init__(self, conn: _Conn, fmt: AudioFormat, spec: dict[str, Any], track: _Track) -> None:
        self.conn = conn
        self.id = _PcmQueue._next_id
        _PcmQueue._next_id += 1
        self.fmt = fmt
        self.rate = fmt.sample_rate
        self.ch = fmt.channels
        self.spec = spec
        self.track = track
        self.cap = spec["max_buffer_ms"] * self.rate // 1000
        self.prebuffer = spec["prebuffer_ms"] * self.rate // 1000
        self.prebuffer_ticks = -(-spec["prebuffer_ms"] // 20)
        self.wake_id = conn.wake.alloc()
        self.lock = threading.Lock()
        self.wlock = threading.Lock()
        self.segs: deque[list[Any]] = deque()  # [data(list|None), off, frames, tag]
        self.queued = 0
        self.queued_real = 0
        self.carry = b""
        self.carry_gen = 0
        self.generation = 0
        self.anchor: tuple[int, float] | None = None
        self.timed_end_ms: int | None = None
        self.gap_reject: list[int] | None = None
        self.tags: dict[str, _Tag] = {}
        self.write_seq = 0
        self.rejected: deque[str] = deque(maxlen=256)
        self.hold_until: float | None = None
        self.gate = False
        self.gate_ticks = 0
        self.closed: str | None = None  # None | "drain" | "abort"
        self.end_reason: str | None = None
        self.space_wanted = 0
        self.drain_waiting = False
        self.drain_pulls = 0
        self.idle_waiting = False
        self.end_waiting = False
        self.active = False
        self.started = False
        self.paused_user = False
        self.paused_idle = True
        self.link_up = conn.link_up
        self.volume = float(spec["volume"])
        self.pulls = 0
        self.position = 0
        self.played_real = 0
        self.written = 0
        self.underrun = 0
        self.gap_filled = 0
        self.rejected_frames = 0
        self.discarded = 0
        self.contended = 0
        self.clears = 0
        self.tags_evicted = 0
        self.last_real_pull_t: float | None = None
        self.last_write_t: float | None = None
        self.real_since_hk = False
        self.reader_gone = False
        self.ended_emitted = False
        _live("PcmQueue", 1)

    def __del__(self) -> None:
        _live("PcmQueue", -1)

    def ms(self, frames: int) -> int:
        return frames * 1000 // self.rate

    # ── write ────────────────────────────────────────────────────────────────────────────────────

    def write(self, data: Any, tag: str | None, at_ms: int | None) -> None:
        if isinstance(data, str):
            raise TypeError("audio must be a bytes-like object, not str")
        _check_chunk_format(data, self.fmt)
        cap_bytes = self.cap * self.fmt.frame_bytes
        payload = _buffer_bytes(data, self.fmt.encoding, ("write_bytes", cap_bytes))
        if self.closed is not None:
            raise _errors().StreamClosed(self.end_reason or "closed")
        if self.conn.closing:
            raise _errors().StreamClosed("connection_closed")
        if tag is not None and len(tag.encode()) > 128:
            raise _invalid("tags are at most 128 bytes")
        if at_ms is not None:
            _check_int("at_ms", at_ms, -(2**63), 2**63 - 1)
        fb = self.fmt.frame_bytes
        with self.wlock:
            if self.carry_gen != self.generation:
                self.carry = b""
            buf = self.carry + payload
            whole = len(buf) // fb * fb
            frames_bytes, tail = buf[:whole], buf[whole:]
            samples = _decode_samples(self.fmt, frames_bytes)
            frames = len(samples) // self.ch
            with self.lock:
                now = self.conn.clock.now()
                if self.closed is not None:
                    raise _errors().StreamClosed(self.end_reason or "closed")
                if self._rejects(tag, at_ms, frames, now):
                    self.rejected_frames += frames
                    self.carry = b""
                    return
                gap = self._gap_frames(at_ms, frames, now) if at_ms is not None else 0
                if self.queued + gap + frames > self.cap:
                    self.space_wanted = min(gap + frames, self.cap)
                    free = self.cap - self.queued
                    raise _errors().PlaybackFull(self.ms(gap + frames), self.ms(free))
                segment_start = self.queued == 0 and not self.active
                if gap > 0:
                    self.segs.append([None, 0, gap, None])
                    self.queued += gap
                    self.gap_filled += gap
                if frames:
                    self.segs.append([samples, 0, frames, tag])
                    self.queued += frames
                    self.queued_real += frames
                    self.written += frames
                    self.last_write_t = now
                    if tag is not None:
                        self._tag_written(tag, frames)
                    if at_ms is not None:
                        end = at_ms + frames * 1000 // self.rate
                        self.timed_end_ms = end if self.timed_end_ms is None else max(self.timed_end_ms, end)
                self.carry = tail
                self.carry_gen = self.generation
                if frames:
                    self.drain_pulls = 0
                    if segment_start:
                        self.active = True
                        if self.prebuffer > 0:
                            self.gate = True
                            self.gate_ticks = self.prebuffer_ticks
                    if self.paused_idle:
                        self.paused_idle = False
                        self.track.record("play")

    def _rejects(self, tag: str | None, at_ms: int | None, frames: int, now: float) -> bool:
        if self.hold_until is not None and now < self.hold_until:
            return True
        if tag is not None and tag in self.rejected:
            return True
        if self.gap_reject is not None and at_ms is not None:
            cut_end, gap_ms = self.gap_reject
            if at_ms < cut_end + gap_ms:
                self.gap_reject[0] = max(cut_end, at_ms + frames * 1000 // self.rate)
                return True
            self.gap_reject = None
        if self.spec["when_disconnected"] == "discard" and not self.link_up:
            return True
        return False

    def _gap_frames(self, at_ms: int, frames: int, now: float) -> int:
        earliest = now + self.queued / self.rate
        if self.anchor is None:
            self.anchor = (at_ms, earliest)
            return 0
        remote, wall = self.anchor
        delta = at_ms - remote
        if abs(delta) > 86_400_000:
            self.anchor = (at_ms, earliest)
            return 0
        gap_ms = round((wall + delta / 1000 - earliest) * 1000)
        if gap_ms > self.spec["max_gap_ms"]:
            self.anchor = (at_ms, earliest)
            return 0
        gap = gap_ms * self.rate // 1000 if gap_ms > 40 else 0
        if gap + frames > self.cap:
            self.anchor = (at_ms, earliest)
            return 0
        if gap_ms < -40:
            self.anchor = (at_ms, earliest)
        return gap

    def _tag_written(self, tag: str, frames: int) -> None:
        self.write_seq += 1
        t = self.tags.get(tag)
        if t is None:
            if len(self.tags) >= 256:
                closed = [k for k, v in self.tags.items() if not v.open]
                victim = closed[0] if closed else min(self.tags, key=lambda k: self.tags[k].last_write)
                del self.tags[victim]
                self.tags_evicted += 1
            t = self.tags[tag] = _Tag(self.write_seq)
        t.open = True
        t.written += frames
        t.last_write = self.write_seq

    # ── mixer pull (MixerHarness) ────────────────────────────────────────────────────────────────

    def should_end(self) -> bool:
        if self.closed == "abort" or self.conn.closing:
            return True
        return self.closed == "drain" and self.queued == 0

    def pull(self) -> list[float]:
        n = self.rate // 50
        out = [0] * (n * self.ch)
        consumed = real = 0
        if self.conn.send_muted:
            pass
        elif not self.lock.acquire(blocking=False):
            self.contended += 1
        else:
            try:
                now = self.conn.clock.now()
                if self.paused_user:
                    pass
                elif self.gate and self.queued < self.prebuffer and self.gate_ticks > 0:
                    self.gate_ticks -= 1
                else:
                    self.gate = False
                    consumed, real = self._pop_into(out, n, now)
                self.queued -= consumed
                self.queued_real -= real
            finally:
                self.lock.release()
        self.pulls += 1
        self.position += n
        self.played_real += real
        if self.active:
            self.underrun += n - consumed
        if consumed:
            self.drain_pulls += 1
        if real:
            self.last_real_pull_t = self.conn.clock.now()
            self.real_since_hk = True
        factor = 48_000 // self.rate
        vol = self.volume / 32768.0
        ch = self.ch
        frames = [out[i : i + ch] for i in range(0, len(out), ch)]
        return [s * vol for fr in frames for _ in range(factor) for s in fr]

    def _pop_into(self, out: list[int], n: int, now: float) -> tuple[int, int]:
        pos = consumed = real = 0
        while pos < n and self.segs:
            seg = self.segs[0]
            data, off, frames, tag = seg
            take = min(n - pos, frames - off)
            if data is not None:
                base = off * self.ch
                out[pos * self.ch : (pos + take) * self.ch] = data[base : base + take * self.ch]
                real += take
                if tag is not None and (t := self.tags.get(tag)) is not None:
                    t.played += take
                    t.last_pull_t = now
            seg[1] += take
            pos += take
            consumed += take
            if seg[1] >= frames:
                self.segs.popleft()
        return consumed, real

    # ── housekeeping (ConnWorker tick) ───────────────────────────────────────────────────────────

    def housekeep(self, now: float) -> None:
        notify = False
        events: list[Any] = []
        with self.lock:
            if self.space_wanted and self.cap - self.queued >= self.space_wanted:
                self.space_wanted = 0
                notify = True
            if self.drain_waiting and self.queued == 0 and (self.drain_pulls or not self.written):
                self.drain_waiting = False
                notify = True
            if self.real_since_hk and not self.started:
                self.started = True
                events.append(_EV.PlaybackStarted(self.id))
            self.real_since_hk = False
            last = max(t for t in (self.last_real_pull_t, self.last_write_t, 0.0) if t is not None)
            if self.active and self.queued == 0 and now - last >= self.spec["idle_ms"] / 1000:
                self.active = False
                self.started = False
                for t in self.tags.values():
                    t.open = False
                events.append(_EV.PlaybackIdle(self.id, self.ms(self.played_real)))
                if self.idle_waiting:
                    self.idle_waiting = False
                    notify = True
                if self.spec["auto_pause"] and not self.paused_idle:
                    self.paused_idle = True
                    self.track.record("pause")
            if self.reader_gone and not self.ended_emitted:
                self.ended_emitted = True
                reason = self.end_reason or ("drained" if self.closed == "drain" else "closed")
                events.append(_EV.PlaybackEnded(self.id, reason))
                notify = True
        for ev in events:
            self.conn.emit(ev)
        if notify:
            self.conn.wake.notify(self.wake_id)

    def close(self, mode: str, reason: str = "closed") -> None:
        with self.lock:
            if self.closed == "abort" or (self.closed == "drain" and mode == "drain"):
                return
            self.closed = mode
            self.end_reason = self.end_reason or reason
            self.gate = False
            if mode == "abort":
                self.segs.clear()
                self.queued = self.queued_real = 0
        if mode == "abort" and self.track.has_control and self.track.ended is None:
            self.track.record("stop")
            self.track.resolve("stopped")
        self.conn.wake.notify(self.wake_id)

    def heard(self, played: int, last_pull_t: float | None, now: float) -> int:
        lat = self.spec["output_latency_ms"]
        since = 1e9 if last_pull_t is None else (now - last_pull_t) * 1000
        return max(0, played - max(0, min(played, round(lat - since))))


class Playback:
    """The native half of ``PlaybackStream`` (fake)."""

    def __init__(self, q: _PcmQueue) -> None:
        self._q = q

    def write(self, data: Any, tag: str | None = None, at_ms: int | None = None) -> None:
        self._q.write(data, tag, at_ms)

    def arm_space(self, nbytes: int) -> bool:
        q = self._q
        frames = max(0, nbytes) // q.fmt.frame_bytes
        with q.lock:
            wanted = min(max(frames, q.space_wanted), q.cap)
            if q.cap - q.queued >= wanted:
                return True
            q.space_wanted = wanted
            return False

    def arm_drain(self) -> bool:
        q = self._q
        with q.lock:
            q.gate = False
            if q.queued == 0 and (q.drain_pulls or not q.written):
                return True
            q.drain_waiting = True
            return False

    def arm_idle(self) -> bool:
        q = self._q
        with q.lock:
            if not q.active:
                return True
            q.idle_waiting = True
            return False

    def arm_end(self) -> bool:
        q = self._q
        with q.lock:
            if q.reader_gone:
                return True
            q.end_waiting = True
            return False

    def clear(
        self, *, reject_late: bool = True, hold_ms: int = 0, reject_until_gap_ms: int | None = None
    ) -> ClearResult:
        q = self._q
        _check_int("hold_ms", hold_ms, 0, 10_000)
        if reject_until_gap_ms is not None:
            _check_int("reject_until_gap_ms", reject_until_gap_ms, 0, 10_000)
        with q.lock:
            now = q.conn.clock.now()
            lat = q.spec["output_latency_ms"] / 1000
            segments = []
            for name, t in q.tags.items():
                recent = t.last_pull_t is not None and now - t.last_pull_t <= lat
                if t.open or recent:
                    played = min(t.played, t.written)
                    segments.append(
                        SegmentProgress(
                            name,
                            q.ms(t.written),
                            q.ms(played),
                            q.ms(q.heard(played, t.last_pull_t, now)),
                            t.ended and t.played >= t.written,
                        )
                    )
            q.discarded += q.queued_real
            q.segs.clear()
            q.queued = q.queued_real = 0
            wake = q.space_wanted > 0
            q.space_wanted = 0
            q.anchor = None
            q.gate = False
            q.generation += 1
            q.clears += 1
            if reject_late:
                for name, t in q.tags.items():
                    if t.open or any(i.tag == name for i in segments):
                        if name not in q.rejected:
                            q.rejected.append(name)
            for t in q.tags.values():
                t.open = False
            if reject_until_gap_ms is not None and q.timed_end_ms is not None:
                q.gap_reject = [q.timed_end_ms, reject_until_gap_ms]
            if hold_ms:
                q.hold_until = now + hold_ms / 1000
            played = q.ms(q.played_real)
            res = ClearResult(
                q.ms(q.discarded),
                played,
                q.ms(q.heard(q.played_real, q.last_real_pull_t, now)),
                q.ms(q.position),
                q.generation,
                tuple(segments),
            )
        if wake:
            q.conn.wake.notify(q.wake_id)
        return res

    def end_segment(self, tag: str) -> None:
        q = self._q
        with q.lock:
            q.gate = False
            t = q.tags.get(tag)
            if t is not None:
                t.open = False
                t.ended = True

    def segment(self, tag: str) -> SegmentProgress | None:
        q = self._q
        with q.lock:
            t = q.tags.get(tag)
            if t is None:
                return None
            played = min(t.played, t.written)
            now = q.conn.clock.now()
            return SegmentProgress(
                tag,
                q.ms(t.written),
                q.ms(played),
                q.ms(q.heard(played, t.last_pull_t, now)),
                t.ended and t.played >= t.written,
            )

    def pause(self) -> None:
        with self._q.lock:
            self._q.paused_user = True
        self._q.track.record("pause")

    def resume(self) -> None:
        with self._q.lock:
            self._q.paused_user = False
        self._q.track.record("play")

    @property
    def volume(self) -> float:
        return self._q.volume

    @volume.setter
    def volume(self, value: float) -> None:
        _check("volume", value, 0.0, 4.0)
        self._q.volume = float(value)

    @property
    def played_ms(self) -> int:
        return self._q.ms(self._q.played_real)

    @property
    def heard_ms(self) -> int:
        q = self._q
        return q.ms(q.heard(q.played_real, q.last_real_pull_t, q.conn.clock.now()))

    @property
    def position_ms(self) -> int:
        return self._q.ms(self._q.position)

    @property
    def buffered_ms(self) -> int:
        return self._q.ms(self._q.queued)

    @property
    def written_ms(self) -> int:
        return self._q.ms(self._q.written)

    @property
    def is_idle(self) -> bool:
        return not self._q.active

    @property
    def closed(self) -> bool:
        return self._q.closed is not None or self._q.conn.closing

    @property
    def wake_id(self) -> int:
        return self._q.wake_id

    @property
    def playback_id(self) -> int:
        return self._q.id

    @property
    def track(self) -> NativeTrack:
        return NativeTrack(self._q.track)

    def stats(self) -> PlaybackStats:
        q = self._q
        now = q.conn.clock.now()
        return PlaybackStats(
            q.ms(q.written),
            q.ms(q.played_real),
            q.ms(q.heard(q.played_real, q.last_real_pull_t, now)),
            q.ms(q.queued),
            q.ms(q.underrun),
            q.ms(q.gap_filled),
            q.ms(q.rejected_frames),
            q.ms(q.discarded),
            q.contended,
            q.clears,
            q.tags_evicted,
        )

    def close(self, drain: bool = False) -> None:
        self._q.close("drain" if drain else "abort")

    def __del__(self) -> None:
        q = getattr(self, "_q", None)
        if q is not None:
            q.close("abort")


class EncodedSink:
    """The native half of ``EncodedStream`` (fake: bytes are consumed one chunk per tick)."""

    def __init__(self, conn: _Conn, max_buffer_bytes: int, track: _Track) -> None:
        self._conn = conn
        self._max = max_buffer_bytes
        self._track = track
        self._buf = bytearray()
        self._finished = False
        self._closed = False
        self._wanted = 0
        self._started = False
        self._lock = threading.Lock()
        self.wake_id = conn.wake.alloc()

    @property
    def track(self) -> NativeTrack:
        return NativeTrack(self._track)

    def write(self, data: Any) -> None:
        raw = _buffer_bytes(data, None, ("write_bytes", self._max))
        with self._lock:
            if self._closed or self._finished or self._conn.closing:
                raise _errors().StreamClosed("connection_closed" if self._conn.closing else "closed")
            if len(self._buf) + len(raw) > self._max:
                self._wanted = len(raw)
                raise _errors().PlaybackFull(0, 0)
            self._buf += raw

    def arm_space(self, nbytes: int) -> bool:
        with self._lock:
            if self._max - len(self._buf) >= nbytes:
                return True
            self._wanted = nbytes
            return False

    def finish(self) -> None:
        self._finished = True

    def close(self, drain: bool = False) -> None:
        if drain:
            self._finished = True
            return
        with self._lock:
            self._closed = True
            self._buf.clear()
        if self._track.ended is None:
            self._track.record("stop")
            self._track.resolve("stopped")

    def _tick(self) -> None:
        with self._lock:
            if self._closed:
                return
            if self._buf and not self._started:
                self._started = True
                self._track.record("play")
            del self._buf[:16_384]
            notify = self._wanted and self._max - len(self._buf) >= self._wanted
            if notify:
                self._wanted = 0
            done = self._finished and not self._buf and self._track.ended is None
        if notify:
            self._conn.wake.notify(self.wake_id)
        if done:
            self._track.resolve("finished")


class OpusSink:
    """The native half of ``OpusPacketStream`` (fake)."""

    def __init__(self, conn: _Conn, max_packets: int, track: _Track) -> None:
        self._conn = conn
        self._max = max_packets
        self._track = track
        self._q: deque[bytes] = deque()
        self._closed = False
        self._paused_idle = True
        self._lock = threading.Lock()
        self.wake_id = conn.wake.alloc()

    @property
    def buffered_ms(self) -> int:
        return 20 * len(self._q)

    @property
    def track(self) -> NativeTrack:
        return NativeTrack(self._track)

    def send(self, packet: Any) -> None:
        raw = _buffer_bytes(packet, None, _opus_size)
        _validate_opus(raw)
        with self._lock:
            if self._closed or self._conn.closing:
                raise _errors().StreamClosed("connection_closed" if self._conn.closing else "closed")
            if len(self._q) >= self._max:
                raise _errors().PlaybackFull(20, 0)
            self._q.append(raw)
            if self._paused_idle:
                self._paused_idle = False
                self._track.record("play")

    def arm_space(self) -> bool:
        return len(self._q) < self._max

    def clear(self) -> int:
        with self._lock:
            ms = 20 * len(self._q)
            self._q.clear()
            return ms

    def close(self, drain: bool = False) -> None:
        with self._lock:
            self._closed = True
            if not drain:
                self._q.clear()
        if not drain and self._track.ended is None:
            self._track.record("stop")
            self._track.resolve("stopped")


# ── the connection ─────────────────────────────────────────────────────────────────────────────────

_OPEN_PHASES = ("joining", "connecting", "connected", "awaiting_move", "awaiting_rejoin", "rejoining")
_STATE_NAMES = {
    "idle": "idle",
    "joining": "joining",
    "connecting": "connecting",
    "connected": "connected",
    "awaiting_move": "reconnecting",
    "awaiting_rejoin": "reconnecting",
    "rejoining": "reconnecting",
    "closed": "closed",
}
_CLOSE_REASONS = (
    "requested",
    "left_channel",
    "kicked",
    "connect_failed",
    "session_invalid",
    "rejoin_failed",
    "lost",
    "dropped",
    "loop_closed",
    "process_exit",
    "connect_aborted",
    "cleanup",
    "overflow",
    "user_left",
    "internal_error",
    "closed",
    "connection_closed",
)
_REGISTRY: weakref.WeakSet[_Conn] = weakref.WeakSet()


class _Conn:
    """Shared state of one connection (the fake ``ConnShared``)."""

    def __init__(
        self, bell: _Doorbell, guild_id: int, user_id: int, settings: VoiceSettings, reconnect: bool, detached: bool
    ):
        self.guild_id = guild_id
        self.user_id = user_id
        self.settings = settings
        self.reconnect = reconnect
        self.detached = detached
        self.clock = _Clock(manual=detached)
        self.mono0 = time.monotonic()
        self.t0 = self.clock.now()
        self.wake = _WakeHub(bell)
        self.lock = threading.RLock()
        self.closing = False
        self.send_muted = False
        self.link_up = False
        # gateway
        self.phase = "idle"
        self.target: int | None = None
        self.channel: int | None = None
        self.session: str | None = None
        self.server: tuple[str, str] | None = None
        self.prev_server: tuple[str, str] | None = None
        self.self_mute = False
        self.self_deaf = False
        self.generation = 0
        self.attempt = 0
        self.leave_requested = False
        self.ssrc: int | None = None
        self.deadline: float | None = None
        self.close_reason: str | None = None
        self.close_code: int | None = None
        # control slots
        self.ctrl_moved: int | None = None
        self.ctrl_classify = False
        self.ctrl_rejoin: tuple[int, int] | None = None
        self.ctrl_closed: str | None = None
        # receive
        self.rx_in: deque[tuple[Any, ...]] = deque()
        self.tick_index = 0
        self.ssrc_users: dict[int, int] = {}
        self.bots: set[int] = set()
        self.known_users: set[int] = set()
        self.unclassified: list[int] = []
        self.speaking: dict[int, list[int]] = {}  # user → [start_tick, last_voiced_tick]
        self.subs: list[_SubQueue] = []
        self.routers: list[_RouterQueue] = []
        self.event_queues: list[_EventQueue] = []
        self.counters: dict[str, int] = dict.fromkeys(
            (
                "ticks_real",
                "ticks_synthetic",
                "ticks_dropped",
                "ticks_skipped",
                "bad_packets",
                "decode_errors",
                "plc_frames",
                "dave_trailers_stripped",
                "dispatch_reinstalls",
                "heartbeat_misses",
                "panics",
                "log_dropped",
                "connects",
                "rejoins",
            ),
            0,
        )
        # send
        self.playbacks: list[_PcmQueue] = []
        self.sinks: list[Any] = []
        self.tracks: list[_Track] = []
        self.queue: deque[_Track] = deque()
        self.current: _Track | None = None
        self.queue_paused = False
        self.track_seq = 0
        self.ops: dict[int, Any] = {}
        self.op_results: list[tuple[int, Any]] = []
        self.next_op = 1
        # Live-source inputs not yet taken by a MixerHarness (playback id → queue).
        self.parked: dict[int, _PcmQueue] = {}
        _live("ConnShared", 1)
        _REGISTRY.add(self)

    def __del__(self) -> None:
        _live("ConnShared", -1)

    def _next_track_id(self) -> int:
        self.track_seq += 1
        return self.track_seq

    # ── time ─────────────────────────────────────────────────────────────────────────────────────

    def captured_at(self, t: float) -> float:
        return self.mono0 + (t - self.t0)

    def expected_tick(self, now: float) -> int:
        return int((now - self.t0) / _TICK_S + 1e-9)

    # ── events / control ─────────────────────────────────────────────────────────────────────────

    def emit(self, ev: Any) -> None:
        if self.closing:
            return
        for q in list(self.event_queues):
            q.push(ev)
        self.event_queues = [q for q in self.event_queues if not q.closed]

    def set_ctrl(self, kind: str, value: Any = None) -> None:
        if kind == "moved":
            self.ctrl_moved = value
        elif kind == "classify":
            self.ctrl_classify = True
        elif kind == "rejoin":
            self.ctrl_rejoin = value
        elif kind == "closed":
            if self.ctrl_closed is None:
                self.ctrl_closed = value
        self.wake.notify(WAKE_CTRL)

    def take_controls(self) -> list[Any]:
        out: list[Any] = []
        with self.lock:
            if self.ctrl_moved is not None:
                out.append(_CTL.ChannelMoved(self.ctrl_moved))
                self.ctrl_moved = None
            if self.ctrl_classify:
                out.append(_CTL.ClassifyUsers())
                self.ctrl_classify = False
            if self.ctrl_rejoin is not None:
                if self.ctrl_closed is None:
                    out.append(_CTL.Rejoin(*self.ctrl_rejoin))
                self.ctrl_rejoin = None
            if self.ctrl_closed is not None:
                out.append(_CTL.Closed(self.ctrl_closed))
        return out

    # ── gateway (§4.5, simplified) ───────────────────────────────────────────────────────────────

    def _set_phase(self, phase: str) -> None:
        if phase != self.phase:
            self.phase = phase
            self.wake.notify(WAKE_CONN)

    def _connecting(self) -> None:
        self.generation += 1
        self.counters["connects"] += 1
        self.deadline = None
        self._set_phase("connecting")
        self.wake.notify(WAKE_CONN)

    def _link(self, up: bool) -> None:
        self.link_up = up
        for q in self.playbacks:
            q.link_up = up
            if not up and q.spec["when_disconnected"] == "discard":
                with q.lock:
                    q.discarded += q.queued_real
                    q.segs.clear()
                    q.queued = q.queued_real = 0
                    q.anchor = None

    def _backoff(self, attempt: int) -> float:
        return float(min(16, 1 << max(0, attempt - 1)))

    def _closed_by_gateway(self, reason: str, code: int | None = None) -> None:
        self.close(reason, code)

    def join(self, channel: int, mute: bool, deaf: bool) -> None:
        with self.lock:
            if self.closing or self.phase != "idle":
                return
            self.target = channel
            self.self_mute, self.self_deaf = mute, deaf
            self.send_muted = mute
            self._set_phase("joining")

    def move(self, channel: int) -> None:
        with self.lock:
            if self.closing:
                return
            if self.phase in ("connected", "joining", "connecting"):
                self.target = channel
                self.server = None
                self.deadline = self.clock.now() + 3.0
                if self.phase == "connected":
                    self._link(False)
                self._set_phase("joining")
                self.wake.notify(WAKE_CONN)
            elif self.phase in ("awaiting_move", "awaiting_rejoin", "rejoining"):
                self.target = channel

    def begin_rejoin(self) -> None:
        with self.lock:
            if self.closing or self.phase not in _OPEN_PHASES:
                return
            self.attempt = 0
            self.server = None
            self.deadline = self.clock.now() + 15.0
            self._set_phase("rejoining")

    def leave(self) -> None:
        with self.lock:
            self.leave_requested = True

    def set_flags(self, mute: bool, deaf: bool) -> None:
        with self.lock:
            if self.closing:
                return
            self.self_mute, self.self_deaf = mute, deaf
            self.send_muted = mute
            self.wake.notify(WAKE_CONN)

    def voice_state(self, session: str, ch: int | None) -> None:
        with self.lock:
            if self.closing or self.phase not in _OPEN_PHASES:
                return
            p = self.phase
            if ch is None:
                if p == "rejoining":
                    return
                if p == "awaiting_move":
                    self.close("kicked")
                    return
                self.close("requested" if self.leave_requested else "left_channel")
                return
            if p == "rejoining":
                self.session = session
                self.target = ch
                if self.server is not None:
                    self._connecting()
                return
            if p == "awaiting_move":
                if ch == self.channel and session != self.session and self.prev_server is not None:
                    self.session = session
                    self.server = self.prev_server
                    self._connecting()
                    return
                self.session = session
                self.target = ch
                self.server = None
                if ch != self.channel:
                    self.set_ctrl("moved", ch)
                self._set_phase("joining")
                return
            if p == "joining":
                self.session = session
                if ch != self.target:
                    self.target = ch
                    self.server = None
                    self.set_ctrl("moved", ch)
                if self.server is not None:
                    self._connecting()
                return
            if p in ("connecting", "connected"):
                ref = self.target if p == "connecting" else self.channel
                if ch != ref:
                    self.target = ch
                    self.session = session
                    self.server = None
                    self.deadline = self.clock.now() + 3.0
                    self.set_ctrl("moved", ch)
                    self._link(False)
                    self._set_phase("joining")
                elif session != self.session:
                    self.session = session
                    self._connecting()

    def voice_server(self, token: str, endpoint: str | None) -> None:
        with self.lock:
            if self.closing or self.phase not in _OPEN_PHASES:
                return
            if endpoint is None:
                self.server = None
                if self.phase in ("connecting", "connected"):
                    self._link(False)
                    self._set_phase("joining")
                return
            ep = endpoint.strip()
            ep = ep[len("wss://") :] if ep.startswith("wss://") else ep
            srv = (token, ep.rstrip("/"))
            p = self.phase
            if p in ("joining", "rejoining"):
                self.server = srv
                self.prev_server = srv
                if self.session is not None:
                    self._connecting()
            elif p in ("connecting", "connected") and srv != self.server:
                self.server = srv
                self.prev_server = srv
                self._connecting()

    def driver_connect(self, ssrc: int) -> None:
        with self.lock:
            if self.closing:
                return
            self.ssrc = ssrc
            if self.phase == "connected":
                self._link(True)

    def connect_result(self, ok: bool, timed_out: bool = False) -> None:
        with self.lock:
            if self.closing or self.phase != "connecting":
                return
            if ok:
                self.channel = self.target
                self.attempt = 0
                self.deadline = None
                self._set_phase("connected")
                self._link(True)
                self.emit(_EV.Connected(self.channel or 0, self.ssrc or 0, self.tick_index * 20))
                self.wake.notify(WAKE_CONN)
            elif self.reconnect and self.attempt < self.settings.rejoin_attempts:
                self.attempt += 1
                self.deadline = self.clock.now() + self._backoff(self.attempt)
                self._set_phase("awaiting_rejoin")
            else:
                self.close("connect_failed")

    def driver_reconnect(self, ssrc: int) -> None:
        with self.lock:
            if self.closing:
                return
            self.ssrc = ssrc
            if self.phase == "connected":
                self.emit(_EV.Reconnected(self.channel or 0, ssrc, 0))
                self._link(True)

    def driver_disconnect(self, kind: str, reason: str | None, code: int | None) -> None:
        with self.lock:
            if self.closing or reason == "requested":
                return
            p = self.phase
            if p == "connected":
                self._link(False)
                if kind == "runtime" and reason == "ws_closed" and code == 4014:
                    self.deadline = self.clock.now() + 5.0
                    self._set_phase("awaiting_move")
                    self.emit(_EV.Disconnected(reason or "lost", code, True))
                elif reason == "ws_closed" and code in (4016, 4017):
                    self.close("session_invalid", code)
                elif self.reconnect and self.attempt < self.settings.rejoin_attempts:
                    self.attempt += 1
                    self.deadline = self.clock.now() + self._backoff(self.attempt)
                    self._set_phase("awaiting_rejoin")
                    self.emit(_EV.Disconnected(reason or "lost", code, True))
                else:
                    self.close("lost")
            elif p in ("joining", "connecting", "awaiting_rejoin", "rejoining") and kind != "connect":
                self._link(False)
                self.emit(_EV.Disconnected(reason or "lost", code, True))

    def deadlines(self, now: float) -> None:
        with self.lock:
            if self.closing or self.deadline is None or now < self.deadline:
                return
            p = self.phase
            self.deadline = None
            if p == "joining" and self.prev_server is not None and self.session is not None:
                self.server = self.prev_server
                self._connecting()
            elif p == "awaiting_move":
                if self.reconnect:
                    self.attempt = 1
                    self.deadline = now
                    self._set_phase("awaiting_rejoin")
                else:
                    self.close("lost")
            elif p == "awaiting_rejoin":
                self.server = None
                self.deadline = now + 15.0
                self.counters["rejoins"] += 1
                self._set_phase("rejoining")
                self.set_ctrl("rejoin", (self.attempt, self.target or self.channel or 0))
            elif p == "rejoining":
                if self.attempt < self.settings.rejoin_attempts:
                    self.attempt += 1
                    self.deadline = now + self._backoff(self.attempt)
                    self._set_phase("awaiting_rejoin")
                else:
                    self.close("rejoin_failed")

    def status(self) -> ConnectionStatus:
        with self.lock:
            return ConnectionStatus(
                state=_STATE_NAMES[self.phase],
                channel_id=self.channel,
                target_channel_id=self.target,
                ssrc=self.ssrc,
                self_mute=self.self_mute,
                self_deaf=self.self_deaf,
                rejoin_attempt=self.attempt,
                generation=self.generation,
                close_reason=self.close_reason,
                close_code=self.close_code,
                timeline_ms=self.tick_index * 20,
            )

    # ── teardown (§4.7) ──────────────────────────────────────────────────────────────────────────

    def close(self, reason: str, code: int | None = None) -> None:
        with self.lock:
            if self.closing:  # T0
                return
            self.closing = True
            self.phase = "closed"  # T1
            self.close_reason = reason
            self.close_code = code
        for q in self.playbacks:  # T5
            q.close("abort", "connection_closed")
            q.reader_gone = True
        for s in self.sinks:
            s.close()
        for q in self.subs:  # T6
            q.close("connection_closed")
        for r in self.routers:
            r.close(True)
        last = _EV.Disconnected(reason, code, False)
        for eq in self.event_queues:
            eq.push(last, force=True)
            eq.close()
        for t in self.tracks:  # T8
            t.resolve("disconnected")
        for t in self.queue:
            t.resolve("disconnected")
        if self.current is not None:
            self.current.resolve("disconnected")
        for op_id in list(self.ops):
            self.op_results.append((op_id, _errors().ConnectionClosed(reason)))
        self.ops.clear()
        self.set_ctrl("closed", reason)
        self.wake.notify(WAKE_TRACKS)
        self.wake.notify(WAKE_OPS)
        self.wake.notify(WAKE_CONN)  # T9
        self.wake.close()
        _REGISTRY.discard(self)

    # ── worker tick (Harness.step) ───────────────────────────────────────────────────────────────

    def worker_step(self, now: float) -> None:
        if self.closing:
            return
        self.deadlines(now)
        self._receive(now)
        for q in list(self.playbacks):
            q.housekeep(now)
        self.playbacks = [q for q in self.playbacks if not q.ended_emitted]
        for s in list(self.sinks):
            if isinstance(s, EncodedSink):
                s._tick()
        self._poll_tracks()
        self._finish_ops()

    def _poll_tracks(self) -> None:
        rung = False
        for t in self.tracks:
            if t.ended is not None and not t.reported:
                t.reported = True
                self.emit(_EV.TrackEnded(t.id, t.ended))
                rung = True
        self.tracks = [t for t in self.tracks if not (t.reported and t.ended)]
        # queue advance
        if (self.current is None or self.current.ended is not None) and not self.queue_paused and self.queue:
            nxt = self.queue.popleft()
            self.current = nxt
            nxt.attach_control(initially_paused=nxt.paused)
            if nxt.lazy_stream is not None:
                max_bytes, _vol = nxt.lazy_stream
                nxt.sink = EncodedSink(self, max_bytes, nxt)
                self.sinks.append(nxt.sink)
            self.tracks.append(nxt)
            rung = True
        elif self.current is not None and self.current.ended is not None and not self.queue:
            self.current = None
        if rung:
            self.wake.notify(WAKE_TRACKS)

    def _finish_ops(self) -> None:
        if not self.ops:
            return
        for op_id, (kind, t, arg) in list(self.ops.items()):
            if kind == "info":
                playing = "end" if t.ended else ("pause" if t.paused else "play")
                res: Any = TrackInfo(playing, t.volume, 0, 0, t.loops)
            else:
                res = float(arg)
            self.op_results.append((op_id, res))
        self.ops.clear()
        self.wake.notify(WAKE_OPS)

    # ── receive pipeline (simplified §8) ─────────────────────────────────────────────────────────

    def _receive(self, now: float) -> None:
        expected = self.expected_tick(now)
        while self.rx_in:
            item = self.rx_in.popleft()
            kind = item[0]
            if kind == "tick":
                _, voices, silent, arrived = item
                if self.tick_index - expected > 5 and not any(v[1] is not None for v in voices):
                    self.counters["ticks_skipped"] += 1
                    continue
                self._tick(voices, synthetic=False, at=arrived)
            elif kind == "map":
                self._map(item[1], item[2])
            elif kind == "left":
                self._user_left(item[1])
            elif kind == "link":
                self._link(item[1])
        if self.settings.receive and (self.subs or self.routers):
            if self.tick_index < expected - 5:
                while self.tick_index < expected - 2:
                    self._tick([], synthetic=True, at=now)
        elif self.tick_index < expected:
            self.tick_index = expected

    def _map(self, ssrc: int, user: int) -> None:
        if self.ssrc_users.get(ssrc) == user:
            return
        self.ssrc_users[ssrc] = user
        self.emit(_EV.UserMapped(user, ssrc))
        if user not in self.known_users:
            self.known_users.add(user)
            self.unclassified.append(user)
            self.set_ctrl("classify")

    def _user_left(self, user: int) -> None:
        if user in self.speaking:
            start, last = self.speaking.pop(user)
            self.emit(_EV.SpeakingStopped(user, self.tick_index * 20, (last - start + 1) * 20))
        self.emit(_EV.UserLeft(user))
        for r in self.routers:
            q = r.streams.pop(user, None)
            if q is not None and r.spec["close_on_leave"]:
                q.close("user_left")

    @staticmethod
    def _decode(packet: bytes | None) -> array.array | None:
        if packet is None or not packet.startswith(_FAKE_OPUS_TAG) or len(packet) != 164:
            return None
        out = array.array("h", bytes(3840))
        for i, b in enumerate(packet[4:]):
            s = _ULAW_DECODE[b]
            base = i * 12
            for j in range(0, 12, 2):
                out[base + j] = s
                out[base + j + 1] = s
        return out

    def _tick(self, voices: list[tuple[int, bytes | None, bool]], *, synthetic: bool, at: float) -> None:
        tick = self.tick_index
        users: dict[int, tuple[array.array | None, bool, bool]] = {}
        unmapped: list[array.array] = []
        if synthetic:
            self.counters["ticks_synthetic"] += 1
        else:
            self.counters["ticks_real"] += 1
        for ssrc, packet, bad in voices:
            if bad:
                self.counters["bad_packets"] += 1
                packet = None
            pcm = self._decode(packet)
            voiced = packet is not None and packet != _SILENT_FRAME
            concealed = packet is None and not bad
            uid = self.ssrc_users.get(ssrc)
            if uid is None:
                if pcm is not None:
                    unmapped.append(pcm)
                continue
            prev = users.get(uid)
            if prev is not None and prev[0] is not None and pcm is not None:
                pcm = _mix([prev[0], pcm])
            users[uid] = (
                pcm if pcm is not None else (prev[0] if prev else None),
                voiced or bool(prev and prev[1]),
                concealed,
            )
        # speaking FSM
        hang = max(1, self.settings.hangover_ms // 20)
        for uid, (_pcm, voiced, concealed) in users.items():
            st = self.speaking.get(uid)
            if voiced or (concealed and st is not None and tick - st[1] <= 3):
                if st is None:
                    self.speaking[uid] = [tick, tick]
                    self.emit(_EV.SpeakingStarted(uid, tick * 20, self.captured_at(at)))
                else:
                    st[1] = tick
        for uid, st in list(self.speaking.items()):
            if tick - st[1] >= hang:
                del self.speaking[uid]
                self.emit(_EV.SpeakingStopped(uid, tick * 20, (st[1] - st[0] + 1) * 20))
        # routers
        for r in self.routers:
            for uid, (_pcm, voiced, _c) in users.items():
                spec = r.spec
                if not voiced or uid in r.streams or r.closed:
                    continue
                if spec["include"] is not None and uid not in spec["include"]:
                    continue
                if spec["ignore_bots"] and uid in self.bots:
                    continue
                try:
                    q = self._new_sub(
                        ("user", uid), spec["fmt"], spec["chunk_ms"], spec["max_queue_ms"], spec["overflow"]
                    )
                except Exception:
                    continue
                r.streams[uid] = q
                r.push(ReceiveSubscription(q))
        # lanes
        for q in list(self.subs):
            if q.closed is not None:
                continue
            if q.pending_filter is not None and q.lane_pos is not None and not q.acc:
                q.source = ("mixed", q.pending_filter)
                q.pending_filter = None
            frame, speakers, voiced, concealed = self._source_frame(q, users, unmapped)
            self._feed(q, tick, frame, speakers, voiced, concealed, synthetic, at)
        self.subs = [q for q in self.subs if q.closed is None]
        self.tick_index += 1

    def _source_frame(
        self,
        q: _SubQueue,
        users: dict[int, tuple[array.array | None, bool, bool]],
        unmapped: list[array.array],
    ) -> tuple[array.array | None, list[int], bool, bool]:
        kind, spec = q.source
        if kind == "user":
            pcm, voiced, concealed = users.get(spec, (None, False, False))
            return pcm, ([spec] if voiced else []), voiced, concealed
        sel = []
        for uid, (pcm, voiced, _c) in users.items():
            if spec["include"] is not None and uid not in spec["include"]:
                continue
            if uid in spec["exclude"] or (spec["ignore_bots"] and uid in self.bots):
                continue
            sel.append((uid, pcm, voiced))
        if spec["policy"] == "floor":
            holder = spec["holder"]
            voiced_now = [u for u, _p, v in sel if v]
            if holder is not None and holder not in voiced_now:
                spec["holder_quiet_ms"] += 20
                if spec["holder_quiet_ms"] >= spec["floor_release_ms"]:
                    holder = None
            elif holder is not None:
                spec["holder_quiet_ms"] = 0
            if holder is None and voiced_now:
                holder = voiced_now[0]
                spec["holder_quiet_ms"] = 0
            spec["holder"] = holder
            sel = [s for s in sel if s[0] == holder]
        frames = [p for _u, p, _v in sel if p is not None]
        if spec["include"] is None and spec["policy"] == "mix":
            frames += unmapped
        speakers = sorted(u for u, _p, v in sel if v)
        return (_mix(frames) if frames else None), speakers, bool(speakers), False

    def _feed(
        self,
        q: _SubQueue,
        tick: int,
        frame: array.array | None,
        speakers: list[int],
        voiced: bool,
        concealed: bool,
        synthetic: bool,
        at: float,
    ) -> None:
        fmt = q.fmt
        n = fmt.sample_rate // 50
        tick_start = tick * n
        if q.lane_pos is None:
            q.lane_pos = -(-tick_start // q.chunk_frames) * q.chunk_frames
        samples = _convert(frame, fmt, n)
        for i in range(n):
            pos = tick_start + i
            if pos < q.lane_pos:
                continue
            if q.acc_meta is None:
                q.acc_meta = {
                    "user_id": q.source[1] if q.source[0] == "user" else None,
                    "timeline_ms": pos * 1000 // fmt.sample_rate,
                    "captured_at": self.captured_at(at),
                    "voiced_frames": 0,
                    "concealed_frames": 0,
                    "gap_frames": 0,
                    "speakers": set(),
                }
            m = q.acc_meta
            if voiced:
                m["voiced_frames"] += 1
            if concealed:
                m["concealed_frames"] += 1
            if synthetic:
                m["gap_frames"] += 1
            m["speakers"].update(speakers)
            q.acc.extend(samples[i * fmt.channels : (i + 1) * fmt.channels])
            if len(q.acc) >= q.chunk_frames * fmt.channels:
                self._emit_chunk(q)

    def _emit_chunk(self, q: _SubQueue) -> None:
        fmt = q.fmt
        samples, q.acc = q.acc, []
        m, q.acc_meta = q.acc_meta, None
        assert m is not None
        q.lane_pos = (q.lane_pos or 0) + q.chunk_frames
        peak = max((abs(s) for s in samples), default=0) / 32768.0
        rms = math.sqrt(sum(s * s for s in samples) / len(samples)) / 32768.0 if samples else 0.0
        meta = {
            "user_id": m["user_id"],
            "timeline_ms": m["timeline_ms"],
            "captured_at": m["captured_at"],
            "frames": q.chunk_frames,
            "voiced_ms": m["voiced_frames"] * 1000 // fmt.sample_rate,
            "concealed_ms": m["concealed_frames"] * 1000 // fmt.sample_rate,
            "gap_ms": m["gap_frames"] * 1000 // fmt.sample_rate,
            "speakers": frozenset(m["speakers"]),
            "rms_dbfs": max(-100.0, 20 * math.log10(rms)) if rms > 0 else -100.0,
            "peak": peak,
        }
        q.push(_encode_samples(fmt, samples), meta)

    def _new_sub(
        self, source: tuple[str, Any], fmt: AudioFormat, chunk_ms: int, max_queue_ms: int, overflow: str
    ) -> _SubQueue:
        if len(self.subs) >= 64:
            raise _limit("subscriptions", len(self.subs) + 1, 0, 64)
        q = _SubQueue(self, source, fmt, chunk_ms, max_queue_ms, overflow)
        used = sum(s.budget for s in self.subs if s.closed is None)
        if used + q.budget > self.settings.max_receive_bytes:
            self.wake.free(q.wake_id)
            raise _limit("receive_budget", used + q.budget, 0, self.settings.max_receive_bytes)
        self.subs.append(q)
        return q

    # ── tracks helpers ───────────────────────────────────────────────────────────────────────────

    def _on_track_paused(self, t: _Track, paused: bool) -> None:
        for q in self.playbacks:
            if q.track is t:
                with q.lock:
                    q.paused_user = paused

    def _on_track_stopped(self, t: _Track) -> None:
        for q in self.playbacks:
            if q.track is t:
                q.close("abort")
        for s in self.sinks:
            if getattr(s, "_track", None) is t:
                s.close()

    def _queue_remove(self, t: _Track) -> None:
        try:
            self.queue.remove(t)
        except ValueError:
            pass
        if t not in self.tracks:
            self.tracks.append(t)

    def playing(self) -> int:
        n = sum(
            1 for t in self.tracks if t.ended is None and t.kind in ("file", "pcm", "opus") and t is not self.current
        )
        if self.current is not None and self.current.ended is None:
            n += 1
        return n


def _mix(frames: list[array.array]) -> array.array:
    if len(frames) == 1:
        return frames[0]
    out = array.array("h", bytes(len(frames[0]) * 2))
    for i in range(len(out)):
        out[i] = max(-32768, min(32767, sum(f[i] for f in frames)))
    return out


def _convert(frame: array.array | None, fmt: AudioFormat, n: int) -> list[int]:
    """48 kHz stereo int16 frame → ``n`` frames at ``fmt`` (naive decimation)."""
    if frame is None:
        return [0] * (n * fmt.channels)
    step = 48_000 // fmt.sample_rate
    out: list[int] = []
    for i in range(n):
        left, right = frame[2 * i * step], frame[2 * i * step + 1]
        if fmt.channels == 2:
            out += (left, right)
        else:
            out.append((left + right) // 2)
    return out


class Connection:
    """The native half of one voice connection (fake)."""

    def __init__(self, bell: WakeBell, guild_id: int, user_id: int, settings: VoiceSettings, reconnect: bool) -> None:
        _ensure_not_forked()
        if not isinstance(bell, WakeBell):
            raise TypeError("bell must be a WakeBell")
        if not isinstance(settings, VoiceSettings):
            raise TypeError("settings must be a VoiceSettings")
        g = _check_int("guild_id", guild_id, 0, 2**64 - 1)
        u = _check_int("user_id", user_id, 0, 2**64 - 1)
        if g == 0 or u == 0:
            raise ValueError("guild_id and user_id must be non-zero")
        door = bell._take()
        if door is None:
            raise ValueError("this WakeBell was already taken")
        _STATE["runtime_started"] = True
        self._c = _Conn(door, g, u, settings, reconnect, detached=False)

    @classmethod
    def _from_conn(cls, c: _Conn) -> Connection:
        self = cls.__new__(cls)
        self._c = c
        return self

    def __del__(self) -> None:
        c = getattr(self, "_c", None)
        if c is not None:
            c.close("dropped")

    def __repr__(self) -> str:
        return f"<Connection guild={self._c.guild_id} state={self._c.status().state}>"

    # gateway
    def request_join(self, channel_id: int, self_mute: bool, self_deaf: bool) -> None:
        if channel_id == 0:
            raise ValueError("channel_id must be non-zero")
        self._c.join(channel_id, self_mute, self_deaf)

    def request_move(self, channel_id: int) -> None:
        if channel_id == 0:
            raise ValueError("channel_id must be non-zero")
        self._c.move(channel_id)

    def begin_rejoin(self) -> None:
        self._c.begin_rejoin()

    def request_leave(self) -> None:
        self._c.leave()

    def on_voice_state(self, session_id: str, channel_id: int | None) -> None:
        self._c.voice_state(session_id, channel_id)

    def on_voice_server(self, token: str, endpoint: str | None) -> None:
        self._c.voice_server(token, endpoint)

    def set_self_flags(self, mute: bool, deaf: bool) -> None:
        self._c.set_flags(mute, deaf)

    def status(self) -> ConnectionStatus:
        return self._c.status()

    @property
    def timeline_ms(self) -> int:
        return self._c.tick_index * 20

    @property
    def closed(self) -> bool:
        return self._c.closing

    def take_ready(self) -> list[int]:
        return self._c.wake.take_ready()

    def take_controls(self) -> list[Any]:
        return self._c.take_controls()

    def take_unclassified_users(self) -> list[int]:
        with self._c.lock:
            out, self._c.unclassified = self._c.unclassified, []
            return out

    def set_users_bot(self, users: Iterable[tuple[int, bool]]) -> None:
        with self._c.lock:
            for uid, bot in users:
                self._c.known_users.add(uid)
                if bot:
                    self._c.bots.add(uid)
                else:
                    self._c.bots.discard(uid)

    # receive
    def _rx_check(self, fmt: Any, chunk_ms: int, max_queue_ms: int, overflow: str) -> AudioFormat:
        c = self._c
        if c.closing:
            raise _errors().ConnectionClosed(c.close_reason or "closed")
        if not c.settings.receive:
            raise _errors().ReceiveDisabled("receive is disabled (VoiceConfig(receive=False))")
        if not isinstance(fmt, AudioFormat):
            raise TypeError("fmt must be an AudioFormat")
        _check_int("chunk_ms", chunk_ms, 10, 1_000)
        _check_int("max_queue_ms", max_queue_ms, chunk_ms, 60_000)
        if overflow not in _OVERFLOWS:
            raise ValueError(f"overflow must be one of {_OVERFLOWS} (got {overflow!r})")
        return fmt

    def _sync_timeline(self) -> None:
        c = self._c
        exp = c.expected_tick(c.clock.now())
        if not c.subs and not c.routers and c.tick_index < exp:
            c.tick_index = exp

    def subscribe_user(
        self, uid: int, fmt: AudioFormat, chunk_ms: int = 20, max_queue_ms: int = 2_000, overflow: str = "drop_oldest"
    ) -> ReceiveSubscription:
        f = self._rx_check(fmt, chunk_ms, max_queue_ms, overflow)
        _check_int("user_id", uid, 0, 2**64 - 1)
        with self._c.lock:
            self._sync_timeline()
            return ReceiveSubscription(self._c._new_sub(("user", uid), f, chunk_ms, max_queue_ms, overflow))

    def subscribe_mixed(
        self,
        fmt: AudioFormat,
        chunk_ms: int = 20,
        max_queue_ms: int = 2_000,
        overflow: str = "drop_oldest",
        include: Iterable[int] | None = None,
        exclude: Iterable[int] = (),
        ignore_bots: bool = True,
        policy: str = "mix",
        floor_release_ms: int = 600,
    ) -> ReceiveSubscription:
        f = self._rx_check(fmt, chunk_ms, max_queue_ms, overflow)
        if policy not in ("mix", "floor"):
            raise ValueError(f"policy must be 'mix' or 'floor' (got {policy!r})")
        _check_int("floor_release_ms", floor_release_ms, 0, 2**32 - 1)
        with self._c.lock:
            self._sync_timeline()
            spec = _mix_filter(include, exclude, ignore_bots, policy, floor_release_ms)
            return ReceiveSubscription(self._c._new_sub(("mixed", spec), f, chunk_ms, max_queue_ms, overflow))

    def subscribe_router(
        self,
        fmt: AudioFormat,
        chunk_ms: int = 20,
        max_queue_ms: int = 2_000,
        overflow: str = "drop_oldest",
        include: Iterable[int] | None = None,
        ignore_bots: bool = True,
        close_on_leave: bool = True,
    ) -> RouterSubscription:
        f = self._rx_check(fmt, chunk_ms, max_queue_ms, overflow)
        spec = {
            "fmt": f,
            "chunk_ms": chunk_ms,
            "max_queue_ms": max_queue_ms,
            "overflow": overflow,
            "include": None if include is None else frozenset(include),
            "ignore_bots": ignore_bots,
            "close_on_leave": close_on_leave,
        }
        with self._c.lock:
            self._sync_timeline()
            r = _RouterQueue(self._c, spec)
            self._c.routers.append(r)
            return RouterSubscription(r)

    def subscribe_events(self, max_queue: int = 256, kind_mask: int = _ALL_KINDS) -> EventSubscription:
        c = self._c
        if c.closing:
            raise _errors().ConnectionClosed(c.close_reason or "closed")
        _check_int("max_queue", max_queue, 1, 4_096)
        _check_int("kind_mask", kind_mask, 0, _ALL_KINDS)
        with c.lock:
            c.event_queues = [q for q in c.event_queues if not q.closed]
            if len(c.event_queues) >= 16:
                raise _limit("event_streams", len(c.event_queues) + 1, 0, 16)
            q = _EventQueue(c, max_queue, kind_mask)
            c.event_queues.append(q)
            return EventSubscription(q)

    # send
    def _live_check(self) -> None:
        c = self._c
        if c.closing:
            raise _errors().ConnectionClosed(c.close_reason or "closed")
        live = sum(1 for q in c.playbacks if q.closed is None) + sum(
            1 for s in c.sinks if not getattr(s, "_closed", False)
        )
        if live >= 16:
            raise _limit("live_sources", live + 1, 0, 16)

    def open_playback(
        self,
        fmt: AudioFormat,
        max_buffer_ms: int = 60_000,
        prebuffer_ms: int = 0,
        idle_ms: int = 250,
        auto_pause: bool = True,
        max_gap_ms: int = 2_000,
        output_latency_ms: int = 40,
        volume: float = 1.0,
        when_disconnected: str = "buffer",
    ) -> Playback:
        self._live_check()
        if not isinstance(fmt, AudioFormat):
            raise TypeError("fmt must be an AudioFormat")
        _check_int("max_buffer_ms", max_buffer_ms, 100, 120_000)
        _check_int("prebuffer_ms", prebuffer_ms, 0, 1_000)
        _check_int("idle_ms", idle_ms, 20, 60_000)
        _check_int("max_gap_ms", max_gap_ms, 0, 10_000)
        _check_int("output_latency_ms", output_latency_ms, 0, 1_000)
        _check("volume", volume, 0.0, 4.0)
        if prebuffer_ms > max_buffer_ms // 2:
            raise _limit("prebuffer_ms", prebuffer_ms, 0, max_buffer_ms // 2)
        if max_gap_ms > max_buffer_ms // 2:
            raise _limit("max_gap_ms", max_gap_ms, 0, max_buffer_ms // 2)
        if when_disconnected not in ("buffer", "discard"):
            raise ValueError("when_disconnected must be 'buffer' or 'discard'")
        c = self._c
        spec = {
            "max_buffer_ms": max_buffer_ms,
            "prebuffer_ms": prebuffer_ms,
            "idle_ms": idle_ms,
            "auto_pause": auto_pause,
            "max_gap_ms": max_gap_ms,
            "output_latency_ms": output_latency_ms,
            "volume": volume,
            "when_disconnected": when_disconnected,
        }
        with c.lock:
            t = _Track(c, "playback")
            t.attach_control(initially_paused=True)
            q = _PcmQueue(c, fmt, spec, t)
            c.playbacks.append(q)
            c.tracks.append(t)
            c.parked[q.id] = q
            return Playback(q)

    def open_encoded(self, max_buffer_bytes: int = 1 << 20, volume: float = 1.0) -> EncodedSink:
        self._live_check()
        _check_int("max_buffer_bytes", max_buffer_bytes, 64 << 10, 16 << 20)
        _check("volume", volume, 0.0, 4.0)
        c = self._c
        with c.lock:
            t = _Track(c, "encoded_stream", opts=(volume, 0, False))
            t.attach_control(initially_paused=True)
            s = EncodedSink(c, max_buffer_bytes, t)
            c.sinks.append(s)
            c.tracks.append(t)
            return s

    def open_opus(self, max_packets: int = 250, volume: float = 1.0) -> OpusSink:
        self._live_check()
        _check_int("max_packets", max_packets, 1, 3_000)
        _check("volume", volume, 0.0, 4.0)
        c = self._c
        with c.lock:
            t = _Track(c, "opus_stream", opts=(volume, 0, False))
            t.attach_control(initially_paused=True)
            s = OpusSink(c, max_packets, t)
            c.sinks.append(s)
            c.tracks.append(t)
            return s

    @staticmethod
    def _source_kind(source: Any) -> str:
        if isinstance(source, AudioFile):
            return "file"
        if isinstance(source, RawPcm):
            return "pcm"
        if isinstance(source, OpusPackets):
            return "opus"
        raise TypeError("source must be an AudioFile, RawPcm or OpusPackets")

    @staticmethod
    def _opts(volume: float, loops: int | None, paused: bool) -> tuple[float, Any, bool]:
        _check("volume", volume, 0.0, 4.0)
        if loops is not None:
            _check_int("loops", loops, 0, 2**32 - 2)
        return float(volume), loops, bool(paused)

    def play(self, source: Any, volume: float = 1.0, loops: int | None = 0, paused: bool = False) -> NativeTrack:
        c = self._c
        if c.closing:
            raise _errors().ConnectionClosed(c.close_reason or "closed")
        kind = self._source_kind(source)
        opts = self._opts(volume, loops, paused)
        with c.lock:
            if c.playing() >= _PLAYING_CAP:
                raise _limit("playing_tracks", c.playing() + 1, 0, _PLAYING_CAP)
            t = _Track(c, kind, source=source, opts=opts)
            t.attach_control(initially_paused=paused)
            c.tracks.append(t)
            return NativeTrack(t)

    def _enqueue(self, t: _Track) -> NativeTrack:
        c = self._c
        if len(c.queue) >= _QUEUE_CAP:
            raise _limit("queue_entries", len(c.queue) + 1, 0, _QUEUE_CAP)
        t.pending = True
        c.queue.append(t)
        return NativeTrack(t)

    def enqueue(self, source: Any, volume: float = 1.0, loops: int | None = 0, paused: bool = False) -> NativeTrack:
        c = self._c
        if c.closing:
            raise _errors().ConnectionClosed(c.close_reason or "closed")
        kind = self._source_kind(source)
        opts = self._opts(volume, loops, paused)
        with c.lock:
            return self._enqueue(_Track(c, kind, source=source, opts=opts))

    def enqueue_stream(
        self, volume: float = 1.0, loops: int | None = 0, paused: bool = False, max_buffer_bytes: int = 1 << 20
    ) -> NativeTrack:
        c = self._c
        if c.closing:
            raise _errors().ConnectionClosed(c.close_reason or "closed")
        opts = self._opts(volume, loops, paused)
        _check_int("max_buffer_bytes", max_buffer_bytes, 64 << 10, 16 << 20)
        with c.lock:
            t = _Track(c, "encoded_stream", opts=opts)
            t.lazy_stream = (max_buffer_bytes, volume)
            return self._enqueue(t)

    def queue_take_stream(self, track: NativeTrack) -> EncodedSink | None:
        t = track._t
        with self._c.lock:
            if t is self._c.current and t.sink is not None and not t.taken:
                t.taken = True
                return t.sink
            return None

    def queue_current(self) -> NativeTrack | None:
        cur = self._c.current
        return None if cur is None or cur.ended is not None else NativeTrack(cur)

    def queue_snapshot(self) -> list[NativeTrack]:
        return [NativeTrack(t) for t in self._c.queue]

    def queue_skip(self) -> None:
        cur = self._c.current
        if cur is not None and cur.ended is None:
            NativeTrack(cur).stop()

    def queue_stop(self) -> None:
        self.queue_clear()
        self.queue_skip()

    def queue_pause(self) -> None:
        self._c.queue_paused = True
        if self._c.current is not None:
            NativeTrack(self._c.current).pause()

    def queue_resume(self) -> None:
        self._c.queue_paused = False
        if self._c.current is not None:
            NativeTrack(self._c.current).resume()

    def queue_clear(self) -> None:
        c = self._c
        with c.lock:
            pending = list(c.queue)
            c.queue.clear()
            for t in pending:
                t.resolve("stopped")
                c.tracks.append(t)

    def queue_dequeue(self, i: int) -> NativeTrack:
        c = self._c
        with c.lock:
            if not isinstance(i, int) or i < 0 or i >= len(c.queue):
                raise IndexError("queue index out of range")
            t = c.queue[i]
            del c.queue[i]
            t.resolve("stopped")
            c.tracks.append(t)
            return NativeTrack(t)

    def queue_len(self) -> int:
        return len(self._c.queue)

    def stop_all(self) -> None:
        c = self._c
        self.queue_clear()
        with c.lock:
            for t in list(c.tracks) + ([c.current] if c.current is not None else []):
                if t.ended is None:
                    NativeTrack(t).stop()
            for q in c.playbacks:
                q.close("abort")
            for s in c.sinks:
                s.close()

    def set_bitrate(self, bps: int | None) -> None:
        if bps is not None:
            _check_int("bitrate", bps, 6_000, 510_000)

    def _start_op(self, track: NativeTrack, kind: str, arg: Any) -> int:
        c = self._c
        t = track._t
        if c.closing:
            raise _errors().ConnectionClosed(c.close_reason or "closed")
        if t.pending:
            raise _errors().TrackError("track has not started")
        with c.lock:
            if len(c.ops) >= _OPS_CAP:
                raise _limit("pending_ops", len(c.ops) + 1, 0, _OPS_CAP)
            op_id = c.next_op
            c.next_op += 1
            c.ops[op_id] = (kind, t, arg)
            return op_id

    def start_track_info(self, track: NativeTrack, timeout_ms: int) -> int:
        _check_int("timeout_ms", timeout_ms, 0, 2**64 - 1)
        return self._start_op(track, "info", None)

    def start_track_seek(self, track: NativeTrack, seconds: float, timeout_ms: int) -> int:
        _check("seconds", seconds, 0.0, 86_400.0)
        _check_int("timeout_ms", timeout_ms, 0, 2**64 - 1)
        if track._t.kind in ("playback", "encoded_stream", "opus_stream"):
            raise _errors().TrackError("seek is not supported for live sources")
        return self._start_op(track, "seek", seconds)

    def take_op_results(self) -> list[tuple[int, Any]]:
        with self._c.lock:
            out, self._c.op_results = self._c.op_results, []
            return out

    def close(self, reason: str = "closed") -> None:
        if reason not in _CLOSE_REASONS:
            raise ValueError(f"unknown close reason {reason!r}")
        self._c.close(reason)

    def stats(self) -> dict[str, int]:
        c = self._c
        out = dict(c.counters)
        out["subscriptions"] = len(c.subs)
        out["lanes"] = len(c.subs)
        out["decoders_active"] = len(c.ssrc_users)
        return out


# ── module functions ───────────────────────────────────────────────────────────────────────────────

_LOG_RING: deque[tuple[int, str, str, float]] = deque(maxlen=1024)
_LOG_COND = threading.Condition()
_LOG_LEVELS: dict[str, int] = {"sbcore": 20, "discord_ext_songbird_native": 20, "": 30}
_LOG_DROPPED = [0]
_LEVELS = {"TRACE": 5, "DEBUG": 10, "INFO": 20, "WARNING": 30, "WARN": 30, "ERROR": 40, "CRITICAL": 50, "OFF": 50}


def supported_codecs() -> list[str]:
    return [
        "opus",
        "dca",
        "raw-pcm",
        "aac",
        "adpcm",
        "alac",
        "flac",
        "mp1",
        "mp2",
        "mp3",
        "pcm",
        "vorbis",
        "format:aiff",
        "format:caf",
        "format:isomp4",
        "format:mkv",
        "format:ogg",
        "format:wav",
    ]


def configure_runtime(net_workers: int = 2, rx_workers: int = 1) -> None:
    _ensure_not_forked()
    _check_int("net_workers", net_workers, 1, 8)
    _check_int("rx_workers", rx_workers, 1, 8)
    if _STATE["runtime_started"]:
        raise RuntimeError("runtime already configured: configure_runtime() must be called before the first connection")


def set_log_level(prefix: str, level: int | str) -> None:
    if isinstance(level, str):
        try:
            lv = _LEVELS[level.upper()]
        except KeyError:
            raise ValueError(f"unknown log level {level!r}") from None
    elif isinstance(level, int) and not isinstance(level, bool):
        lv = level
    else:
        raise TypeError("level must be an int or a level name")
    _LOG_LEVELS[prefix] = lv


def _log_level_for(target: str) -> int:
    best, level = -1, _LOG_LEVELS.get("", 30)
    for prefix, lv in _LOG_LEVELS.items():
        if prefix and (target == prefix or target.startswith(prefix + "::")) and len(prefix) > best:
            best, level = len(prefix), lv
    return level


def _emit_log(level: int, target: str, message: str) -> bool:
    """Fake-only: push a record as a Rust thread would (any thread; never blocks)."""
    if _STATE["shutting_down"] or level < _log_level_for(target) or level >= 50 and _log_level_for(target) >= 50:
        return False
    with _LOG_COND:
        if len(_LOG_RING) == _LOG_RING.maxlen:
            _LOG_DROPPED[0] += 1
        _LOG_RING.append((level, target, message[:512], time.time()))
        _LOG_COND.notify_all()
    return True


def _log_wait(max: int, timeout_s: float) -> list[tuple[int, str, str, float]]:
    n = min(1024, _max(1, max))
    t = _min(60.0, _max(0.0, timeout_s))
    with _LOG_COND:
        if not _LOG_RING and not _STATE["shutting_down"] and t > 0:
            _LOG_COND.wait(t)
        out = []
        while _LOG_RING and len(out) < n:
            out.append(_LOG_RING.popleft())
        return out


def _log_dropped() -> int:
    n, _LOG_DROPPED[0] = _LOG_DROPPED[0], 0
    return n


def _shutdown_all() -> None:
    _STATE["shutting_down"] = True
    with _LOG_COND:
        _LOG_COND.notify_all()
    for c in list(_REGISTRY):
        c.close("process_exit")


def _disable_after_fork() -> None:
    _STATE["forked"] = True


def _debug_live_counts() -> dict[str, int]:
    with _LIVE_LOCK:
        out = dict(_LIVE)
    out["songbird_live_tasks"] = 0
    return out


# ── native._testing ────────────────────────────────────────────────────────────────────────────────


class Harness:
    """The worker and manual clock of a detached connection (fake)."""

    def __init__(self, c: _Conn) -> None:
        self._c = c

    def step(self, advance_ms: int = 0) -> None:
        _check_int("advance_ms", advance_ms, 0, 3_600_000)
        c = self._c
        target = c.clock.elapsed + advance_ms / 1000
        while True:
            with c.lock:
                c.worker_step(c.clock.now())
            remaining = target - c.clock.elapsed
            if remaining <= 1e-9:
                return
            c.clock.advance(_min(_TICK_S, remaining))

    @property
    def elapsed_ms(self) -> int:
        return round(self._c.clock.elapsed * 1000)

    @property
    def parked(self) -> int:
        return len(self._c.parked)


def _detached_connection(
    guild_id: int, user_id: int, settings: VoiceSettings, bell: WakeBell | None = None, *, reconnect: bool = True
) -> tuple[Connection, Harness]:
    _ensure_not_forked()
    g = _check_int("guild_id", guild_id, 0, 2**64 - 1)
    u = _check_int("user_id", user_id, 0, 2**64 - 1)
    if g == 0 or u == 0:
        raise ValueError("guild_id and user_id must be non-zero")
    if bell is not None:
        door = bell._take()
        if door is None:
            raise ValueError("this WakeBell was already taken")
    else:
        door = _Doorbell(None)
    c = _Conn(door, g, u, settings, reconnect, detached=True)
    return Connection._from_conn(c), Harness(c)


def _rx_push(conn: Connection, item: tuple[Any, ...]) -> None:
    c = conn._c
    if c.closing or not c.settings.receive:
        return
    if len(c.rx_in) >= 50:
        raise _limit("tick_channel", 51, 0, 50)
    c.rx_in.append(item)


def _seq(conn: Connection, ssrc: int) -> int:
    seqs = conn._c.__dict__.setdefault("_seqs", {})
    cur = seqs.get(ssrc, 0)
    seqs[ssrc] = (cur + 1) & 0xFFFF
    return cur


def _inject_tick(conn: Connection, voices: list[tuple[int, bytes | None]], silent: Iterable[int] = ()) -> None:
    silent = list(silent)
    if len(voices) > 256 or len(silent) > 256:
        raise _limit("tick_voices", _max(len(voices), len(silent)), 0, 256)
    vs = []
    for ssrc, payload in voices:
        _check_int("ssrc", ssrc, 0, 2**32 - 1)
        _seq(conn, ssrc)
        vs.append((ssrc, None if payload is None else bytes(payload), False))
    _rx_push(conn, ("tick", vs, list(silent), conn._c.clock.now()))


def _inject_map(conn: Connection, ssrc: int, user_id: int) -> None:
    _check_int("ssrc", ssrc, 0, 2**32 - 1)
    _check_int("user_id", user_id, 0, 2**64 - 1)
    _rx_push(conn, ("map", ssrc, user_id))


def _inject_user_left(conn: Connection, user_id: int) -> None:
    _rx_push(conn, ("left", user_id))


def _inject_driver_event(
    conn: Connection, kind: str, reason: str | None = None, code: int | None = None, ssrc: int = 0
) -> None:
    c = conn._c
    if kind == "connect":
        c.driver_connect(ssrc)
        if c.phase == "connecting":
            c.connect_result(True)
        _rx_push(conn, ("link", True))
    elif kind in ("connect_failed", "connect_timeout"):
        c.connect_result(False, timed_out=kind == "connect_timeout")
    elif kind == "reconnect":
        c.driver_reconnect(ssrc)
        _rx_push(conn, ("link", True))
    elif kind == "disconnect" or kind.startswith("disconnect:"):
        dkind = kind.partition(":")[2] or "runtime"
        if dkind not in ("connect", "reconnect", "runtime"):
            raise ValueError(f"unknown disconnect kind {dkind!r}")
        c.driver_disconnect(dkind, reason, code)
        _rx_push(conn, ("link", False))
    else:
        raise ValueError(f"unknown driver event {kind!r}")


def _rtp_wrap(
    opus: bytes, seq: int, ssrc: int, *, extension: bool = False, padding: int = 0, dave_trailer: bool = False
) -> bytes:
    _check_int("seq", seq, 0, 0xFFFF)
    _check_int("padding", padding, 0, 255)
    total = 12 + (8 if extension else 0) + len(opus) + (12 if dave_trailer else 0) + padding + 20
    _check_int("rtp_packet_bytes", total, 0, 1_460)
    b0 = 0x80 | (0x20 if padding else 0) | (0x10 if extension else 0)
    pkt = bytearray(struct.pack(">BBHII", b0, 120, seq, (seq * 960) & 0xFFFFFFFF, ssrc))
    if extension:
        pkt += b"\xbe\xde\x00\x01\x10\xaa\x00\x00"
    pkt += bytes(opus)
    if dave_trailer:
        pkt += b"\xd1\xd2\xd3\xd4\xd5\xd6\xd7\xd8\x01\x0c\xfa\xfa"
    if padding:
        pkt += bytes(padding - 1) + bytes([padding])
    pkt += bytes(20)
    return bytes(pkt)


def _extract_opus(pkt: bytes) -> bytes | None:
    """Python port of ``rx::opus::extract_opus`` for packets with the 20-byte crypto suffix."""
    if len(pkt) < 12 or pkt[0] >> 6 != 2:
        return None
    cc = pkt[0] & 0x0F
    hdr = 12 + 4 * cc
    s, e = hdr, len(pkt) - 20
    if e <= s:
        return None
    if pkt[0] & 0x10:
        if s + 4 > e:
            return None
        words = struct.unpack_from(">H", pkt, s + 2)[0]
        s += 4 + 4 * words
    if pkt[0] & 0x20:
        e -= pkt[e - 1]
    if s >= e:
        return None
    body = pkt[s:e]
    if len(body) >= 11 and body.endswith(b"\xfa\xfa"):
        n = body[-3]
        if 11 <= n < len(body) and (_opus_samples(body[:-n]) or 0) in range(120, 5761):
            body = body[:-n]
    return body or None


def _inject_voice_tick_rtp(conn: Connection, packets: list[tuple[int, bytes]]) -> None:
    vs = []
    for ssrc, pkt in packets:
        body = _extract_opus(bytes(pkt))
        vs.append((ssrc, body, body is None))
    _rx_push(conn, ("tick", vs, [], conn._c.clock.now()))


def _encode_opus(pcm48_stereo: bytes) -> list[bytes]:
    """Fake codec: each 20 ms becomes ``FC 'SBF'`` + 160 µ-law bytes (8 kHz mono)."""
    data = bytes(pcm48_stereo)
    _check_int("pcm48_stereo_bytes", len(data), 0, 60_000 * 192)
    out = []
    for off in range(0, len(data), 3840):
        tick = data[off : off + 3840].ljust(3840, b"\x00")
        a = array.array("h")
        a.frombytes(tick[: len(tick) // 2 * 2])
        if sys.byteorder == "big":
            a.byteswap()
        body = bytes(_ulaw_encode((a[i * 12] + a[i * 12 + 1]) // 2) for i in range(160))
        out.append(_FAKE_OPUS_TAG + body)
    return out


class MixerHarness:
    """Drives a playback's (fake) reader/decoder: one ``pull`` per tick, 960 frames at 48 kHz."""

    def __init__(self, playback: Playback) -> None:
        q = playback._q
        if q.conn.parked.pop(q.id, None) is None:
            raise _errors().TrackError("no parked input for this source (detached connections only; already taken?)")
        self._q = q
        self._ended = False
        self.pulls = 0
        _live("LiveReader", 1)

    def __del__(self) -> None:
        _live("LiveReader", -1)

    @property
    def ended(self) -> bool:
        return self._ended

    @property
    def channels(self) -> int:
        return self._q.ch

    def _paused(self) -> bool:
        for call in reversed(self._q.track.log):
            if call in ("pause", "play"):
                return call == "pause"
        return True

    def pull(self, ticks: int) -> bytes:
        _check_int("ticks", ticks, 0, 3_000)
        q = self._q
        out = array.array("f")
        silence = [0.0] * (960 * q.ch)
        for _ in range(ticks):
            if not self._ended and q.track.ended is not None:
                self._drop()
            if self._ended or self._paused():
                out.extend(silence)
                continue
            if q.should_end():
                self.pulls += 1
                self._drop()
                out.extend(silence)
                continue
            out.extend(q.pull())
            self.pulls += 1
        if sys.byteorder == "big":
            out.byteswap()
        return out.tobytes()

    def _drop(self) -> None:
        if self._ended:
            return
        self._ended = True
        q = self._q
        q.reader_gone = True
        if q.track.ended is None:
            q.track.resolve("stopped" if q.closed == "abort" else "finished")

    def drop_track(self) -> None:
        self._drop()


def _track_log(track: NativeTrack) -> list[str]:
    return list(track._t.log)


def _alloc_count() -> int:
    return 0


_testing = types.ModuleType("discord.ext.songbird.native._testing")
_testing.__dict__.update(
    {
        "Harness": Harness,
        "MixerHarness": MixerHarness,
        "detached_connection": _detached_connection,
        "inject_tick": _inject_tick,
        "inject_map": _inject_map,
        "inject_user_left": _inject_user_left,
        "inject_driver_event": _inject_driver_event,
        "inject_voice_tick_rtp": _inject_voice_tick_rtp,
        "encode_opus": _encode_opus,
        "rtp_wrap": _rtp_wrap,
        "track_log": _track_log,
        "alloc_count": _alloc_count,
        "FAKE": True,
    }
)

#: Marks this module as the fake (``native.FAKE`` does not exist on the real module).
FAKE = True


def install() -> types.ModuleType:
    """Register this module as ``discord.ext.songbird.native`` (and ``._testing``)."""
    mod = sys.modules[__name__]
    sys.modules["discord.ext.songbird.native"] = mod
    sys.modules["discord.ext.songbird.native._testing"] = _testing
    return mod
