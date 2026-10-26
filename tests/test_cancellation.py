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
    stream = vc.listen_mixed(songbird.GPT_LIVE, max_queue_ms=60_000)
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


async def test_cancelled_write_writes_nothing(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    pb = vc.open_playback(songbird.GPT_LIVE, max_buffer_ms=100, max_gap_ms=40)
    pb.write_nowait(tone(100, rate=24_000, channels=1))  # full
    rng = random.Random(99)
    for _ in range(50):

        async def boom(delay: float) -> None:
            await asyncio.sleep(delay)
            raise RuntimeError("cancel the group")

        try:
            async with asyncio.TaskGroup() as tg:
                tg.create_task(pb.write(tone(40, rate=24_000, channels=1)))
                tg.create_task(boom(rng.random() * 0.002))
        except* RuntimeError:
            pass
        assert pb.written_ms == 100  # all-or-nothing per attempt
    pb.close()


async def test_cancelled_event_wait(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    events = vc.events(kinds=[songbird.VoiceEvent.UserMapped])
    for i in range(200):
        if i % 2:
            testing.inject_map(vc._conn, 1_000 + i, 50_000 + i)
            vc.harness.step(20)
        try:
            await asyncio.wait_for(anext(events), 0.000_1)
        except TimeoutError:
            pass
    remaining = drain(events)
    assert all(isinstance(e, songbird.VoiceEvent.UserMapped) for e in remaining)
    events.close()
