"""Sources, track handles and the track queue (§7.9, §7.10, §9.8)."""

from __future__ import annotations

import asyncio
import logging
import math
import os
from collections.abc import AsyncIterable, Iterator
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Literal

from . import native
from ._pump import cancel_tasks
from .errors import ConnectionClosed, LimitExceeded, StreamClosed

if TYPE_CHECKING:
    from ._pump import _Pump

__all__ = [
    "AudioFile",
    "OpusPackets",
    "RawPcm",
    "Source",
    "TrackEnd",
    "TrackHandle",
    "TrackInfo",
    "TrackQueue",
    "load_audio_file",
]

_log = logging.getLogger("discord.ext.songbird")

AudioFile = native.AudioFile
RawPcm = native.RawPcm
OpusPackets = native.OpusPackets
TrackInfo = native.TrackInfo

type Source = native.AudioFile | native.RawPcm | native.OpusPackets
"""Anything ``play()``/``enqueue()`` accepts."""

type EndReason = Literal["finished", "stopped", "error", "disconnected"]

#: Longest ``timeout`` accepted by ``seek()``/``info()`` (1 h).
_MAX_OP_TIMEOUT_S = 3_600.0


async def load_audio_file(path: str | os.PathLike[str], *, max_bytes: int = 256 << 20) -> native.AudioFile:
    """``AudioFile.from_path`` in a worker thread (``asyncio.to_thread``), for use on the event loop.

    Parameters
    ----------
    path : str or os.PathLike
        File to read and probe.
    max_bytes : int, default 256 MiB
        Larger files raise ``LimitExceeded``.

    Raises
    ------
    OSError
        The file cannot be read.
    SourceError
        The content is not a recognised audio format.
    """
    return await asyncio.to_thread(native.AudioFile.from_path, path, max_bytes=max_bytes)


@dataclass(frozen=True, slots=True)
class TrackEnd:
    """How a track ended (``TrackHandle.wait_ended()``).

    Attributes
    ----------
    reason : {"finished", "stopped", "error", "disconnected"}
    error : str or None
        The decode error, for ``reason == "error"``.
    """

    reason: EndReason
    error: str | None = None


def _op_timeout_ms(timeout: float) -> int:
    if not (isinstance(timeout, int | float) and math.isfinite(timeout) and 0 <= timeout <= _MAX_OP_TIMEOUT_S):
        raise LimitExceeded("timeout", timeout, 0.0, _MAX_OP_TIMEOUT_S)
    return round(timeout * 1000)


