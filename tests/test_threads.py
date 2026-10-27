"""Free-threading and cross-thread use (§5.6, §13.5 ``test_threads.py``). Also runs on GIL builds."""

from __future__ import annotations

import array
import asyncio
import faulthandler
import gc
import json
import os
import struct
import sys
import sysconfig
import threading
import time
from collections.abc import Callable
from typing import Any

import pytest
from conftest import SSRC, HarnessClient, run_isolated, tone
from discord.ext import songbird
from discord.ext.songbird import PlaybackFull, StreamClosed

FREE_THREADED = bool(sysconfig.get_config_var("Py_GIL_DISABLED"))


def test_gil_state_after_import() -> None:
    """G7 / §5.6: importing the package, discord, numpy and pyarrow leaves the GIL disabled on a
    free-threaded build (3.14t) and enabled on a GIL build (3.14), and the native module loaded is
    the one built for this interpreter's ABI.

    The child runs **without** ``PYTHON_GIL`` (CI exports ``PYTHON_GIL=0`` for 3.14t runs):
    ``PYTHON_GIL=0`` keeps the GIL off even when a module without ``Py_mod_gil`` asks for it, so
    the check would pass whatever the import did."""
    proc = run_isolated(
        """
        import json, sys, sysconfig
        import discord  # noqa: F401
        from discord.ext import songbird  # noqa: F401
        from discord.ext.songbird import native
        try:
            import numpy, pyarrow  # noqa: F401
        except ImportError:
            pass
        print(json.dumps({
            "forced": sys.flags.gil,
            "enabled": sys._is_gil_enabled(),
            "native": getattr(native, "__file__", None),
            "fake": bool(getattr(native, "FAKE", False)),
            "suffix": sysconfig.get_config_var("EXT_SUFFIX"),
        }))
        """,
        env={"PYTHON_GIL": None},
    )
    assert proc.returncode == 0, proc.stderr
    assert "has been enabled to load module" not in proc.stderr, proc.stderr
    state = json.loads(proc.stdout.strip().splitlines()[-1])
    if FREE_THREADED:
        assert state["forced"] is None, "the child must not force the GIL off"
        assert state["enabled"] is False, "importing re-enabled the GIL"
    else:
        assert state["enabled"] is True
    if not state["fake"]:
        assert state["native"].endswith(state["suffix"]), (state["native"], state["suffix"])


