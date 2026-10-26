"""Receive streams and event streams (§8.9–§8.11, §9.5, §9.6).

Every wait follows the same cancel-safe pattern: pop (or arm and get ``None``) synchronously, then
await the stream's wake id on the connection's pump and re-poll. A cancelled ``__anext__`` therefore
never loses or duplicates a chunk.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator, Iterable
from typing import TYPE_CHECKING, Any, Literal, Self

from . import native
from .errors import StreamClosed, StreamOverflow

if TYPE_CHECKING:
    from ._pump import _Pump

__all__ = [
    "AudioChunk",
    "AudioStream",
    "ChunkBatch",
    "EventStream",
    "MixPolicy",
    "Overflow",
    "StreamStats",
    "UserStreams",
    "VoiceEvent",
]

AudioChunk = native.AudioChunk
StreamStats = native.StreamStats
ChunkBatch = native.ChunkBatch
VoiceEvent = native.VoiceEvent

type Overflow = Literal["drop_oldest", "drop_newest", "error"]
type MixPolicy = Literal["mix", "floor"]

#: ``VoiceEvent`` variants in declaration order: bit ``i`` of a native kind mask is ``_EVENT_KINDS[i]``.
_EVENT_KINDS: tuple[type[Any], ...] = (
    VoiceEvent.Connected,
    VoiceEvent.Reconnected,
    VoiceEvent.Disconnected,
    VoiceEvent.UserMapped,
    VoiceEvent.UserLeft,
    VoiceEvent.SpeakingStarted,
    VoiceEvent.SpeakingStopped,
    VoiceEvent.PlaybackStarted,
    VoiceEvent.PlaybackIdle,
    VoiceEvent.PlaybackEnded,
    VoiceEvent.TrackEnded,
    VoiceEvent.ReceiveStalled,
    VoiceEvent.Lagged,
)
ALL_EVENT_KINDS = (1 << len(_EVENT_KINDS)) - 1


def kind_mask(kinds: Iterable[type[Any]] | None) -> int:
    """The native kind mask of ``kinds`` (``VoiceEvent`` variant classes); ``None`` = every kind."""
    if kinds is None:
        return ALL_EVENT_KINDS
    mask = 0
    for k in kinds:
        try:
            mask |= 1 << _EVENT_KINDS.index(k)
        except ValueError:
            raise TypeError(f"{k!r} is not a VoiceEvent variant (e.g. VoiceEvent.SpeakingStarted)") from None
    return mask


class AudioStream(AsyncIterator[AudioChunk]):
    """Bounded, continuous, cancel-safe receive stream: one chunk per ``chunk_ms``, whether or not
    anyone speaks.

    Ends (``StopAsyncIteration``) after a close by the connection, the user's leave or the router,
    once the queued chunks are drained; :meth:`close` ends it at once and discards what is still
    queued. Under ``overflow="error"``, ``__anext__`` raises :class:`StreamOverflow` once after the
    queued chunks, then stops.

    Created by ``SongbirdClient.listen*``; never instantiate it directly.

    Attributes
    ----------
    format : AudioFormat
        Chunk format.
    chunk_ms : int
        Chunk duration.
    user_id : int or None
        The user of a per-user stream; ``None`` for a mixed stream.
    kind : {"user", "mixed"}
    """

    format: native.AudioFormat
    chunk_ms: int
    user_id: int | None
    kind: Literal["user", "mixed"]

    def __init__(
        self,
        sub: native.ReceiveSubscription,
        pump: _Pump,
        *,
        kind: Literal["user", "mixed"],
        mix_filter: dict[str, Any] | None = None,
    ) -> None:
        self._sub = sub
        self._pump = pump
        self.format = sub.format
        self.chunk_ms = sub.chunk_ms
        self.user_id = sub.user_id
        self.kind = kind
        self._filter = dict(mix_filter) if mix_filter is not None else None

    def __repr__(self) -> str:
        who = f"user_id={self.user_id}" if self.kind == "user" else "mixed"
        return f"<AudioStream {who} format={self.format!r} chunk_ms={self.chunk_ms} closed={self.closed}>"

    # ── state ────────────────────────────────────────────────────────────────────────────────────

    @property
    def closed(self) -> bool:
        """Whether the stream is closed (after a close by the connection, queued chunks may still
        be readable)."""
        return self._sub.closed

    @property
    def close_reason(self) -> str | None:
        """``'closed'``, ``'connection_closed'``, ``'user_left'``, ``'overflow'`` or ``'internal_error'``."""
        return self._sub.close_reason

    @property
    def queued_ms(self) -> int:
        """Audio queued and not yet read, in ms."""
        return self._sub.stats().queued_ms

    @property
    def dropped_ms(self) -> int:
        """Total audio dropped by the overflow policy, in ms."""
        return self._sub.stats().dropped_ms

    @property
    def origin_timeline_ms(self) -> int | None:
        """``timeline_ms`` of this stream's clock origin (``start_ms == 0``); ``None`` until the first
        chunk after a reset."""
        return self._sub.origin_timeline_ms

    def stats(self) -> native.StreamStats:
        """A snapshot of the stream counters."""
        return self._sub.stats()

    # ── reading ──────────────────────────────────────────────────────────────────────────────────

    def __aiter__(self) -> Self:
        return self

    async def __anext__(self) -> native.AudioChunk:
        """The next chunk. Pops only after wake-up, synchronously: cancellation loses nothing.

        Raises
        ------
        StreamOverflow
            Once, under ``overflow="error"``, after the queued chunks.
        StopAsyncIteration
            Once the stream is closed and drained.
        """
        try:
            return await self._next()
        except StreamOverflow:
            raise
        except StreamClosed:
            raise StopAsyncIteration from None

    async def read(self, timeout: float | None = None) -> native.AudioChunk:
        """The next chunk.

        Parameters
        ----------
        timeout : float or None
            Seconds to wait; ``None`` waits indefinitely.

        Raises
        ------
        StreamClosed
            The stream is closed and drained.
        StreamOverflow
            Once, under ``overflow="error"``, after the queued chunks.
        TimeoutError
            No chunk arrived within ``timeout``.
        """
        async with asyncio.timeout(timeout):
            return await self._next()

    async def _next(self) -> native.AudioChunk:
        self._pump.check_loop()
        while True:
            chunk = self._sub.try_recv()
            if chunk is not None:
                return chunk
            if self._pump.is_closed:
                # The connection is gone; every subscription was closed at teardown (T6).
                chunk = self._sub.try_recv()
                if chunk is not None:
                    return chunk
                raise StreamClosed(self._sub.close_reason or "connection_closed")
            await self._pump.wait(self._sub.wake_id)

    def read_nowait(self) -> native.AudioChunk | None:
        """The next chunk without waiting (any thread); ``None`` if none is queued.

        Raises
        ------
        StreamClosed
            The stream is closed and drained.
        StreamOverflow
            Once, under ``overflow="error"``, after the queued chunks.
        """
        return self._sub.read_nowait()

    def skip_to_live(self, *, reset_clock: bool = True) -> int:
        """Drop everything queued (e.g. after connecting to a downstream consumer) and return the
        milliseconds dropped.

        Parameters
        ----------
        reset_clock : bool, default True
            The next chunk has ``start_ms == 0`` and ``seq == 0``, so the stream clock starts at the
            first audio the consumer receives.
        """
        return self._sub.skip_to_live(reset_clock)

    def read_batch(self, max_chunks: int = 50) -> native.ChunkBatch | None:
        """``[arrow]`` Drain up to ``max_chunks`` queued chunks into one record batch (``None`` if
        nothing is queued)."""
        return self._sub.read_batch(max_chunks)

    # ── mixed-stream filters ─────────────────────────────────────────────────────────────────────

    def _set_filter(self, **changes: Any) -> None:
        if self.kind != "mixed" or self._filter is None:
            raise ValueError("set_include()/set_exclude() apply to mixed streams only")
        f = {**self._filter, **changes}
        self._sub.set_filter(
            f["include"],
            f["exclude"],
            f["ignore_bots"],
            f["policy"],
            f["floor_release_ms"],
        )
        self._filter = f

    def set_include(self, users: Iterable[Any] | None) -> None:
        """Mix only ``users`` (``None`` = everyone). Applied at the next chunk boundary, with no gap
        or duplicate. Mixed streams only (``ValueError`` otherwise)."""
        self._set_filter(include=None if users is None else user_ids(users))

    def set_exclude(self, users: Iterable[Any]) -> None:
        """Never mix ``users``. Mixed streams only (``ValueError`` otherwise)."""
        self._set_filter(exclude=user_ids(users))

    # ── closing ──────────────────────────────────────────────────────────────────────────────────

    def close(self) -> None:
        """Idempotent and safe from any thread; unregisters at the next tick and frees queued chunks,
        so iteration ends at once (read until :meth:`read_nowait` returns ``None`` first to keep
        them). A stream already closed by the connection keeps its queued chunks readable."""
        self._sub.close()

    async def aclose(self) -> None:
        """:meth:`close`."""
        self._pump.check_loop()
        self.close()

    async def __aenter__(self) -> Self:
        self._pump.check_loop()
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()


class UserStreams(AsyncIterator[AudioStream]):
    """Yields one :class:`AudioStream` per user as they first speak.

    With ``close_on_leave=True`` each stream ends when its user leaves (``UserLeft``); a user who
    returns gets a new stream. Created by ``SongbirdClient.listen_users``.
    """

    def __init__(self, router: native.RouterSubscription, pump: _Pump, register: Any = None) -> None:
        self._router = router
        self._pump = pump
        self._register = register

    @property
    def dropped(self) -> int:
        """Users who got no stream: refused because the connection's 64-stream cap (shared with
        explicit ``listen_*`` streams) or its ``max_receive_mb`` budget was used up (each logged as
        a warning; the user is not retried until they leave and return), or discarded because 64
        new streams waited unread in this router."""
        return self._router.dropped

    def __aiter__(self) -> Self:
        return self

    async def __anext__(self) -> AudioStream:
        self._pump.check_loop()
        while True:
            try:
                sub = self._router.try_next()
            except StreamClosed:
                raise StopAsyncIteration from None
            if sub is not None:
                stream = AudioStream(sub, self._pump, kind="user")
                if self._register is not None:
                    self._register(stream)
                return stream
            if self._pump.is_closed:
                raise StopAsyncIteration
            await self._pump.wait(self._router.wake_id)

    def close(self, *, close_streams: bool = True) -> None:
        """Close the router; by default also every stream it created. Idempotent, any thread."""
        self._router.close(close_streams)

    async def aclose(self, *, close_streams: bool = True) -> None:
        """:meth:`close`."""
        self._pump.check_loop()
        self.close(close_streams=close_streams)

    async def __aenter__(self) -> Self:
        self._pump.check_loop()
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()


class EventStream(AsyncIterator[Any]):
    """Bounded (drop-oldest) voice event queue; a ``VoiceEvent.Lagged`` reports drops.

    Ends after ``Disconnected(will_retry=False)``, which is always the last event, or at once after
    :meth:`close`. Created by ``SongbirdClient.events``.
    """

    def __init__(self, sub: native.EventSubscription, pump: _Pump) -> None:
        self._sub = sub
        self._pump = pump

    @property
    def dropped(self) -> int:
        """Events dropped by the overflow policy."""
        return self._sub.dropped

    def __aiter__(self) -> Self:
        return self

    async def __anext__(self) -> Any:
        self._pump.check_loop()
        while True:
            try:
                ev = self._sub.try_recv()
            except StreamClosed:
                raise StopAsyncIteration from None
            if ev is not None:
                return ev
            if self._pump.is_closed:
                try:
                    ev = self._sub.try_recv()
                except StreamClosed:
                    raise StopAsyncIteration from None
                if ev is not None:
                    return ev
                raise StopAsyncIteration
            await self._pump.wait(self._sub.wake_id)

    def read_nowait(self) -> Any:
        """The next event without waiting (any thread); ``None`` if none is queued.

        Raises
        ------
        StreamClosed
            The stream is closed and drained.
        """
        return self._sub.read_nowait()

    def close(self) -> None:
        """Idempotent and safe from any thread; frees the queued events, so iteration ends at once.
        A stream the connection already closed keeps its events, ending with ``Disconnected``."""
        self._sub.close()

    async def aclose(self) -> None:
        """:meth:`close`."""
        self._pump.check_loop()
        self.close()

    async def __aenter__(self) -> Self:
        self._pump.check_loop()
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()


def user_id(user: Any) -> int:
    """The id of a ``UserLike`` (an ``int`` or anything with an integer ``id``)."""
    if isinstance(user, bool):
        raise TypeError("a user id must be an int or a discord.abc.Snowflake")
    if isinstance(user, int):
        return user
    uid = getattr(user, "id", None)
    if isinstance(uid, int) and not isinstance(uid, bool):
        return uid
    raise TypeError(f"expected a user id or a discord.abc.Snowflake, got {type(user).__name__}")


def user_ids(users: Iterable[Any]) -> list[int]:
    """Ids of ``UserLike`` values (deduplicated, order kept)."""
    seen: dict[int, None] = {}
    for u in users:
        seen[user_id(u)] = None
    return list(seen)