class TrackHandle:
    """Control of one track: a memory/file source started by ``play()``/``enqueue()``, or the track
    of a live stream (``PlaybackStream.track`` …).

    Handles compare equal (and hash equal) when they refer to the same track.

    Attributes
    ----------
    id : int
        Stable per connection.
    uuid : str or None
        songbird's track uuid (``None`` until a queued entry starts).
    kind : {"file", "pcm", "opus", "playback", "encoded_stream", "opus_stream"}
    """

    __slots__ = ("_native", "_pump", "__weakref__")

    def __init__(self, track: native.NativeTrack, pump: _Pump) -> None:
        self._native = track
        self._pump = pump

    def __repr__(self) -> str:
        state = f"ended={self.end_reason!r}" if self.ended else ("pending" if self._native.pending else "active")
        return f"<TrackHandle id={self.id} kind={self.kind} {state}>"

    @property
    def id(self) -> int:
        return self._native.id

    @property
    def uuid(self) -> str | None:
        return self._native.uuid

    @property
    def kind(self) -> Literal["file", "pcm", "opus", "playback", "encoded_stream", "opus_stream"]:
        return self._native.kind

    @property
    def ended(self) -> bool:
        """Whether the track has ended (on any path)."""
        return self._native.ended

    @property
    def end_reason(self) -> EndReason | None:
        """``"finished"``, ``"stopped"``, ``"error"``, ``"disconnected"`` or ``None``."""
        return self._native.end_reason

    @property
    def pending(self) -> bool:
        """A queue entry that has not started yet."""
        return self._native.pending

    def pause(self) -> None:
        """Pause (file tracks: applied by songbird's live mixer; live streams: held by the stream
        itself until :meth:`resume`, whatever is written or sent meanwhile; pending entries: when
        they start)."""
        self._native.pause()

    def resume(self) -> None:
        """Resume playing."""
        self._native.resume()

    def stop(self) -> None:
        """Stop the track (a pending queue entry is removed); it ends with ``"stopped"``.

        A memory/file track (``play()``, a queue entry) ends at once, also while the voice link is
        down. songbird drops it from its mixer only once the link is up, and holds at most 48
        tracks per connection (``LimitExceeded("resident_tracks")``). A live stream's track ends
        once songbird processes the stop; use the stream's ``close()`` to end the stream itself.
        """
        self._native.stop()

    def set_volume(self, volume: float) -> None:
        """``0.0..=4.0``. File tracks: applied by songbird's live mixer."""
        self._native.set_volume(volume)

    def set_loops(self, count: int | None) -> None:
        """Loop ``count`` more times; ``None`` loops forever. ``TrackError`` for live sources."""
        self._native.set_loops(count)

    async def seek(self, seconds: float, *, timeout: float = 5.0) -> float:
        """Seek and return the new position in seconds.

        Raises
        ------
        LimitExceeded
            ``seconds`` is negative, not finite or above 86 400.
        TrackError
            Live sources and queue entries that have not started.
        TimeoutError
            songbird did not answer within ``timeout`` (the mixer is parked while nothing plays).
        """
        conn = self._pump.conn
        op = conn.start_track_seek(self._native, seconds, _op_timeout_ms(timeout))
        result = await self._pump.wait_op(op)
        return float(result)

    async def info(self, *, timeout: float = 1.0) -> native.TrackInfo:
        """songbird's view of the track.

        Raises
        ------
        TrackError
            A queue entry that has not started.
        TimeoutError
            songbird did not answer within ``timeout`` (the mixer is parked while nothing plays).
        """
        conn = self._pump.conn
        op = conn.start_track_info(self._native, _op_timeout_ms(timeout))
        return await self._pump.wait_op(op)

    async def wait_ended(self) -> TrackEnd:
        """Wait until the track ends — on every termination path (end, error, stop, disconnect)."""
        if not self._native.ended:
            await self._pump.wait_track(self._native)
        reason = self._native.end_reason
        if reason is None:  # the connection closed under us; T8 resolves leftovers as disconnected
            return TrackEnd("disconnected", None)
        return TrackEnd(reason, self._native.end_error)

    def __eq__(self, other: object) -> bool:
        if isinstance(other, TrackHandle):
            return self._native == other._native
        return NotImplemented

    def __hash__(self) -> int:
        return hash(self._native)


