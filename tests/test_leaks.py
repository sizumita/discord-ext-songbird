"""Leak tests (§13.4): weakrefs, the pump's weakref callback and ``finally``, 100 connect/close
cycles through the test hooks with every ``_debug_live_counts()`` entry back to baseline, and — on
the real module — the same with a real songbird Driver."""

from __future__ import annotations

import asyncio
import gc
import io
import logging
import socket
import threading
import time
import wave
import weakref
from collections.abc import AsyncIterator
from typing import Any

import pytest
from conftest import (
    CHANNEL_ID,
    FAKE,
    GUILD_ID,
    OTHER_CHANNEL_ID,
    FakeClient,
    FakeGuild,
    HarnessClient,
    World,
    connect_detached,
    flush_native_logs,
    inject_ticks,
    pump_settle,
    run_isolated,
    tone,
    voice_packets,
    wait_for,
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


#: Tasks alive on the native runtimes. Only these see a songbird Driver that was never Poisoned:
#: ``OwnedDriver`` drops when the teardown returns, ``Dispatch`` clones go with the global-event
#: removal, and a parked mixer is not a live one (§11.3).
TASK_COUNTS = ("net_alive_tasks", "rx_alive_tasks")


def _settled(baseline: dict[str, int], now: dict[str, int]) -> bool:
    return all(now.get(k, 0) <= v for k, v in baseline.items())


async def _real_driver_cycle() -> dict[str, int]:
    """One SongbirdClient with a real Driver stuck connecting to a listener that never answers,
    every kind of stream and a track open, then disconnected. Returns the counts while open."""
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
    during = _counts()
    refs = [weakref.ref(o) for o in (vc, vc._conn, stream, pb, events, track)]
    await vc.disconnect()
    with pytest.raises(ConnectFailed) as ei:
        await task
    assert ei.value.reason == "requested"
    del ei  # its traceback holds connect()'s frames, and with them the client
    assert [op.channel_id for op in guild.op4].count(None) == 1
    # Poison waits behind the pending connection attempt: end it now rather than after 60 s.
    for s in accepted:
        s.close()
    listener.close()
    del vc, stream, pb, events, track, task
    collect()
    assert all(r() is None for r in refs)
    return during


async def _wait_settled(baseline: dict[str, int], timeout: float) -> dict[str, int]:
    deadline = time.monotonic() + timeout
    while not _settled(baseline, now := _counts()) and time.monotonic() < deadline:
        await asyncio.sleep(0.1)
    return now


@pytest.mark.real_only
async def test_full_lifecycle_returns_to_baseline() -> None:
    """Five cycles with a real Driver stuck connecting: every native counter and every task of the
    NET and RX runtimes (songbird's core, events and parked-mixer tasks among them) returns to its
    baseline, so a Driver that is never Poisoned cannot pass."""
    # Warm-up: songbird keeps one process-wide task once its first Driver has run, and process-wide
    # helper tasks may start lazily. Both belong in the baseline.
    before = _counts()
    await _real_driver_cycle()
    await _wait_settled({k: v for k, v in before.items() if k not in TASK_COUNTS}, 61)
    await asyncio.sleep(0.5)
    baseline = _counts()
    for _ in range(5):
        during = await _real_driver_cycle()
        # Not vacuous: the cycle really ran a Driver with its songbird tasks.
        assert during["OwnedDriver"] > baseline["OwnedDriver"], (baseline, during)
        assert during["net_alive_tasks"] >= baseline["net_alive_tasks"] + 3, (baseline, during)
    now = await _wait_settled(baseline, 61)
    assert _settled(baseline, now), (baseline, now)


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


# ── 100 connect/close cycles: the 0.5 leak reproduction against the 0.6 API (§15.1 defect 1) ─────

SPEAKER, SPEAKER_SSRC = 7_000_000_000_000_001, 101
#: How a cycle ends, in turn: the user's leave, a forced one, being removed from the channel, a
#: terminal loss with ``reconnect=False`` (best-effort leave), and an app that drops the client
#: without disconnecting.
CLOSE_PATHS = ("disconnect", "force", "left_channel", "lost", "abandoned")
#: Counters every cycle must raise, so the baseline check cannot pass vacuously.
EXERCISED = ("ConnShared", "SubQueue", "EventQueue", "PcmQueue", "TrackFlags", "LiveReader")
EXERCISED_REAL = ("ConnWorker", "Dispatch", "SsrcState", "DecodeThread", "SongbirdInput", "DoneGuard")


def _wav(ms: int) -> bytes:
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(48_000)
        w.writeframes(tone(ms))
    return buf.getvalue()


async def _blocked_feed(gate: asyncio.Event) -> AsyncIterator[bytes]:
    """A queued stream's source: a short WAV, then it stalls until ``gate`` opens."""
    yield _wav(100)
    await gate.wait()


async def _leak_cycle(path: str, testing: Any, gate: asyncio.Event) -> tuple[list[weakref.ref[Any]], dict[str, int]]:
    """One SongbirdClient lifetime through discord.py's ``Connectable.connect`` with fake discord
    objects: every kind of stream, source and track is opened and used, then the client ends by
    ``path``. Returns weak references to every Python object and the live counts while open."""
    client = FakeClient()
    guild = FakeGuild(client)
    world = World(client, guild, guild.add_channel(CHANNEL_ID), guild.add_channel(OTHER_CHANNEL_ID))
    vc = await connect_detached(world, reconnect=path != "lost")
    conn, pump = vc._conn, vc._pump
    assert conn is not None and pump is not None
    testing.inject_map(conn, SPEAKER_SSRC, SPEAKER)
    mixed = vc.listen_mixed()
    user = vc.listen_user(SPEAKER, songbird.MULAW_8K)
    router = vc.listen_users(songbird.PCM16_16K)
    events = vc.events()
    pb = vc.open_playback()
    pb.write_nowait(tone(100, rate=24_000, channels=1))
    mixer = testing.MixerHarness(pb._pb)
    encoded = vc.open_encoded_stream()
    opus = vc.open_opus_stream()
    opus.send_nowait(voice_packets(testing)[0])
    track = vc.play(RawPcm(bytes(4_800), songbird.PCM16_24K))
    queued_stream = vc.queue.enqueue_stream(_blocked_feed(gate))
    queued = vc.enqueue(OpusPackets(voice_packets(testing, 60)))
    inject_ticks(vc, testing, 5, ssrc=SPEAKER_SSRC)
    mixer.pull(3)
    await pump_settle()
    per_user = await asyncio.wait_for(anext(router), 2)
    chunk = mixed.read_nowait()
    assert chunk is not None and user.read_nowait() is not None and per_user.read_nowait() is not None
    assert events.read_nowait() is not None
    await wait_for(lambda: bool(vc.queue._tasks))  # the queued stream is current and being fed
    during = _counts()
    refs: list[weakref.ref[Any]] = [
        weakref.ref(o)
        for o in (vc, conn, pump, vc.queue, mixed, user, router, per_user, events, pb, encoded, opus, track)
        + (queued_stream, queued, chunk)
    ]
    if path == "abandoned":
        world.client._connection._voice_clients.clear()
        del vc
        collect()  # the pump's weakref callback closes the connection ("dropped")
        assert conn.status().close_reason == "dropped"
    elif path == "left_channel":
        await vc.on_voice_state_update({"channel_id": None, "session_id": "s"})  # type: ignore
        await wait_for(lambda: world.client._connection._get_voice_client(GUILD_ID) is None)
        assert conn.status().close_reason == "left_channel"
    elif path == "lost":
        testing.inject_driver_event(conn, "disconnect", reason="io")
        await wait_for(lambda: world.client._connection._get_voice_client(GUILD_ID) is None)
        assert conn.status().close_reason == "lost"
    else:
        await vc.disconnect(force=path == "force")
        assert conn.status().close_reason == "requested"
    assert await pump.closed(timeout=2)
    # `gate` stays shut: the queued stream's feed waits on its source until the library cancels it
    # (cleanup(), or the pump's shutdown when the client was collected).
    await world.guild.settle()
    await pump_settle()
    return refs, during


async def test_connect_close_cycles_return_to_baseline(testing: Any, caplog: pytest.LogCaptureFixture) -> None:
    """100 connect/close cycles leave no Python object and no native object behind: every
    ``weakref`` dies and every ``_debug_live_counts()`` entry returns to its baseline (§13.4).
    Normal closes log no errors."""
    caplog.set_level(logging.WARNING, logger="discord.ext.songbird")
    # Warm-up: anything started once per process (a helper task) belongs in the baseline.
    refs, _ = await _leak_cycle("disconnect", testing, asyncio.Event())
    del refs
    collect()
    await asyncio.sleep(0.2)
    baseline = _counts()
    for i in range(100):
        gate = asyncio.Event()
        refs, during = await _leak_cycle(CLOSE_PATHS[i % len(CLOSE_PATHS)], testing, gate)
        exercised = EXERCISED if FAKE else EXERCISED + EXERCISED_REAL
        assert all(during.get(k, 0) > baseline.get(k, 0) for k in exercised), (baseline, during)
        collect()
        alive = [r() for r in refs if r() is not None]
        assert not alive, (CLOSE_PATHS[i % len(CLOSE_PATHS)], alive)

    def back(now: dict[str, int]) -> bool:
        # Instance counters exactly; runtime tasks at most (one of an earlier test may still finish).
        return all(now[k] <= baseline.get(k, 0) if k in TASK_COUNTS else now[k] == baseline.get(k, 0) for k in now)

    deadline = time.monotonic() + 10  # decode threads exit on their own once their stream closed
    while not back(now := _counts()) and time.monotonic() < deadline:
        await asyncio.sleep(0.01)
        collect()
    assert back(now), (baseline, now)
    assert not [o for o in gc.get_objects() if isinstance(o, (SongbirdClient, native.Connection))]
    flush_native_logs()  # everything the native side logged has reached `logging`
    errors = [r for r in caplog.records if r.levelno >= logging.ERROR and r.name.startswith("discord.ext.songbird")]
    assert not errors, [f"{r.name}: {r.getMessage()}" for r in errors[:5]]
