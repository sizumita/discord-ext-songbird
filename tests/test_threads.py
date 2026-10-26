"""Free-threading and cross-thread use (§5.6, §13.5 ``test_threads.py``). Also runs on GIL builds."""

from __future__ import annotations

import asyncio
import sys
import threading
import time
from typing import Any

import pytest
from conftest import HarnessClient, run_isolated, tone
from discord.ext import songbird
from discord.ext.songbird import PlaybackFull, StreamClosed


@pytest.mark.skipif(not hasattr(sys, "_is_gil_enabled") or sys._is_gil_enabled(), reason="needs a free-threaded build")
def test_gil_stays_disabled_after_import() -> None:
    proc = run_isolated(
        """
        import sys
        import discord
        from discord.ext import songbird
        try:
            import numpy, pyarrow  # noqa: F401
        except ImportError:
            pass
        assert not sys._is_gil_enabled(), "importing re-enabled the GIL"
        print("OK")
        """,
        env={"PYTHON_GIL": "0"},
    )
    assert proc.returncode == 0, proc.stderr
    assert "OK" in proc.stdout


async def test_concurrent_read_nowait_delivers_each_chunk_once(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    stream = vc.listen_mixed(songbird.GPT_LIVE, max_queue_ms=60_000)
    for _ in range(400):
        testing.inject_tick(vc._conn, [], [])
        vc.harness.step(20)
    got: list[list[int]] = [[] for _ in range(4)]
    barrier = threading.Barrier(4)

    def reader(i: int) -> None:
        barrier.wait()
        while (c := stream.read_nowait()) is not None:
            got[i].append(c.seq)

    threads = [threading.Thread(target=reader, args=(i,)) for i in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(10)
    seqs = sorted(s for g in got for s in g)
    assert seqs == list(range(400))
    stream.close()


async def test_writers_and_clear_race_the_mixer(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(songbird.GPT_LIVE, max_buffer_ms=2_000, max_gap_ms=1_000)
    mh = testing.MixerHarness(pb._pb)
    data = tone(20, rate=24_000, channels=1)
    stop = threading.Event()
    errors: list[BaseException] = []

    def writer(i: int) -> None:
        try:
            while not stop.is_set():
                try:
                    pb.write_nowait(data, tag=f"w{i}")
                except PlaybackFull:
                    pass
                if i == 0:
                    pb.clear(reject_late=False)
                st = pb.stats()
                assert st.played_ms <= st.written_ms
        except BaseException as e:  # pragma: no cover - reported below
            errors.append(e)

    threads = [threading.Thread(target=writer, args=(i,)) for i in range(8)]
    for t in threads:
        t.start()
    deadline = time.monotonic() + 1.0
    while time.monotonic() < deadline:
        mh.pull(1)
    stop.set()
    for t in threads:
        t.join(10)
    assert not errors, errors
    st = pb.stats()
    assert st.played_ms <= st.written_ms
    assert all(i.played_ms <= i.written_ms for i in pb.clear().items)
    pb.close()


async def test_close_from_a_foreign_thread_wakes_the_loop(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    stream = vc.listen_mixed()
    reader = asyncio.create_task(stream.read(timeout=5))
    await asyncio.sleep(0.01)
    threading.Thread(target=stream.close).start()
    with pytest.raises(StreamClosed):
        await asyncio.wait_for(reader, 5)


@pytest.mark.real_only
async def test_loop_jitter_while_decoding(connected_vc: HarnessClient, testing: Any) -> None:
    """``loop.call_later`` jitter p99 < 5 ms while 20 speakers are decoded off the loop."""
    vc = connected_vc
    packets = testing.encode_opus(tone(20))
    for i in range(20):
        testing.inject_map(vc._conn, 500 + i, 60_000 + i)
    stream = vc.listen_mixed(songbird.GPT_LIVE, max_queue_ms=60_000)
    stop = threading.Event()

    def feeder() -> None:
        while not stop.is_set():
            try:
                testing.inject_tick(vc._conn, [(500 + i, packets[0]) for i in range(20)], [])
            except songbird.LimitExceeded:
                pass
            vc.harness.step(20)
            time.sleep(0.02)

    t = threading.Thread(target=feeder)
    t.start()
    loop = asyncio.get_running_loop()
    lateness: list[float] = []
    for _ in range(100):
        fut = loop.create_future()
        start = loop.time()
        loop.call_later(0.01, fut.set_result, None)
        await fut
        lateness.append(loop.time() - start - 0.01)
    stop.set()
    t.join(5)
    lateness.sort()
    assert lateness[98] < 0.005
    stream.close()
