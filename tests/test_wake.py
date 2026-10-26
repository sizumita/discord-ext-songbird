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
    _rsock, wsock = socket.socketpair()
    bell = native.WakeBell(wsock.detach())
    conn, _h = testing.detached_connection(1, 2, settings(), bell)
    with pytest.raises(ValueError):
        native.Connection(bell, 1, 2, settings(), True)
    conn.close("requested")


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
