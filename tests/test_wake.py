"""The Rust → Python doorbell (§5.4, §13.3 ``test_wake.py``). Runs on every platform's default loop
(Windows: Proactor)."""

from __future__ import annotations

import asyncio
import gc
import socket
import tempfile
from typing import Any

import pytest
from discord.ext import songbird
from discord.ext.songbird import native
from discord.ext.songbird.streams import ALL_EVENT_KINDS


def settings() -> Any:
    return songbird.VoiceConfig().to_native()


async def read_until_eof(rsock: socket.socket) -> bytes:
    loop = asyncio.get_running_loop()
    out = b""
    async with asyncio.timeout(2):
        while chunk := await loop.sock_recv(rsock, 256):
            out += chunk
    return out


async def test_doorbell_end_to_end(testing: Any) -> None:
    loop = asyncio.get_running_loop()
    rsock, wsock = socket.socketpair()
    rsock.setblocking(False)
    bell = native.WakeBell(wsock.detach())
    assert not bell.taken
    conn, harness = testing.detached_connection(1, 2, settings(), bell)
    assert bell.taken
    sub = conn.subscribe_events(16, ALL_EVENT_KINDS)
    assert sub.try_recv() is None  # observed empty: armed
    testing.inject_map(conn, 5, 6)
    harness.step(20)
    data = await asyncio.wait_for(loop.sock_recv(rsock, 256), 2)
    assert len(data) >= 1
    assert sub.wake_id in conn.take_ready()
    ev = sub.try_recv()
    assert isinstance(ev, songbird.VoiceEvent.UserMapped) and (ev.user_id, ev.ssrc) == (6, 5)
    # Nothing armed, nothing pending: no new byte.
    with pytest.raises(BlockingIOError):
        rsock.recv(1)
    conn.close("requested")
    await read_until_eof(rsock)  # the remaining bytes, then b"" (SHUT_WR)
    assert native.WAKE_CONN in conn.take_ready() or conn.closed
    rsock.close()


async def test_bytes_coalesce_per_burst(testing: Any) -> None:
    rsock, wsock = socket.socketpair()
    rsock.setblocking(False)
    conn, harness = testing.detached_connection(1, 2, settings(), native.WakeBell(wsock.detach()))
    subs = [conn.subscribe_events(16, ALL_EVENT_KINDS) for _ in range(8)]
    for s in subs:
        assert s.try_recv() is None
    testing.inject_map(conn, 5, 6)
    harness.step(20)
    await asyncio.sleep(0.01)
    assert len(rsock.recv(256)) == 1  # one byte per notification burst
    ready = set(conn.take_ready())
    assert {s.wake_id for s in subs} <= ready
    conn.close("requested")
    rsock.close()


async def test_failed_connection_closes_the_fd_once() -> None:
    rsock, wsock = socket.socketpair()
    fd = wsock.detach()
    bell = native.WakeBell(fd)
    with pytest.raises(ValueError):
        native.Connection(bell, 0, 1, settings(), True)  # zero id: rejected before taking the bell
    assert not bell.taken
    del bell
    gc.collect()
    rsock.settimeout(2)
    assert rsock.recv(1) == b""  # the write end was closed (exactly once: no EBADF anywhere)
    rsock.close()


async def test_taken_bell_cannot_be_reused(testing: Any) -> None:
    rsock, wsock = socket.socketpair()
    bell = native.WakeBell(wsock.detach())
    conn, _h = testing.detached_connection(1, 2, settings(), bell)
    with pytest.raises(ValueError):
        native.Connection(bell, 1, 2, settings(), True)
    conn.close("requested")
    rsock.close()


def test_wakebell_rejects_non_sockets() -> None:
    with pytest.raises(OSError):
        native.WakeBell(-1)
    with tempfile.TemporaryFile() as f:
        import os

        fd = os.dup(f.fileno())
        with pytest.raises(OSError):
            native.WakeBell(fd)


async def test_pump_wakes_waiters(connected_vc: Any, testing: Any) -> None:
    vc = connected_vc
    events = vc.events()
    waiter = asyncio.create_task(anext(events))
    await asyncio.sleep(0.01)
    assert not waiter.done()
    testing.inject_map(vc._conn, 77, 88)
    vc.harness.step(20)
    ev = await asyncio.wait_for(waiter, 2)
    assert isinstance(ev, songbird.VoiceEvent.UserMapped)
    events.close()


async def test_waiting_from_another_loop_raises(connected_vc: Any) -> None:
    import threading

    vc = connected_vc
    stream = vc.listen_mixed()
    errors: list[BaseException] = []

    def other_loop() -> None:
        try:
            asyncio.run(stream.read(timeout=0.5))
        except BaseException as e:
            errors.append(e)

    t = threading.Thread(target=other_loop)
    t.start()
    t.join(5)
    assert errors and isinstance(errors[0], RuntimeError)
    stream.close()


async def test_another_loop_raises_even_when_no_wait_is_needed(connected_vc: Any, testing: Any) -> None:
    """The loop check comes before every fast path: a foreign-loop await fails deterministically
    instead of only under backpressure, and it consumes, writes or seeks nothing."""
    import threading

    from conftest import inject_ticks

    vc = connected_vc
    stream = vc.listen_mixed(songbird.PCM16_24K)
    events = vc.events()
    pb = vc.open_playback(songbird.PCM16_24K)
    opus = vc.open_opus_stream()
    encoded = vc.open_encoded_stream()
    track = vc.play(songbird.OpusPackets([b"\xf8\xff\xfe"] * 10))
    ended = vc.play(songbird.OpusPackets([b"\xf8\xff\xfe"]))
    ended.stop()
    testing.inject_map(vc._conn, 101, 7_000_000_000_000_001)  # a UserMapped event
    inject_ticks(vc, testing, 2, voiced=False)
    assert stream.queued_ms == 40
    calls: dict[str, Any] = {
        "read": lambda: stream.read(),
        "anext": lambda: anext(stream),
        "events": lambda: anext(events),
        "write": lambda: pb.write(bytes(960)),
        "drain": lambda: pb.drain(),
        "wait_idle": lambda: pb.wait_idle(),
        "opus_send": lambda: opus.send(b"\xf8\xff\xfe"),
        "encoded_write": lambda: encoded.write(b"x"),
        "seek": lambda: track.seek(0.5),
        "info": lambda: track.info(),
        "wait_ended": lambda: ended.wait_ended(),
    }
    errors: dict[str, BaseException | None] = {}

    def other_loop() -> None:
        for name, make in calls.items():
            try:
                asyncio.run(make())
                errors[name] = None
            except BaseException as e:
                errors[name] = e

    t = threading.Thread(target=other_loop)
    t.start()
    t.join(10)
    assert {k: type(e).__name__ for k, e in errors.items()} == dict.fromkeys(calls, "RuntimeError")
    assert stream.queued_ms == 40, "nothing was read"
    assert events.read_nowait() is not None, "no event was consumed"
    assert pb.written_ms == 0 and opus.buffered_ms == 0
    assert "seek" not in testing.track_log(track._native), "a foreign-loop seek() must not seek"
    for s in (stream, events, pb, opus, encoded):
        s.close()
