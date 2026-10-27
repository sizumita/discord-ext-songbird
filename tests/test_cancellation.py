"""Cancellation never loses or duplicates data (§5.5, §13.3 ``test_cancellation.py``)."""

from __future__ import annotations

import asyncio
import random
from typing import Any

from conftest import FAKE, HarnessClient, drain, tone
from discord.ext import songbird


async def test_anext_cancellation_never_loses_chunks(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    iterations = 1_000 if FAKE else 10_000
    rng = random.Random(1234)
    stream = vc.listen_mixed(songbird.PCM16_24K, max_queue_ms=60_000)
    seen: list[int] = []
    injected = 0
    for _ in range(iterations):
        if rng.random() < 0.4:
            testing.inject_tick(vc._conn, [], [])
            vc.harness.step(20)
            injected += 1
        try:
            chunk = await asyncio.wait_for(anext(stream), 0.000_1)
        except TimeoutError:
            continue
        seen.append(chunk.seq)
    seen += [c.seq for c in drain(stream)]
    assert seen == list(range(len(seen)))  # every chunk exactly once, in order
    assert len(seen) == injected
    stream.close()


async def test_cancelled_write_writes_nothing(connected_vc: HarnessClient, testing: Any) -> None:
    """A ``write()`` cancelled at any point (while waiting, right after the space it waits for
    appeared, after its wake-up) writes nothing; one that completes writes everything once. The
    mixer frees space at random moments, so every interleaving is exercised."""
    vc = connected_vc
    pb = vc.open_playback(songbird.PCM16_24K, max_buffer_ms=100, max_gap_ms=40)
    mixer = testing.MixerHarness(pb._pb)
    pb.write_nowait(tone(100, rate=24_000, channels=1))  # full
    rng = random.Random(99)
    attempts = 200
    completed = 0

    async def write() -> None:
        nonlocal completed
        await pb.write(tone(40, rate=24_000, channels=1))
        completed += 1

    async def free_space(delay: float) -> None:
        await asyncio.sleep(delay)
        mixer.pull(2)  # 40 ms played: room for exactly one write
        vc.harness.step(20)

    async def boom(delay: float) -> None:
        await asyncio.sleep(delay)
        raise RuntimeError("cancel the group")

    for _ in range(attempts):
        try:
            async with asyncio.TaskGroup() as tg:
                tg.create_task(write())
                tg.create_task(free_space(rng.random() * 0.002))
                tg.create_task(boom(rng.random() * 0.002))
        except* RuntimeError:
            pass
        assert pb.written_ms == 100 + 40 * completed  # all-or-nothing per attempt
    assert 0 < completed < attempts, completed  # both outcomes happened
    pb.close()


async def test_cancelled_event_wait(connected_vc: HarnessClient, testing: Any) -> None:
    """Events that arrive while ``anext`` waits, including right before it is cancelled, are
    delivered exactly once and in order: received plus left over is exactly what was injected."""
    vc = connected_vc
    events = vc.events(max_queue=4_096, kinds=[songbird.VoiceEvent.UserMapped])
    rng = random.Random(7)
    injected: list[int] = []
    received: list[int] = []

    async def inject_later(delay: float, user: int) -> None:
        await asyncio.sleep(delay)
        testing.inject_map(vc._conn, user - 40_000, user)
        vc.harness.step(20)
        injected.append(user)

    for i in range(300):
        injector = asyncio.create_task(inject_later(rng.random() * 0.001, 50_000 + i)) if i % 2 else None
        try:
            ev = await asyncio.wait_for(anext(events), rng.random() * 0.001)
            received.append(ev.user_id)
        except TimeoutError:
            pass
        if injector is not None:
            await injector
    remaining = [e.user_id for e in drain(events)]
    assert received + remaining == injected
    assert received, "no event was delivered to a waiting anext"
    events.close()
