"""PlaybackStream through the mixer harness (§7, §9.7, §13.3 ``test_playback.py``).

``MixerHarness(playback).pull(n)`` drives the real reader/decoder pair exactly as songbird's mixer
would (a paused track is not pulled); ``Harness.step`` runs the housekeeper with the manual clock.
"""

from __future__ import annotations

import array
import asyncio
import sys
from typing import Any

import pytest
from conftest import HarnessClient, pump_settle, tone
from discord.ext import songbird
from discord.ext.songbird import InvalidAudio, LimitExceeded, PlaybackFull, PlaybackStream, StreamClosed, VoiceEvent

FMT = songbird.GPT_LIVE  # PCM16LE 24 kHz mono: 48 bytes per ms


def pcm(ms: int) -> bytes:
    return tone(ms, rate=24_000, channels=1)


def mixer(testing: Any, pb: PlaybackStream) -> Any:
    return testing.MixerHarness(pb._pb)


def floats(raw: bytes) -> array.array:
    a = array.array("f")
    a.frombytes(raw)
    if sys.byteorder == "big":
        a.byteswap()
    return a


def tick(vc: HarnessClient, mh: Any, n: int = 1) -> bytes:
    """``n`` mixer pulls, each followed by 20 ms of manual time (housekeeping)."""
    out = b""
    for _ in range(n):
        out += mh.pull(1)
        vc.harness.step(20)
    return out


