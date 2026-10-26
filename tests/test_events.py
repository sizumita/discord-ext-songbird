"""Event streams (§8.5, §8.11, §9.6, §13.3 ``test_events.py``)."""

from __future__ import annotations

from typing import Any

import pytest
from conftest import SSRC, HarnessClient, drain, voice_packets
from discord.ext.songbird import StreamClosed, VoiceEvent

A, B = 7_000_000_000_000_001, 7_000_000_000_000_002


async def test_kind_filter_and_match(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pkt = voice_packets(testing)[0]
    async with vc.events(kinds=[VoiceEvent.SpeakingStarted]) as only_start, vc.events() as everything:
        testing.inject_map(vc._conn, 101, A)
        testing.inject_tick(vc._conn, [(101, pkt)], [])
        vc.harness.step(20)
        for _ in range(20):  # past the 200 ms hangover
            testing.inject_tick(vc._conn, [], [101])
            vc.harness.step(20)
        starts = drain(only_start)
        all_events = drain(everything)
    assert len(starts) == 1
    match starts[0]:
        case VoiceEvent.SpeakingStarted(user_id=uid, timeline_ms=t):
            assert (uid, t) == (A, 0)
        case _:
            pytest.fail(f"unexpected event {starts[0]!r}")
    kinds = [type(e) for e in all_events]
    assert kinds.index(VoiceEvent.UserMapped) < kinds.index(VoiceEvent.SpeakingStarted)
    assert kinds.index(VoiceEvent.SpeakingStarted) < kinds.index(VoiceEvent.SpeakingStopped)
    stopped = next(e for e in all_events if isinstance(e, VoiceEvent.SpeakingStopped))
    assert stopped.user_id == A and stopped.duration_ms >= 20
    mapped = next(e for e in all_events if isinstance(e, VoiceEvent.UserMapped))
    assert (mapped.user_id, mapped.ssrc) == (A, 101)


async def test_lagged_after_overflow(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    async with vc.events(max_queue=2, kinds=[VoiceEvent.UserMapped]) as events:
        for i in range(5):
            testing.inject_map(vc._conn, 200 + i, 9_000 + i)
        vc.harness.step(20)
        got = drain(events)
        assert events.dropped == 3
    assert isinstance(got[0], VoiceEvent.Lagged) and got[0].dropped == 3
    assert [e.user_id for e in got[1:]] == [9_003, 9_004]


async def test_user_left_after_speaking_stopped(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pkt = voice_packets(testing)[0]
    async with vc.events(kinds=[VoiceEvent.SpeakingStopped, VoiceEvent.UserLeft]) as events:
        testing.inject_map(vc._conn, 101, B)
        testing.inject_tick(vc._conn, [(101, pkt)], [])
        vc.harness.step(20)
        testing.inject_user_left(vc._conn, B)
        vc.harness.step(20)
        got = drain(events)
    assert [type(e) for e in got] == [VoiceEvent.SpeakingStopped, VoiceEvent.UserLeft]


async def test_disconnected_is_last(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    events = vc.events()
    await vc.disconnect()
    got = [e async for e in events]
    assert got, "expected at least the final Disconnected event"
    last = got[-1]
    assert isinstance(last, VoiceEvent.Disconnected)
    assert last.will_retry is False
    with pytest.raises(StreamClosed):
        events.read_nowait()


async def test_connected_event_and_reconnected(world: Any, testing: Any) -> None:
    from conftest import connect_detached

    vc = await connect_detached(world)
    try:
        async with vc.events() as events:
            testing.inject_driver_event(vc._conn, "reconnect", ssrc=SSRC)
            ev = await anext(events)
        assert isinstance(ev, VoiceEvent.Reconnected)
        assert ev.ssrc == SSRC
    finally:
        await vc.disconnect()


async def test_event_stream_limits(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    streams = [vc.events() for _ in range(16)]
    with pytest.raises(ValueError):
        vc.events()
    for s in streams:
        s.close()
    with pytest.raises(TypeError):
        vc.events(kinds=[int])


async def test_receive_off_still_reports_users(world: Any, testing: Any) -> None:
    """``receive=False`` only stops audio: mappings and leaves (WebSocket events) are still
    reported, so a music bot can follow listeners with ``events()``. Speaking events need audio."""
    import asyncio

    from conftest import connect_detached
    from discord.ext import songbird

    vc = await connect_detached(world, config=songbird.VoiceConfig(receive=False))
    try:
        pkt = voice_packets(testing)[0]
        async with vc.events(kinds=[VoiceEvent.UserMapped, VoiceEvent.UserLeft]) as users, vc.events() as everything:
            testing.inject_map(vc._conn, 101, A)
            for _ in range(3):
                testing.inject_tick(vc._conn, [(101, pkt)], [])
                vc.harness.step(20)
            testing.inject_user_left(vc._conn, A)
            vc.harness.step(20)
            got = [await asyncio.wait_for(anext(users), 2) for _ in range(2)]
            rest = drain(everything)
        assert isinstance(got[0], VoiceEvent.UserMapped) and (got[0].user_id, got[0].ssrc) == (A, 101)
        assert isinstance(got[1], VoiceEvent.UserLeft) and got[1].user_id == A
        assert not [e for e in rest if isinstance(e, VoiceEvent.SpeakingStarted | VoiceEvent.SpeakingStopped)]
    finally:
        await vc.disconnect()
