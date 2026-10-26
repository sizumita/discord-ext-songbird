"""Leak tests (§13.4): weakrefs, the pump's weakref callback and ``finally``, and — on the real
module — ``_debug_live_counts()`` returning to baseline with a real songbird Driver."""

from __future__ import annotations

import asyncio
import gc
import socket
import threading
import time
import weakref
from typing import Any

import pytest
from conftest import (
    GUILD_ID,
    FakeClient,
    FakeGuild,
    HarnessClient,
    World,
    connect_detached,
    inject_ticks,
    run_isolated,
)
from discord.ext import songbird
from discord.ext.songbird import ConnectFailed, OpusPackets, RawPcm, SongbirdClient, native

pytestmark = pytest.mark.usefixtures("testing")


def collect() -> None:
    for _ in range(3):
        gc.collect()


async def test_everything_is_collected_after_disconnect(world: World, testing: Any) -> None:
    vc = await connect_detached(world)
    stream = vc.listen_mixed()
    events = vc.events()
    pb = vc.open_playback()
    track = vc.play(OpusPackets([b"\xf8\xff\xfe"]))
    inject_ticks(vc, testing, 2, voiced=False)
    chunk = stream.read_nowait()
    refs = [weakref.ref(o) for o in (vc, vc._conn, vc._pump, stream, events, pb, track, chunk)]
    await vc.disconnect()
    del vc, stream, events, pb, track, chunk
    collect()
    alive = [r() for r in refs if r() is not None]
    assert not alive, alive


async def test_abandoned_client_closes_its_connection(world: World) -> None:
    vc = await connect_detached(world)
    conn = vc._conn
    pump = vc._pump
    assert conn is not None and pump is not None
    world.client._connection._voice_clients.clear()  # simulate an app that drops every reference
    ref = weakref.ref(vc)
    del vc
    collect()
    assert ref() is None
    assert conn.closed  # the pump's weakref callback closed it ("dropped")
    assert conn.status().close_reason == "dropped"
    assert await pump.closed(timeout=2)


async def test_unclosed_stream_is_unregistered(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    base = vc.stats()["subscriptions"]
    stream = vc.listen_mixed()
    inject_ticks(vc, testing, 1, voiced=False)
    assert vc.stats()["subscriptions"] == base + 1
    del stream
    collect()
    inject_ticks(vc, testing, 1, voiced=False)
    assert vc.stats()["subscriptions"] == base
    # The budget was released too: a stream as large as the whole budget fits again.
    big = vc.listen_mixed(songbird.PCM16_48K, chunk_ms=1_000, max_queue_ms=60_000)
    big.close()


def test_closed_loop_tears_the_connection_down() -> None:
    result: dict[str, Any] = {}

    async def scenario() -> None:
        client = FakeClient()
        guild = FakeGuild(client)
        world = World(client, guild, guild.add_channel(10), guild.add_channel(11))
        vc = await connect_detached(world)
        result["conn"] = vc._conn
        result["vc"] = weakref.ref(vc)
        # return without disconnect(): asyncio.run() cancels the pump, whose finally closes

    t = threading.Thread(target=lambda: asyncio.run(scenario()))
    t.start()
    t.join(10)
    conn = result["conn"]
    assert conn.closed
    assert conn.status().close_reason in ("loop_closed", "cleanup")


async def test_user_built_cycle_is_collected(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    stream: Any = vc.listen_mixed()

    def make_cycle(obj: Any) -> None:
        obj.callback = lambda: obj  # obj → callback → obj

    make_cycle(stream)
    ref = weakref.ref(stream)
    del stream
    collect()
    assert ref() is None


# ── real Driver lifecycle (§13.4 test 1) ──────────────────────────────────────────────────────────


def _counts() -> dict[str, int]:
    return {k: v for k, v in native._debug_live_counts().items() if v is not None}


@pytest.mark.real_only
async def test_full_lifecycle_returns_to_baseline() -> None:
    """Five cycles with a real Driver stuck connecting to a listener that never answers."""
    baseline = _counts()
    for _ in range(5):
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.setblocking(False)
        port = listener.getsockname()[1]
        accepted: list[socket.socket] = []

        client = FakeClient()
        guild = FakeGuild(client)
        guild.endpoint = f"127.0.0.1:{port}"
        channel = guild.add_channel(20)
        cfg = songbird.VoiceConfig(driver_timeout_s=60.0)
        task = asyncio.create_task(channel.connect(cls=SongbirdClient.with_config(cfg), timeout=60, reconnect=False))
        async with asyncio.timeout(10):
            while (vc := client._connection._get_voice_client(GUILD_ID)) is None or vc._conn is None:
                await asyncio.sleep(0.005)
            while vc.status.state != "connecting":
                await asyncio.sleep(0.005)
        try:
            accepted.append(listener.accept()[0])
        except BlockingIOError:
            pass
        stream = vc.listen_mixed()
        pb = vc.open_playback()
        events = vc.events()
        track = vc.play(RawPcm(bytes(4_800), songbird.PCM16_24K))
        refs = [weakref.ref(o) for o in (vc, vc._conn, stream, pb, events, track)]
        await vc.disconnect()
        with pytest.raises(ConnectFailed) as ei:
            await task
        assert ei.value.reason == "requested"
        del ei  # its traceback holds connect()'s frames, and with them the client
        assert [op.channel_id for op in guild.op4].count(None) == 1
        for s in accepted:
            s.close()
        listener.close()
        del vc, stream, pb, events, track, task
        collect()
        assert all(r() is None for r in refs)
    deadline = time.monotonic() + 61
    while time.monotonic() < deadline:
        now = _counts()
        if all(now.get(k, 0) <= v for k, v in baseline.items()):
            break
        await asyncio.sleep(0.1)
    now = _counts()
    assert all(now.get(k, 0) <= v for k, v in baseline.items()), (baseline, now)


@pytest.mark.real_only
def test_repeated_cycles_do_not_grow_rss() -> None:
    proc = run_isolated(
        """
        import asyncio, resource, sys
        from conftest import FakeClient, FakeGuild, World, connect_detached

        async def cycle():
            client = FakeClient(); guild = FakeGuild(client)
            world = World(client, guild, guild.add_channel(10), guild.add_channel(11))
            vc = await connect_detached(world)
            s = vc.listen_mixed(); p = vc.open_playback(); s.close(); p.close()
            await vc.disconnect()

        async def main():
            for _ in range(10):
                await cycle()
            base = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
            for _ in range(100):
                await cycle()
            grow = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss - base
            scale = 1 if sys.platform == "darwin" else 1024
            print("GROWTH", grow * scale)

        asyncio.run(main())
        """,
        timeout=300,
    )
    assert proc.returncode == 0, proc.stderr
    growth = int(proc.stdout.split("GROWTH")[1])
    assert growth < 5 * 1024 * 1024


async def test_track_queue_holds_no_reference_to_the_client(world: World) -> None:
    vc = await connect_detached(world)
    queue = vc.queue
    queue_ref = weakref.ref(queue)
    ref = weakref.ref(vc)
    await vc.disconnect()
    gc.disable()  # reference counting alone must free the client: no client ↔ queue cycle
    try:
        del vc
        assert ref() is None
    finally:
        gc.enable()
    del queue
    collect()
    assert queue_ref() is None