async def test_concurrent_read_nowait_delivers_each_chunk_once(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    stream = vc.listen_mixed(songbird.PCM16_24K, max_queue_ms=60_000)
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
    pb = vc.open_playback(songbird.PCM16_24K, max_buffer_ms=2_000, max_gap_ms=1_000)
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
    assert all(i.played_ms <= i.written_ms for i in pb.clear().segments)
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
    """Decoding 20 speakers off the loop adds < 5 ms to the ``loop.call_later`` jitter p99."""
    if sys.platform == "darwin" and os.environ.get("GITHUB_ACTIONS") == "true":
        # Their timer noise alone swings between 3 and 12 ms within one run (idle windows of
        # 2.8, 11.7 and 10.9 ms back to back), so no bound separates noise from a stall there. The
        # decode path is the same on every platform; the Linux runners and local runs check it.
        pytest.skip("GitHub macOS runner: idle timer noise reaches 12 ms; checked on Linux and locally")
    vc = connected_vc
    packets = testing.encode_opus(tone(20))
    for i in range(20):
        testing.inject_map(vc._conn, 500 + i, 60_000 + i)
    stream = vc.listen_mixed(songbird.PCM16_24K, max_queue_ms=60_000)
    stop = threading.Event()

    def feeder() -> None:
        while not stop.is_set():
            try:
                testing.inject_tick(vc._conn, [(500 + i, packets[0]) for i in range(20)], [])
            except songbird.LimitExceeded:
                pass
            vc.harness.step(20)
            time.sleep(0.02)

    async def p99() -> float:
        loop = asyncio.get_running_loop()
        lateness: list[float] = []
        for _ in range(100):
            fut = loop.create_future()
            start = loop.time()
            loop.call_later(0.01, fut.set_result, None)
            await fut
            lateness.append(loop.time() - start - 0.01)
        lateness.sort()
        return lateness[98]

    async def best_of_three(bound: float) -> list[float]:
        windows: list[float] = []
        while len(windows) < 3 and (not windows or windows[-1] >= bound):
            windows.append(await p99())
        return windows

    # Timer and scheduler noise on a shared host can exceed 5 ms with nothing stalling the loop
    # (GitHub's macOS runners measured 6-16 ms here). So the bound is relative to the idle p99
    # measured first, and it must hold in one of three ~1 s windows. Work that really stalls the
    # loop (e.g. decoding while holding the GIL) adds its stall to every window and still fails.
    idle = await best_of_three(0.001)
    bound = min(idle) + 0.005
    t = threading.Thread(target=feeder)
    t.start()
    try:
        loaded = await best_of_three(bound)
    finally:
        stop.set()
        t.join(5)
    assert min(loaded) < bound, {"idle": idle, "loaded": loaded}
    stream.close()


@pytest.mark.real_only
@pytest.mark.parametrize("view", ["bytearray", "memoryview"])
def test_buffer_copy_is_one_state_of_a_concurrently_written_bytearray(testing: Any, view: str) -> None:
    """On the free-threaded build another thread may rewrite a ``bytearray`` in place while a call
    (``write_nowait``, ``RawPcm``, ``AudioFile``, …) copies it. The copy must be one whole state of
    the object, never a mix of two, and never a Rust slice over memory being written (a data race):
    it runs through CPython's own buffer copy inside the exporter's critical section."""
    size = 1 << 20
    first, second = b"\x11" * size, b"\x22" * size
    data = bytearray(first)
    src = data if view == "bytearray" else memoryview(data)
    stop = threading.Event()

    def writer() -> None:
        while not stop.is_set():
            data[:] = second  # same length: allowed while the buffer is exported
            data[:] = first

    t = threading.Thread(target=writer)
    t.start()
    try:
        copies = [testing.copy_buffer(src) for _ in range(300)]
    finally:
        stop.set()
        t.join(5)
    torn = sum(1 for c in copies if c not in (first, second))
    assert torn == 0, f"{torn} of {len(copies)} copies mixed two states"


def _endless_wav_header(rate: int = 48_000, channels: int = 2) -> bytes:
    """A PCM16 WAV header announcing ten minutes of audio, so an encoded stream keeps decoding
    whatever PCM is written after it."""
    data = rate * channels * 2 * 600
    fmt = struct.pack("<HHIIHH", 1, channels, rate, rate * channels * 2, channels * 2, 16)
    return (
        b"RIFF"
        + struct.pack("<I", 36 + data)
        + b"WAVEfmt "
        + struct.pack("<I", 16)
        + fmt
        + b"data"
        + struct.pack("<I", data)
    )


async def test_cross_thread_load_and_a_foreign_close_never_deadlock(connected_vc: HarnessClient, testing: Any) -> None:
    """§13.5 "no deadlock (watchdog)", on GIL and free-threaded builds alike (G7).

    Every native call §5.2 allows on other Python threads runs at once, next to the worker step and
    the mixer pull (the test hooks' detached stand-ins for the RX runtime and songbird's mixer
    thread), while another thread forces cyclic GC and, on a GIL build, the GIL changes hands every
    10 µs. Then a foreign thread closes the connection under that load. A native call that waited,
    still attached, on a thread that needs the interpreter would hang here; the watchdog then dumps
    every thread's stack and ends the run instead of hanging CI."""
    vc = connected_vc
    conn = vc._conn
    assert conn is not None
    speaker = 7_000_000_000_000_001
    packet = testing.encode_opus(tone(20))[0]
    testing.inject_map(conn, SSRC, speaker)
    mixed = vc.listen_mixed(songbird.PCM16_24K, max_queue_ms=2_000)
    user = vc.listen_user(speaker, songbird.PCM16_16K, max_queue_ms=2_000)
    events = vc.events()
    pb = vc.open_playback(songbird.PCM16_24K, max_buffer_ms=2_000, max_gap_ms=1_000)
    mixer = testing.MixerHarness(pb._pb)
    encoded = vc.open_encoded_stream(max_buffer_bytes=64 << 10)
    opus = vc.open_opus_stream(max_packets=50)
    track = vc.play(songbird.RawPcm(tone(1_000, rate=24_000, channels=1), songbird.PCM16_24K), loops=None)
    pcm = tone(20, rate=24_000, channels=1)
    pcm_copy = bytearray(pcm)  # a non-bytes buffer: copied before the write
    big_pcm = tone(12_000, rate=48_000, channels=1)  # > 1 MiB: parsed detached (§5.2)
    floats = array.array("f", [0.25] * 44_100 * 2)

    stop = threading.Event()
    closed = threading.Event()
    errors: list[BaseException] = []
    progress: dict[str, int] = {}
    expected = (PlaybackFull, songbird.LimitExceeded)

    def run(name: str, body: Callable[[], None]) -> threading.Thread:
        def loop() -> None:
            progress[name] = 0
            while not stop.is_set():
                try:
                    body()
                except expected:
                    pass
                except songbird.SongbirdError as e:
                    if not closed.is_set() and not conn.closed:
                        errors.append(e)
                        return
                except BaseException as e:  # pragma: no cover - reported below
                    errors.append(e)
                    return
                progress[name] += 1

        t = threading.Thread(target=loop, name=f"load-{name}", daemon=True)
        t.start()
        return t

    def worker() -> None:
        testing.inject_tick(conn, [(SSRC, packet)], [])
        vc.harness.step(20)

    def writer(i: int) -> Callable[[], None]:
        def body() -> None:
            if i % 2:
                pb.write_nowait(pcm_copy, tag=f"w{i}")
            else:
                pb.write_nowait(pcm, tag=f"w{i}")
            if i == 0:
                pb.clear(reject_late=False)
            st = pb.stats()
            assert st.played_ms <= st.written_ms
            _ = (pb.buffered_ms, pb.heard_ms, pb.is_idle)

        return body

    header_sent = threading.Event()

    def encoded_writer() -> None:
        if not header_sent.is_set():
            encoded.write_nowait(_endless_wav_header())
            header_sent.set()
        encoded.write_nowait(tone(20))

    def opus_sender() -> None:
        opus.send_nowait(packet)
        if progress["opus"] % 25 == 0:
            opus.clear()

    def reader() -> None:
        for s in (mixed, user):
            while s.read_nowait() is not None:
                pass
            _ = (s.queued_ms, s.dropped_ms, s.stats())
        while events.read_nowait() is not None:
            pass

    def sources() -> None:
        songbird.RawPcm(big_pcm, songbird.PCM16_48K)
        songbird.RawPcm.from_samples(floats, 44_100, 2)
        conn.stats()
        conn.status()

    def collector() -> None:
        cycle: list[Any] = []
        cycle.append(cycle)
        gc.collect()
        time.sleep(0.002)

    old_switch = sys.getswitchinterval()
    faulthandler.dump_traceback_later(120, exit=True)  # a hang anywhere, the loop included
    threads: list[threading.Thread] = []
    try:
        sys.setswitchinterval(1e-5)
        threads = [
            run("worker", worker),
            run("mixer", lambda: mixer.pull(1)),
            *(run(f"writer{i}", writer(i)) for i in range(3)),
            run("encoded", encoded_writer),
            run("opus", opus_sender),
            *(run(f"reader{i}", reader) for i in range(2)),
            run("sources", sources),
            run("gc", collector),
        ]
        loop = asyncio.get_running_loop()
        last = loop.time()
        worst = 0.0
        deadline = last + 1.0
        while loop.time() < deadline:  # the loop thread keeps using its own API meanwhile
            track.pause()
            track.resume()
            vc.stats()
            await asyncio.sleep(0.001)
            now = loop.time()
            worst, last = max(worst, now - last), now
        closer = threading.Thread(target=conn.close, name="foreign-close")
        closer.start()
        closer.join(10)
        assert not closer.is_alive(), "close() from a foreign thread hung"
        closed.set()
        await asyncio.sleep(0.2)  # every thread now runs against a closed connection
        stop.set()
        for t in threads:
            t.join(max(0.0, 10 - (loop.time() - deadline)))
        stuck = [t.name for t in threads if t.is_alive()]
        if stuck:
            faulthandler.dump_traceback(all_threads=True)
        assert not stuck, f"threads still blocked after close: {stuck}"
    finally:
        stop.set()
        sys.setswitchinterval(old_switch)
        faulthandler.cancel_dump_traceback_later()
    assert not errors, errors
    assert all(progress[t.name.removeprefix("load-")] > 0 for t in threads), progress
    assert worst < 2.0, f"the loop stalled for {worst:.3f} s"
    assert conn.closed
    for s in (mixed, user):
        assert s.closed
        with pytest.raises(StreamClosed):  # what T6 left queued drains, then the close is reported
            for _ in range(10_000):
                s.read_nowait()
    assert pb.closed
    with pytest.raises(StreamClosed):
        pb.write_nowait(pcm)
    await asyncio.wait_for(track.wait_ended(), 5)