class TrackQueue:
    """Sequential queue; only the current entry is handed to songbird.

    Indexing, ``len()`` and iteration cover **pending** entries only (index 0 = next up); the playing
    entry is :attr:`current`. Advancing is not gapless (about 20–60 ms between entries).
    """

    def __init__(self, pump: _Pump) -> None:
        # The queue holds the pump, never the client: no client ↔ queue reference cycle.
        self._pump = pump
        # Lazy encoded-stream entries: native track → feed, fed only once the entry is current.
        self._feeds: dict[native.NativeTrack, Any] = {}
        self._tasks: set[asyncio.Task[Any]] = set()

    def _wrap(self, t: native.NativeTrack) -> TrackHandle:
        return TrackHandle(t, self._pump)

    @property
    def _conn(self) -> native.Connection:
        return self._pump.conn

    def enqueue(
        self, source: Source, *, volume: float = 1.0, loops: int | None = 0, paused: bool = False
    ) -> TrackHandle:
        """Append ``source``.

        Parameters
        ----------
        source : AudioFile, RawPcm or OpusPackets
        volume : float, default 1.0
            ``0.0..=4.0``.
        loops : int or None, default 0
            Extra repetitions; ``None`` loops forever.
        paused : bool, default False
            Start paused when the entry becomes current.

        Raises
        ------
        LimitExceeded
            Beyond 10 000 pending entries.
        """
        return self._wrap(self._conn.enqueue(source, volume, loops, paused))

    def enqueue_stream(
        self,
        feed: AsyncIterable[bytes] | asyncio.StreamReader,
        *,
        volume: float = 1.0,
        loops: int | None = 0,
        paused: bool = False,
        max_buffer_bytes: int = 1 << 20,
    ) -> TrackHandle:
        """Queue a live encoded stream (0.5 ``enqueue(Track(StreamInput(reader)))``).

        Its buffer and decode thread are opened, and ``feed`` is pumped, only when the entry becomes
        current; until then it holds no buffer and no thread.
        """
        t = self._conn.enqueue_stream(volume, loops, paused, max_buffer_bytes)
        self._feeds[t] = feed
        return self._wrap(t)

    @property
    def current(self) -> TrackHandle | None:
        """The playing entry."""
        t = self._conn.queue_current()
        return None if t is None else self._wrap(t)

    def skip(self) -> None:
        """Stop the current entry and advance."""
        self._conn.queue_skip()

    def stop(self) -> None:
        """``clear()`` then ``skip()``: stop the current track and empty the queue."""
        self._conn.queue_stop()
        self._prune()

    def pause(self) -> None:
        """Pause the current entry and stop advancing."""
        self._conn.queue_pause()

    def resume(self) -> None:
        """Resume the current entry and advancing."""
        self._conn.queue_resume()

    def clear(self) -> None:
        """Drop pending entries (they end with ``"stopped"``); the current track keeps playing."""
        self._conn.queue_clear()
        self._prune()

    def dequeue(self, index: int) -> TrackHandle:
        """Remove pending entry ``index`` (it ends with ``"stopped"``).

        Raises
        ------
        IndexError
            ``index`` is out of range.
        """
        t = self._conn.queue_dequeue(index)
        self._feeds.pop(t, None)
        return self._wrap(t)

    def __len__(self) -> int:
        return self._conn.queue_len()

    def __bool__(self) -> bool:
        return len(self) > 0

    def __iter__(self) -> Iterator[TrackHandle]:
        return iter([self._wrap(t) for t in self._conn.queue_snapshot()])

    def __getitem__(self, index: int) -> TrackHandle:
        return self._wrap(self._conn.queue_snapshot()[index])

    # ── lazy encoded streams (driven by the pump on WAKE_TRACKS) ─────────────────────────────────

    def _prune(self) -> None:
        for t in [t for t in self._feeds if t.ended]:
            del self._feeds[t]

    def _on_tracks(self) -> None:
        for t in list(self._feeds):
            if t.ended:
                del self._feeds[t]
                continue
            if t.pending:
                continue
            try:
                sink = self._conn.queue_take_stream(t)
            except ConnectionClosed:
                return
            if sink is None:
                continue
            feed = self._feeds.pop(t)
            from .playback import EncodedStream  # local: playback imports tracks

            stream = EncodedStream(sink, self._pump)
            # Spawned by the pump: it is cancelled at shutdown even if the client was collected.
            task = self._pump.spawn(_feed(stream, feed), name=f"songbird-queue-feed-{t.id}")
            self._tasks.add(task)
            task.add_done_callback(self._feed_done)

    def _feed_done(self, task: asyncio.Task[Any]) -> None:
        self._tasks.discard(task)
        if not task.cancelled() and (exc := task.exception()) is not None:
            _log.error("songbird queue feed %s failed", task.get_name(), exc_info=exc)

    def _close(self) -> None:
        """The connection closed: stop feeding (a feed may wait on its source indefinitely)."""
        self._feeds.clear()
        cancel_tasks(self._tasks)


async def _feed(stream: Any, feed: Any) -> None:
    """Pump a queued stream's feed; ends quietly when the stream or connection closes."""
    try:
        await stream.feed_from(feed)
    except StreamClosed, ConnectionClosed:
        pass
