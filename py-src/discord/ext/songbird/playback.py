"""Realtime output streams (§7, §9.7): ``PlaybackStream``, ``EncodedStream``, ``OpusPacketStream``.

Writes are all-or-nothing. ``*_nowait`` methods are synchronous, safe from any thread and never
block the mixer; the ``async`` variants only wait for space (a cancelled ``write()`` has written
nothing). In a task that reads audio from a network connection, use the ``*_nowait`` methods and
treat :class:`PlaybackFull` as an overrun (§9.10 rule 3).
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterable, Buffer, Callable
from typing import TYPE_CHECKING, Self

from . import native
from .errors import PlaybackFull, StreamClosed
from .tracks import TrackHandle

if TYPE_CHECKING:
    from ._pump import _Pump

__all__ = [
    "ClearResult",
    "EncodedStream",
    "OpusPacketStream",
    "PlaybackStats",
    "PlaybackStream",
    "SegmentProgress",
]

ClearResult = native.ClearResult
PlaybackStats = native.PlaybackStats
SegmentProgress = native.SegmentProgress

#: Extra grace for ``aclose(drain=True)`` beyond the buffered audio, and how long an encoded
#: stream's drain may make no progress before it is closed instead (§7.8).
_DRAIN_GRACE_S = 2.0
#: Pause between retries when the needed space includes inserted gap silence (defensive).
_RETRY_S = 0.02


async def _yield_before_retry(spins: int) -> None:
    """Pause before retrying a write that did not fit although space was reported: yield to the
    loop on the first retry, then wait one tick, so a retry loop can never starve the event loop."""
    await asyncio.sleep(0 if spins <= 1 else _RETRY_S)


async def _wait_drained(
    track: TrackHandle, timeout: float | None, abort: Callable[[], None], progress: Callable[[], int] | None = None
) -> None:
    """Wait for a drain-closed stream's track to end, then return; call ``abort`` instead on
    ``timeout``, or (``timeout=None`` with ``progress``) once ``progress()`` has not changed for
    ``_DRAIN_GRACE_S``. A drain never waits unboundedly: while self-muted, while the voice link is
    down or while paused, nothing plays, and the track would not end."""
    try:
        if timeout is not None or progress is None:
            async with asyncio.timeout(timeout):
                await track.wait_ended()
            return
        last = progress()
        while True:
            try:
                async with asyncio.timeout(_DRAIN_GRACE_S):
                    await track.wait_ended()
                return
            except TimeoutError:
                now = progress()
                if now == last:
                    raise
                last = now
    except TimeoutError:
        abort()


def _nbytes(data: Buffer) -> int:
    """Byte length of a write (0 for objects that are not buffers; the write then raises)."""
    try:
        return memoryview(data).nbytes
    except TypeError:
        return 0


class PlaybackStream:
    """Realtime PCM output, created by ``SongbirdClient.open_playback``.

    Starts paused (nothing is sent until the first write). Underrun plays silence; the track
    auto-pauses after ``idle_ms`` (the speaking indicator turns off) and resumes on the next write.
    While self-muted, playback holds its position instead of being consumed.

    Attributes
    ----------
    playback_id : int
        Identifies this stream in ``VoiceEvent.Playback*``.
    format : AudioFormat
        Format of every write.
    track : TrackHandle
        The underlying track.
    """

    playback_id: int
    format: native.AudioFormat
    track: TrackHandle

    def __init__(self, pb: native.Playback, pump: _Pump, fmt: native.AudioFormat) -> None:
        self._pb = pb
        self._pump = pump
        self.playback_id = pb.playback_id
        self.format = fmt
        self.track = TrackHandle(pb.track, pump)

    def __repr__(self) -> str:
        return (
            f"<PlaybackStream id={self.playback_id} format={self.format!r} "
            f"buffered_ms={self.buffered_ms} closed={self.closed}>"
        )

    # ── positions (ms) ───────────────────────────────────────────────────────────────────────────

    @property
    def played_ms(self) -> int:
        """Real audio handed to the Discord encoder."""
        return self._pb.played_ms

    @property
    def heard_ms(self) -> int:
        """Estimate: ``played_ms`` minus the part of ``output_latency_ms`` not yet elapsed since the
        last real pull (reaches ``played_ms`` once playback has stopped for ``output_latency_ms``)."""
        return self._pb.heard_ms

    @property
    def position_ms(self) -> int:
        """Playhead including silence."""
        return self._pb.position_ms

    @property
    def buffered_ms(self) -> int:
        """Queued audio, including inserted gap silence."""
        return self._pb.buffered_ms

    @property
    def written_ms(self) -> int:
        """Accepted real audio (excludes gap silence and rejected writes)."""
        return self._pb.written_ms

    @property
    def is_idle(self) -> bool:
        """Whether playback is idle: nothing queued and no audio for ``idle_ms``."""
        return self._pb.is_idle

    @property
    def closed(self) -> bool:
        return self._pb.closed

    @property
    def volume(self) -> float:
        """``0.0..=4.0``; changes ramp over 5 ms."""
        return self._pb.volume

    @volume.setter
    def volume(self, value: float) -> None:
        self._pb.volume = value

    # ── writing ──────────────────────────────────────────────────────────────────────────────────

    async def write(
        self,
        data: Buffer,
        *,
        tag: str | None = None,
        at_ms: int | None = None,
        timeout: float | None = None,
    ) -> None:
        """Append raw audio in :attr:`format`, waiting only while the buffer is full.

        Parameters
        ----------
        data : bytes-like
            Any size (a partial frame is carried to the next write). Typed buffers must match the
            encoding (``'h'`` for pcm16le, ``'f'`` for f32le) or be raw bytes. An ``AudioChunk`` can
            be written directly when its format equals :attr:`format`. A ``str`` raises
            ``TypeError``.
        tag : str or None
            Application-defined segment tag (at most 128 bytes) for :meth:`clear`, :meth:`segment`
            and :meth:`end_segment`, e.g. the id of the message or segment the audio belongs to. Use a
            new tag for each segment: a tag cut off by :meth:`clear` stays rejected (see :meth:`clear`).
        at_ms : int or None
            Source-timeline timestamp in ms. A gap since the previous timed write is filled with
            silence (up to ``max_gap_ms``), so audio stays aligned with the source's clock.
        timeout : float or None
            Seconds to wait for space (``TimeoutError``); a timed-out write wrote nothing.

        Raises
        ------
        TypeError
            ``data`` is a ``str`` or not a buffer.
        StreamClosed
            The stream or connection is closed.
        InvalidAudio
            A mismatched buffer format, an ``AudioChunk`` of another format, or a tag longer than
            128 bytes.
        LimitExceeded
            The write can never fit the buffer (checked before anything is copied).
        """
        await self._write(lambda: self._pb.write(data, tag, at_ms), _nbytes(data), timeout)

    def write_nowait(self, data: Buffer, *, tag: str | None = None, at_ms: int | None = None) -> None:
        """Like :meth:`write`, but raises :class:`PlaybackFull` (nothing consumed) instead of waiting.
        Safe from any thread."""
        self._pb.write(data, tag, at_ms)

    async def _write(self, attempt: Callable[[], None], nbytes: int, timeout: float | None) -> None:
        self._pump.check_loop()
        async with asyncio.timeout(timeout):
            spins = 0
            while True:
                try:
                    attempt()
                    return
                except PlaybackFull:
                    pass
                if self._pb.arm_space(nbytes):
                    # Space for `nbytes` exists, yet the write did not fit (it also needs room for
                    # inserted gap silence, or another thread took the space): never spin.
                    spins += 1
                    await _yield_before_retry(spins)
                    continue
                spins = 0
                if self._pump.is_closed:
                    raise StreamClosed("connection_closed")
                await self._pump.wait(self._pb.wake_id)

    # ── interruption and tagged segments ─────────────────────────────────────────────────────────

    def clear(
        self, *, reject_late: bool = True, hold_ms: int = 0, reject_until_gap_ms: int | None = None
    ) -> native.ClearResult:
        """Interrupt: discard all queued audio now (silent within ≤ 40 ms). Any thread.

        Parameters
        ----------
        reject_late : bool, default True
            Drop later writes for every open or interrupted tag (audio of a segment that was cut
            off and is still arriving). The stream remembers the 256 most recently rejected tags
            for its whole lifetime, so never reuse a tag: a later write with a rejected tag is
            dropped silently and only counted in ``stats().rejected_ms``.
        hold_ms : int, default 0
            Fallback for untimed writes: drop **all** writes for this long.
        reject_until_gap_ms : int or None
            Drop later *timed* writes (``at_ms``) contiguous with the cleared audio until one starts
            at least this many ms after it.

        Returns
        -------
        ClearResult
            What was discarded, and per-tag progress (``segments``): how much of each tagged segment
            was actually played before the interruption.
        """
        return self._pb.clear(reject_late=reject_late, hold_ms=hold_ms, reject_until_gap_ms=reject_until_gap_ms)

    def end_segment(self, tag: str) -> None:
        """Mark the tagged segment ``tag`` as fully written: closes the tag and releases the
        prebuffer gate; ``SegmentProgress.complete`` becomes ``True`` once it has played."""
        self._pb.end_segment(tag)

    def segment(self, tag: str) -> native.SegmentProgress | None:
        """Progress of the tagged segment ``tag``, if it is known."""
        return self._pb.segment(tag)

    def played_ms_of(self, tag: str) -> int:
        """``segment(tag).played_ms`` or 0 (clamped to ``written_ms``)."""
        it = self._pb.segment(tag)
        return 0 if it is None else it.played_ms

    # ── waiting ──────────────────────────────────────────────────────────────────────────────────

    async def drain(self, timeout: float | None = None) -> bool:
        """Flush: wait until everything written has been played.

        Returns
        -------
        bool
            ``False`` on timeout or when the stream or connection closes. While the link is down or
            the client is self-muted it keeps waiting (the audio plays afterwards).
        """
        self._pump.check_loop()
        try:
            async with asyncio.timeout(timeout):
                while True:
                    if self._pb.closed:
                        return False
                    if self._pb.arm_drain():
                        return True
                    if self._pump.is_closed:
                        return False
                    await self._pump.wait(self._pb.wake_id)
        except TimeoutError:
            return False

    async def wait_idle(self) -> None:
        """Wait until playback is idle: everything written has played and no new audio arrived for
        ``idle_ms``. Useful when the source has no explicit end-of-segment signal. Returns at once
        if nothing is playing."""
        self._pump.check_loop()
        while not self._pb.closed and not self._pb.arm_idle():
            if self._pump.is_closed:
                return
            await self._pump.wait(self._pb.wake_id)

    # ── control ──────────────────────────────────────────────────────────────────────────────────

    def pause(self) -> None:
        """Hold playback (silence, nothing consumed); RTP stops."""
        self._pb.pause()

    def resume(self) -> None:
        self._pb.resume()

    def stats(self) -> native.PlaybackStats:
        """A snapshot of the stream counters."""
        return self._pb.stats()

    def close(self) -> None:
        """Stop immediately and free the buffer; idempotent, any thread."""
        self._pb.close(False)

    async def aclose(self, *, drain: bool = False, timeout: float | None = None) -> None:
        """Close; with ``drain=True`` first let the buffered audio play (at most ``timeout`` s,
        default ``buffered_ms + 2 s``), then stop."""
        self._pump.check_loop()
        if not drain:
            self.close()
            return
        deadline = timeout if timeout is not None else self._pb.buffered_ms / 1000 + _DRAIN_GRACE_S
        self._pb.close(True)
        try:
            async with asyncio.timeout(deadline):
                while not self._pb.arm_end():
                    if self._pump.is_closed:
                        break
                    await self._pump.wait(self._pb.wake_id)
        except TimeoutError:
            self._pb.close(False)

    async def __aenter__(self) -> Self:
        self._pump.check_loop()
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose(drain=False)


class EncodedStream:
    """Push encoded bytes (mp3/ogg/wav/…, see ``supported_codecs()``); decoded off the mixer thread.

    Created by ``SongbirdClient.open_encoded_stream`` (and by ``TrackQueue.enqueue_stream`` when the
    entry becomes current). The track starts when the first decoded audio is ready.
    """

    track: TrackHandle

    def __init__(self, sink: native.EncodedSink, pump: _Pump) -> None:
        self._sink = sink
        self._pump = pump
        self.track = TrackHandle(sink.track, pump)

    async def write(self, data: Buffer, *, timeout: float | None = None) -> None:
        """Append encoded bytes, waiting while the buffer is full.

        Raises
        ------
        LimitExceeded
            The write is larger than the whole buffer (checked before anything is copied).
        """
        self._pump.check_loop()
        n = memoryview(data).nbytes
        async with asyncio.timeout(timeout):
            spins = 0
            while True:
                try:
                    self._sink.write(data)
                    return
                except PlaybackFull:
                    pass
                if not isinstance(data, bytes):
                    # The sink copies any other buffer on every attempt; it fits the buffer (it was
                    # not LimitExceeded), so copy it once and let every retry borrow it.
                    data = bytes(data)
                if self._sink.arm_space(n):
                    spins += 1  # space was reported but taken (another thread): never spin
                    await _yield_before_retry(spins)
                    continue
                spins = 0
                if self._pump.is_closed:
                    raise StreamClosed("connection_closed")
                await self._pump.wait(self._sink.wake_id)

    def write_nowait(self, data: Buffer) -> None:
        """Append encoded bytes; raises :class:`PlaybackFull` (nothing consumed) when full."""
        self._sink.write(data)

    async def feed_from(
        self,
        src: asyncio.StreamReader | AsyncIterable[bytes],
        *,
        chunk_size: int = 16_384,
        finish: bool = True,
    ) -> int:
        """Pump ``src`` into the stream; returns the number of bytes fed.

        Parameters
        ----------
        src : asyncio.StreamReader or AsyncIterable[bytes]
        chunk_size : int, default 16384
            Largest single write (reads from a ``StreamReader`` use this size).
        finish : bool, default True
            Call :meth:`finish` at the end of ``src``.
        """
        self._pump.check_loop()
        if chunk_size <= 0:
            raise ValueError("chunk_size must be positive")
        total = 0
        if isinstance(src, asyncio.StreamReader):
            while chunk := await src.read(chunk_size):
                await self.write(chunk)
                total += len(chunk)
        else:
            async for chunk in src:
                if isinstance(chunk, bytes) and len(chunk) <= chunk_size:
                    await self.write(chunk)  # borrowed by the sink: no copy before the ring's
                    total += len(chunk)
                    continue
                view = memoryview(chunk)  # split without copying; the sink checks each piece first
                for start in range(0, view.nbytes, chunk_size):
                    piece = view[start : start + chunk_size]
                    await self.write(piece)
                    total += piece.nbytes
        if finish:
            self.finish()
        return total

    def finish(self) -> None:
        """End of input: the track ends after the decoded audio has played."""
        self._sink.finish()

    def close(self) -> None:
        """Stop now; idempotent, any thread."""
        self._sink.close(False)

    async def aclose(self, *, drain: bool = True, timeout: float | None = None) -> None:
        """Close; with ``drain=True`` finish the input and wait for the decoded audio to play, then
        return.

        The wait is bounded: by ``timeout`` seconds if given, otherwise for as long as audio keeps
        playing. Once nothing has played for 2 s (self-muted, the voice link down, the track
        paused) the stream is closed (stopped) instead.
        """
        self._pump.check_loop()
        if not drain:
            self.close()
            return
        self._sink.finish()
        self._sink.close(True)
        await _wait_drained(self.track, timeout, self.close, lambda: self._sink.played_ms)

    async def __aenter__(self) -> Self:
        self._pump.check_loop()
        return self

    async def __aexit__(self, exc_type: type[BaseException] | None, *exc: object) -> None:
        """``aclose()`` (drain, bounded) on a normal exit; ``close()`` at once when the block
        raised or was cancelled, so a cancellation or timeout is not held up by the drain."""
        await self.aclose(drain=exc_type is None)


class OpusPacketStream:
    """Push 20 ms / 48 kHz Opus packets, created by ``SongbirdClient.open_opus_stream``.

    Underrun sends Opus silence frames; after ``idle_ms`` (250) without packets the track pauses (no
    RTP) until the next send. Starts paused. Passthrough (no re-encode) when it is the only track.
    """

    track: TrackHandle

    def __init__(self, sink: native.OpusSink, pump: _Pump) -> None:
        self._sink = sink
        self._pump = pump
        self.track = TrackHandle(sink.track, pump)

    @property
    def buffered_ms(self) -> int:
        return self._sink.buffered_ms

    async def send(self, packet: Buffer, *, timeout: float | None = None) -> None:
        """Queue one packet, waiting while the queue is full."""
        self._pump.check_loop()
        async with asyncio.timeout(timeout):
            spins = 0
            while True:
                try:
                    self._sink.send(packet)
                    return
                except PlaybackFull:
                    pass
                if self._sink.arm_space():
                    spins += 1  # space was reported but taken (another thread): never spin
                    await _yield_before_retry(spins)
                    continue
                spins = 0
                if self._pump.is_closed:
                    raise StreamClosed("connection_closed")
                await self._pump.wait(self._sink.wake_id)

    def send_nowait(self, packet: Buffer) -> None:
        """Queue one packet.

        Raises
        ------
        PlaybackFull
            The queue is full (nothing consumed).
        InvalidAudio
            Not a 1–1275-byte packet of exactly 960 samples at 48 kHz.
        """
        self._sink.send(packet)

    def clear(self) -> int:
        """Discard queued packets; returns the milliseconds discarded."""
        return self._sink.clear()

    def close(self) -> None:
        """Stop now; idempotent, any thread."""
        self._sink.close(False)

    async def aclose(self, *, drain: bool = True, timeout: float | None = None) -> None:
        """Close; with ``drain=True`` wait for the queued packets to play (at most ``timeout`` s,
        default ``buffered_ms + 2 s``), then stop.

        A paused stream with packets left cannot drain and is stopped at once.
        """
        self._pump.check_loop()
        if not drain:
            self.close()
            return
        deadline = timeout if timeout is not None else self._sink.buffered_ms / 1000 + _DRAIN_GRACE_S
        self._sink.close(True)
        await _wait_drained(self.track, deadline, self.close)

    async def __aenter__(self) -> Self:
        self._pump.check_loop()
        return self

    async def __aexit__(self, exc_type: type[BaseException] | None, *exc: object) -> None:
        """``aclose()`` (drain, bounded) on a normal exit; ``close()`` at once when the block
        raised or was cancelled, so a cancellation or timeout is not held up by the drain."""
        await self.aclose(drain=exc_type is None)