async def test_unwritten_playback_is_paused_and_first_write_resumes(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    mh = mixer(testing, pb)
    assert mh.channels == 1
    out = mh.pull(5)
    assert mh.pulls == 0  # a paused track is not pulled at all
    assert len(out) == 5 * 960 * 4 and not any(floats(out))
    assert testing.track_log(pb.track._native) == []  # created paused; nothing else
    pb.write_nowait(pcm(40))
    assert testing.track_log(pb.track._native) == ["play"]
    out = mh.pull(1)
    assert mh.pulls == 1
    assert any(floats(out))
    assert pb.played_ms == 20 and pb.written_ms == 40
    pb.close()


async def test_write_variants_and_partial_frames(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    data = pcm(20)
    pb.write_nowait(data[:3])  # odd byte count: one frame + a carried byte
    pb.write_nowait(data[3:5])
    pb.write_nowait(data[5:])
    assert pb.written_ms == 20
    b64 = __import__("base64").b64encode(pcm(20)).decode()
    pb.write_base64_nowait(b64)
    pb.write_nowait(b64)  # str is base64 too
    pb.write_base64_nowait(b64.encode())
    await pb.write(pcm(20))
    await pb.write_base64(b64)
    assert pb.written_ms == 120
    with pytest.raises(InvalidAudio):
        pb.write_base64_nowait("@@not base64@@")
    assert pb.written_ms == 120  # nothing consumed
    pb.close()


async def test_buffer_types(connected_vc: HarnessClient) -> None:
    np = pytest.importorskip("numpy")
    vc = connected_vc
    pb = vc.open_playback(FMT)
    raw = pcm(20)
    pb.write_nowait(bytearray(raw))
    pb.write_nowait(memoryview(raw))
    pb.write_nowait(np.frombuffer(raw, "<i2"))
    pb.write_nowait(array.array("h", np.frombuffer(raw, "<i2").tolist()))
    assert pb.written_ms == 80
    for bad in (
        np.zeros(480, np.float32),
        array.array("f", [0.0] * 480),
        np.zeros(480, ">i2"),
        array.array("i", [0] * 480),
    ):
        with pytest.raises(InvalidAudio):
            pb.write_nowait(bad)
    pb.close()
    f32 = vc.open_playback(songbird.F32_48K)
    f32.write_nowait(np.zeros(960, np.float32))
    assert f32.written_ms == 20
    with pytest.raises(InvalidAudio):
        f32.write_nowait(np.zeros(960, np.int16))
    f32.close()


async def test_full_then_await_write(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT, max_buffer_ms=100, max_gap_ms=40)
    mh = mixer(testing, pb)
    pb.write_nowait(pcm(100))
    with pytest.raises(PlaybackFull) as ei:
        pb.write_nowait(pcm(40))
    assert ei.value.needed_ms == 40 and ei.value.free_ms == 0
    assert pb.written_ms == 100  # nothing consumed
    writer = asyncio.create_task(pb.write(pcm(40), timeout=2))
    await pump_settle()
    assert not writer.done()
    tick(vc, mh, 3)
    await asyncio.wait_for(writer, 2)
    assert pb.written_ms == 140
    with pytest.raises(LimitExceeded):
        pb.write_nowait(pcm(200))  # can never fit
    pb.close()


async def test_write_timeout_writes_nothing(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT, max_buffer_ms=100, max_gap_ms=40)
    pb.write_nowait(pcm(100))
    with pytest.raises(TimeoutError):
        await pb.write(pcm(20), timeout=0.05)
    assert pb.written_ms == 100
    pb.close()


async def test_clear_is_immediate(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    mh = mixer(testing, pb)
    pb.write_nowait(pcm(1_000), tag="item-1")
    assert any(floats(tick(vc, mh)))
    res = pb.clear()
    assert res.discarded_ms == 980
    assert pb.buffered_ms == 0
    assert not any(floats(mh.pull(1)))  # silent at the next pull
    (item,) = res.items
    assert item.tag == "item-1" and res.item("item-1") is item
    assert item.played_ms == 20 and item.heard_ms <= item.written_ms == 1_000
    assert res.generation >= 1
    assert pb.stats().clears == 1
    pb.close()


async def test_underrun_item_is_reported_and_rejected(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    mh = mixer(testing, pb)
    pb.write_nowait(pcm(40), tag="a")
    tick(vc, mh, 3)  # fully played, then one underrun tick; the tag is still open
    res = pb.clear()
    assert [i.tag for i in res.items] == ["a"]
    assert res.items[0].played_ms == res.items[0].written_ms == 40
    pb.write_nowait(pcm(40), tag="a")  # a late delta of the cancelled item
    assert pb.written_ms == 40
    assert pb.stats().rejected_ms == 40
    pb.write_nowait(pcm(40), tag="b")  # a new item plays
    assert pb.written_ms == 80
    pb.close()


async def test_end_item_complete(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    mh = mixer(testing, pb)
    pb.write_nowait(pcm(40), tag="x")
    pb.end_item("x")
    item = pb.item("x")
    assert item is not None and not item.complete
    tick(vc, mh, 2)
    item = pb.item("x")
    assert item is not None and item.complete and item.played_ms == 40
    assert pb.played_ms_of("x") == 40 and pb.played_ms_of("nope") == 0
    assert pb.item("nope") is None
    pb.close()


async def test_reject_until_gap(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    for at in (0, 100, 200):
        pb.write_nowait(pcm(100), at_ms=at)
    pb.clear(reject_until_gap_ms=250)
    pb.write_nowait(pcm(100), at_ms=300)  # contiguous with the interrupted utterance
    pb.write_nowait(pcm(100), at_ms=450)  # still within the window
    assert pb.written_ms == 300
    pb.write_nowait(pcm(100), at_ms=900)  # starts ≥ 250 ms after the cut: a new reply
    assert pb.written_ms == 400
    pb.close()


async def test_hold_ms(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    pb.clear(hold_ms=100, reject_late=False)
    pb.write_nowait(pcm(20))
    assert pb.written_ms == 0
    vc.harness.step(120)
    pb.write_nowait(pcm(20))
    assert pb.written_ms == 20
    pb.close()


async def test_gap_fill_after_idle_pause(connected_vc: HarnessClient, testing: Any) -> None:
    """§7.6 regression: wall-clock anchor; an idle-paused track does not add stale silence."""
    vc = connected_vc
    pb = vc.open_playback(FMT)
    mh = mixer(testing, pb)
    pb.write_nowait(pcm(1_000), at_ms=0)
    tick(vc, mh, 50)  # 1000 ms played
    tick(vc, mh, 14)  # 280 ms idle → auto-pause
    assert pb.is_idle
    assert testing.track_log(pb.track._native)[-1] == "pause"
    vc.harness.step(2_500 - 1_280)  # the next delta arrives on time, at t0 + 2500 ms
    pb.write_nowait(pcm(100), at_ms=2_500)
    assert pb.buffered_ms <= 100 + 20
    pb.close()


async def test_gap_fill_for_an_early_delta(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    mh = mixer(testing, pb)
    pb.write_nowait(pcm(1_000), at_ms=0)
    tick(vc, mh, 50)
    tick(vc, mh, 5)  # t0 + 1100 ms
    pb.write_nowait(pcm(100), at_ms=2_500)
    assert 1_400 - 20 <= pb.buffered_ms - 100 <= 1_400 + 20
    assert pb.stats().gap_filled_ms >= 1_380
    assert pb.written_ms == 1_100  # gap silence is not "written"
    pb.close()


async def test_late_delta_appends(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    pb.write_nowait(pcm(200), at_ms=1_000)
    pb.write_nowait(pcm(100), at_ms=500)  # overlap: append, never trim speech
    assert pb.buffered_ms == 300
    pb.write_nowait(pcm(20), at_ms=2**63 - 1)  # huge jump: re-anchor, no silence, no panic
    pb.write_nowait(pcm(20), at_ms=-(2**63))
    assert pb.buffered_ms == 340
    pb.close()


async def test_prebuffer_shorter_reply_still_plays(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT, prebuffer_ms=200)
    mh = mixer(testing, pb)
    pb.write_nowait(pcm(60))
    tick(vc, mh, 10)  # gated: silence, nothing consumed
    assert pb.played_ms == 0
    tick(vc, mh, 3)
    assert pb.played_ms == 60
    pb.close()


async def test_prebuffer_released_by_end_item(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT, prebuffer_ms=200)
    mh = mixer(testing, pb)
    pb.write_nowait(pcm(60), tag="t")
    pb.end_item("t")
    tick(vc, mh, 3)
    assert pb.played_ms == 60
    pb.close()


async def test_self_mute_holds_position(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    mh = mixer(testing, pb)
    await vc.set_self_mute(True)
    pb.write_nowait(pcm(40))
    tick(vc, mh, 5)
    assert pb.played_ms == 0 and pb.buffered_ms == 40
    drain = asyncio.create_task(pb.drain(timeout=2))
    await pump_settle()
    assert not drain.done()
    await vc.set_self_mute(False)
    tick(vc, mh, 3)
    assert await drain is True
    assert pb.played_ms == 40
    pb.close()


async def test_drain_and_wait_idle(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT, idle_ms=100)
    mh = mixer(testing, pb)
    assert await pb.drain(timeout=0.1) is True  # nothing written
    await pb.wait_idle()  # nothing playing
    pb.write_nowait(pcm(40))
    assert not pb.is_idle
    drained = asyncio.create_task(pb.drain())
    idle = asyncio.create_task(pb.wait_idle())
    await pump_settle()
    assert not drained.done() and not idle.done()
    tick(vc, mh, 3)
    assert await asyncio.wait_for(drained, 2) is True
    assert not idle.done()
    tick(vc, mh, 6)
    await asyncio.wait_for(idle, 2)
    assert pb.is_idle
    assert await pb.drain(timeout=0.05) is True
    pb.close()
    assert await pb.drain() is False


async def test_aclose_drain(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    mh = mixer(testing, pb)
    pb.write_nowait(pcm(40))
    closing = asyncio.create_task(pb.aclose(drain=True, timeout=2))
    await pump_settle()
    tick(vc, mh, 4)
    await asyncio.wait_for(closing, 2)
    assert pb.closed and mh.ended
    assert pb.played_ms == 40
    with pytest.raises(StreamClosed):
        pb.write_nowait(pcm(20))


async def test_aclose_drain_times_out_to_abort(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    pb.write_nowait(pcm(1_000))  # nothing pulls it: the drain cannot finish
    await pb.aclose(drain=True, timeout=0.05)
    assert pb.closed and pb.buffered_ms == 0


async def test_events_started_idle_ended(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    async with vc.events(kinds=[VoiceEvent.PlaybackStarted, VoiceEvent.PlaybackIdle, VoiceEvent.PlaybackEnded]) as ev:
        pb = vc.open_playback(FMT, idle_ms=60)
        mh = mixer(testing, pb)
        pb.write_nowait(pcm(40))
        tick(vc, mh, 8)
        pb.close()
        tick(vc, mh, 3)
        got = []
        while (e := ev.read_nowait()) is not None:
            got.append(e)
    assert [type(e) for e in got] == [VoiceEvent.PlaybackStarted, VoiceEvent.PlaybackIdle, VoiceEvent.PlaybackEnded]
    assert all(e.playback_id == pb.playback_id for e in got)
    assert got[1].played_ms == 40


async def test_close_and_writes_after_close(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    pb.write_nowait(pcm(100))
    pb.close()
    pb.close()  # idempotent
    assert pb.closed and pb.buffered_ms == 0
    with pytest.raises(StreamClosed):
        pb.write_nowait(pcm(20))
    with pytest.raises(StreamClosed):
        await pb.write(pcm(20))
    assert (await pb.track.wait_ended()).reason == "stopped"


async def test_pause_resume_volume(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    mh = mixer(testing, pb)
    pb.write_nowait(pcm(100))
    tick(vc, mh)
    pb.pause()
    tick(vc, mh, 2)
    assert pb.played_ms == 20
    pb.resume()
    tick(vc, mh)
    assert pb.played_ms == 40
    assert pb.volume == 1.0
    pb.volume = 0.0
    tick(vc, mh, 3)  # 5 ms ramp, then silence
    assert not any(floats(mh.pull(1)))
    with pytest.raises(LimitExceeded):
        pb.volume = 4.5
    pb.close()


async def test_open_playback_limits(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    with pytest.raises(LimitExceeded):
        vc.open_playback(max_buffer_ms=100, prebuffer_ms=1_000)
    with pytest.raises(LimitExceeded):
        vc.open_playback(max_buffer_ms=100, max_gap_ms=10_000)
    with pytest.raises(LimitExceeded):
        vc.open_playback(max_buffer_ms=10**12)
    with pytest.raises(InvalidAudio):
        vc.open_playback(FMT).write_nowait(pcm(20), tag="x" * 129)
    streams = [vc.open_playback(FMT) for _ in range(16)]
    with pytest.raises(LimitExceeded):
        vc.open_playback(FMT)
    for s in streams:
        s.close()


async def test_max_gap_default_follows_the_buffer(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT, max_buffer_ms=500)  # default max_gap_ms = min(2000, 250)
    pb.write_nowait(pcm(20), at_ms=0)
    pb.write_nowait(pcm(20), at_ms=300)  # a 280 ms gap exceeds 250: a discontinuity, no silence
    assert pb.buffered_ms == 40
    pb.close()


async def test_playback_stats(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    mh = mixer(testing, pb)
    pb.write_nowait(pcm(40))
    tick(vc, mh, 3)
    st = pb.stats()
    assert (st.written_ms, st.played_ms, st.buffered_ms) == (40, 40, 0)
    assert st.underrun_ms == 20
    assert st.heard_ms <= st.played_ms
    vc.harness.step(100)
    assert pb.heard_ms == pb.played_ms == 40  # converges once output_latency_ms has passed
    assert pb.position_ms == 60
    pb.close()


@pytest.mark.real_only
async def test_mixer_pull_does_not_allocate(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pb = vc.open_playback(FMT)
    mh = mixer(testing, pb)
    pb.write_nowait(pcm(1_000))
    mh.pull(2)  # warm up
    before = testing.alloc_count()
    mh.pull(40)
    assert testing.alloc_count() == before
    pb.close()


class _StubbornSink:
    """A native sink/playback stand-in that reports space but rejects the first writes
    (another thread took the space, or gap silence does not fit)."""

    track: Any = None
    wake_id = 0
    playback_id = 1

    def __init__(self, rejections: int) -> None:
        self.rejections = rejections
        self.calls = 0

    def _attempt(self, *_args: Any) -> None:
        self.calls += 1
        if self.calls <= self.rejections:
            raise PlaybackFull(20, 20)

    write = send = write_base64 = _attempt

    def arm_space(self, *_args: Any) -> bool:
        return True


@pytest.mark.parametrize("kind", ["playback", "encoded", "opus"])
async def test_write_retries_never_starve_the_loop(kind: str) -> None:
    from discord.ext.songbird import EncodedStream, OpusPacketStream

    stub = _StubbornSink(rejections=3)
    sink: Any = stub  # stands in for the native sink types
    ticks = 0

    async def other_task() -> None:
        nonlocal ticks
        while True:
            ticks += 1
            await asyncio.sleep(0)

    t = asyncio.create_task(other_task())
    await asyncio.sleep(0)
    before = ticks
    pump: Any = None  # never reached: arm_space always reports space
    if kind == "playback":
        await asyncio.wait_for(PlaybackStream(sink, pump, FMT).write(b"\0\0"), 2)
    elif kind == "encoded":
        await asyncio.wait_for(EncodedStream(sink, pump).write(b"\0\0"), 2)
    else:
        await asyncio.wait_for(OpusPacketStream(sink, pump).send(b"\xf8\xff\xfe"), 2)
    t.cancel()
    assert stub.calls == 4
    assert ticks > before  # the retry loop yielded to the event loop between attempts
