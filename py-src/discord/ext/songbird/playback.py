"""Realtime output streams (§7, §9.7): ``PlaybackStream``, ``EncodedStream``, ``OpusPacketStream``.

Writes are all-or-nothing. ``*_nowait`` methods are synchronous, safe from any thread and never
block the mixer; the ``async`` variants only wait for space (a cancelled ``write()`` has written
nothing). In a task that reads a provider WebSocket, use the ``*_nowait`` methods and treat
:class:`PlaybackFull` as an overrun (§9.10 rule 3).
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
    "ItemProgress",
    "OpusPacketStream",
    "PlaybackStats",
    "PlaybackStream",
]

ClearResult = native.ClearResult
ItemProgress = native.ItemProgress
PlaybackStats = native.PlaybackStats

#: Extra grace for ``aclose(drain=True)`` beyond the buffered audio (§7.8).
_DRAIN_GRACE_S = 2.0
#: Pause between retries when the needed space includes inserted gap silence (defensive).
_RETRY_S = 0.02


async def _yield_before_retry(spins: int) -> None:
    """Pause before retrying a write that did not fit although space was reported: yield to the
    loop on the first retry, then wait one tick, so a retry loop can never starve the event loop."""
    await asyncio.sleep(0 if spins <= 1 else _RETRY_S)


def _nbytes(data: Buffer | str) -> int:
    """Decoded byte length of a write (base64 strings decode to ~3/4 of their length)."""
    if isinstance(data, str):
        return len(data) * 3 // 4
    try:
        return memoryview(data).nbytes
    except TypeError:
        return 0


class PlaybackStream:
    """Realtime PCM output (AI speech), created by ``SongbirdClient.open_playback``.

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
        """Whether the stream is idle (no turn in progress)."""
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
        data: Buffer | str,
        *,
        tag: str | None = None,
        at_ms: int | None = None,
        timeout: float | None = None,
    ) -> None:
        """Append audio in :attr:`format`, waiting only while the buffer is full.

        Parameters
        ----------
        data : bytes-like or str
            Any size (a partial frame is carried to the next write). A ``str`` is base64. Typed
            buffers must match the encoding (``'h'`` for pcm16le, ``'f'`` for f32le) or be raw bytes.
        tag : str or None
            Provider item id (OpenAI Realtime ``item_id``) for :meth:`clear` / :meth:`item`.
        at_ms : int or None
            Provider timestamp (GPT-Live ``start_ms``) for wall-clock gap filling.
        timeout : float or None
            Seconds to wait for space (``TimeoutError``); a timed-out write wrote nothing.

        Raises
        ------
        StreamClosed
            The stream or connection is closed.
        InvalidAudio
            Invalid base64, a mismatched buffer format, or a tag longer than 128 bytes.
        LimitExceeded
            The write can never fit the buffer.
        """
        await self._write(lambda: self._pb.write(data, tag, at_ms), _nbytes(data), timeout)

    def write_nowait(self, data: Buffer | str, *, tag: str | None = None, at_ms: int | None = None) -> None:
        """Like :meth:`write`, but raises :class:`PlaybackFull` (nothing consumed) instead of waiting.
        Safe from any thread."""
        self._pb.write(data, tag, at_ms)

    async def write_base64(
        self,
        data: str | bytes,
        *,
        tag: str | None = None,
        at_ms: int | None = None,
        timeout: float | None = None,
    ) -> None:
        """:meth:`write` of base64 text (``str`` or ASCII ``bytes``), decoded in Rust."""
        n = len(data) * 3 // 4
        await self._write(lambda: self._pb.write_base64(data, tag, at_ms), n, timeout)

    def write_base64_nowait(self, data: str | bytes, *, tag: str | None = None, at_ms: int | None = None) -> None:
        """:meth:`write_nowait` of base64 text, decoded in Rust. Safe from any thread."""
        self._pb.write_base64(data, tag, at_ms)

    async def _write(self, attempt: Callable[[], None], nbytes: int, timeout: float | None) -> None:
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

    # ── barge-in and items ───────────────────────────────────────────────────────────────────────

    def clear(
        self, *, reject_late: bool = True, hold_ms: int = 0, reject_until_gap_ms: int | None = None
    ) -> native.ClearResult:
        """Barge-in: discard all queued audio now (silent within ≤ 40 ms). Any thread.

        Parameters
        ----------
        reject_late : bool, default True
            Drop later writes for every open or interrupted tag (OpenAI Realtime item ids).
        hold_ms : int, default 0
            Fallback for untimed writes: drop **all** writes for this long.
        reject_until_gap_ms : int or None
            Drop later *timed* writes contiguous with the cleared audio until one starts at least
            this many ms after it (GPT-Live ``start_ms``).

        Returns
        -------
        ClearResult
            What was discarded, and per-item progress for truncation (``items``).
        """
        return self._pb.clear(reject_late=reject_late, hold_ms=hold_ms, reject_until_gap_ms=reject_until_gap_ms)

    def end_item(self, tag: str) -> None:
        """The provider finished sending this item's audio (e.g. ``response.output_audio.done``):
        closes the tag and releases the prebuffer gate; ``ItemProgress.complete`` becomes ``True``
        once it has played."""
        self._pb.end_item(tag)

    def item(self, tag: str) -> native.ItemProgress | None:
        """Progress of item ``tag``, if it is known."""
        return self._pb.item(tag)

    def played_ms_of(self, tag: str) -> int:
        """``item(tag).played_ms`` or 0 (clamped to ``written_ms``)."""
        it = self._pb.item(tag)
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
        """Wait until the stream is idle: the end of the current AI turn (GPT-Live has no
        output-done event). Returns at once if nothing is playing."""
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
        """Append encoded bytes, waiting while the buffer is full."""
        n = memoryview(data).nbytes
        async with asyncio.timeout(timeout):
            spins = 0
            while True:
                try:
                    self._sink.write(data)
                    return
                except PlaybackFull:
                    pass
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
        if chunk_size <= 0:
            raise ValueError("chunk_size must be positive")
        total = 0
        if isinstance(src, asyncio.StreamReader):
            while chunk := await src.read(chunk_size):
                await self.write(chunk)
                total += len(chunk)
        else:
            async for chunk in src:
                view = memoryview(chunk)
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
        """Close; with ``drain=True`` finish the input and wait (at most ``timeout`` s) for the
        decoded audio to play."""
        if not drain:
            self.close()
            return
        self._sink.finish()
        self._sink.close(True)
        try:
            async with asyncio.timeout(timeout):
                await self.track.wait_ended()
        except TimeoutError:
            self.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()


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
        """Close; with ``drain=True`` wait (at most ``timeout`` s) for the queued packets to play."""
        if not drain:
            self.close()
            return
        self._sink.close(True)
        try:
            async with asyncio.timeout(timeout):
                await self.track.wait_ended()
        except TimeoutError:
            self.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()
