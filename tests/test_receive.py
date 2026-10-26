"""Receive streams through the detached harness (§8, §9.5, §13.3 ``test_receive.py``).

Ticks are injected through the same channel ``Dispatch`` feeds; one tick per 20 ms of manual time,
so the timeline never synthesises or skips unless a test wants it to.
"""

from __future__ import annotations

import base64
import gc
from typing import Any

import pytest
from conftest import FakeUser, HarnessClient, drain, inject_ticks, pump_settle, voice_packets
from discord.ext import songbird
from discord.ext.songbird import AudioFormat, StreamClosed, StreamOverflow

A, B, BOT = 7_000_000_000_000_001, 7_000_000_000_000_002, 7_000_000_000_000_003
SSRC_A, SSRC_B, SSRC_BOT = 101, 102, 103

FORMATS = [
    songbird.PCM16_8K,
    songbird.PCM16_16K,
    songbird.PCM16_24K,
    songbird.PCM16_48K,
    songbird.PCM16_48K_STEREO,
    songbird.F32_48K,
    songbird.F32_48K_STEREO,
    songbird.MULAW_8K,
    songbird.ALAW_8K,
]


def mapped(vc: HarnessClient, testing: Any, *pairs: tuple[int, int]) -> None:
    for ssrc, user in pairs:
        testing.inject_map(vc._conn, ssrc, user)


def tick(
    vc: HarnessClient, testing: Any, voices: list[tuple[int, bytes | None]], silent: list[int] | None = None
) -> None:
    testing.inject_tick(vc._conn, voices, silent or [])
    vc.harness.step(20)


@pytest.mark.parametrize("fmt", FORMATS, ids=repr)
@pytest.mark.parametrize("chunk_ms", [10, 20, 100])
async def test_chunk_count_and_sizes(
    connected_vc: HarnessClient, testing: Any, fmt: AudioFormat, chunk_ms: int
) -> None:
    vc = connected_vc
    mapped(vc, testing, (SSRC_A, A))
    async with vc.listen_user(A, fmt, chunk_ms=chunk_ms, max_queue_ms=2_000) as stream:
        assert (stream.format, stream.chunk_ms, stream.user_id, stream.kind) == (fmt, chunk_ms, A, "user")
        inject_ticks(vc, testing, 10, ssrc=SSRC_A)
        chunks = drain(stream)
    assert len(chunks) == 10 * 20 // chunk_ms
    for i, c in enumerate(chunks):
        assert len(c) == fmt.bytes_for(chunk_ms)
        assert c.format == fmt
        assert c.frames == fmt.sample_rate * chunk_ms // 1000
        assert c.duration_ms == chunk_ms
        assert (c.seq, c.start_ms, c.end_ms) == (i, i * chunk_ms, (i + 1) * chunk_ms)
        assert c.timeline_ms == i * chunk_ms
        assert c.start_sample == c.start_ms * fmt.sample_rate // 1000
        assert c.user_id == A
        assert c.gap_ms == 0 and c.dropped_ms == 0


async def test_speaking_and_speakers(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    mapped(vc, testing, (SSRC_A, A), (SSRC_B, B))
    pkt = voice_packets(testing)[0]
    async with vc.listen_mixed(songbird.GPT_LIVE) as mixed, vc.listen_user(B, songbird.GPT_LIVE) as only_b:
        tick(vc, testing, [(SSRC_A, pkt), (SSRC_B, pkt)])
        tick(vc, testing, [(SSRC_A, pkt)])
        tick(vc, testing, [], [SSRC_A, SSRC_B])
        m = drain(mixed)
        b = drain(only_b)
    assert [c.speakers for c in m] == [frozenset({A, B}), frozenset({A}), frozenset()]
    assert [c.speaking for c in m] == [True, True, False]
    assert [c.user_id for c in m] == [None, None, None]
    assert [c.speaking for c in b] == [True, False, False]
    assert b[0].speakers == frozenset({B}) and b[1].speakers == frozenset()
    assert m[0].voiced_ms == 20 and m[2].voiced_ms == 0
    assert b[1].to_bytes() == bytes(len(b[1]))  # absent user: digital silence


async def test_absent_user_stream_is_silent_and_continuous(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    async with vc.listen_user(B, songbird.MULAW_8K) as stream:
        inject_ticks(vc, testing, 5, ssrc=SSRC_A)
        chunks = drain(stream)
    assert len(chunks) == 5
    assert all(c.to_bytes() == songbird.MULAW_8K.silence(20) for c in chunks)
    assert not any(c.speaking for c in chunks)


async def test_silence_is_synthesised_without_ticks(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    async with vc.listen_mixed(songbird.GPT_LIVE) as stream:
        vc.harness.step(1_000)
        chunks = drain(stream)
    assert 44 <= len(chunks) <= 50
    assert [c.timeline_ms for c in chunks] == [20 * i for i in range(len(chunks))]
    assert all(c.gap_ms == 20 and not c.speaking for c in chunks)
    assert vc.stats()["ticks_synthetic"] >= len(chunks)


async def test_skip_to_live_resets_the_clock(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    async with vc.listen_mixed(songbird.GPT_LIVE) as stream:
        inject_ticks(vc, testing, 5, voiced=False)
        assert stream.queued_ms == 100
        assert stream.skip_to_live() == 100
        assert stream.queued_ms == 0
        inject_ticks(vc, testing, 2, voiced=False)
        first, second = drain(stream)
        assert (first.seq, first.start_ms) == (0, 0)
        assert (second.seq, second.start_ms) == (1, 20)
        assert stream.origin_timeline_ms == first.timeline_ms == 100
        assert first.timeline_ms - first.start_ms == stream.origin_timeline_ms
        inject_ticks(vc, testing, 1, voiced=False)
        assert stream.skip_to_live(reset_clock=False) == 20
        inject_ticks(vc, testing, 1, voiced=False)
        (c,) = drain(stream)
        assert (c.seq, c.start_ms) == (3, 60)  # clock kept running


async def test_overflow_error_raises_once(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    stream = vc.listen_mixed(songbird.GPT_LIVE, max_queue_ms=40, overflow="error")
    inject_ticks(vc, testing, 5, voiced=False)
    got = []
    with pytest.raises(StreamOverflow):
        async for c in stream:
            got.append(c)
    assert len(got) == 2
    assert stream.closed and stream.close_reason == "overflow"
    assert stream.stats().overflowed
    with pytest.raises(StopAsyncIteration):
        await anext(stream)
    with pytest.raises(StreamClosed):
        await stream.read(timeout=0.1)


async def test_drop_oldest_reports_dropped_ms(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    async with vc.listen_mixed(songbird.GPT_LIVE, max_queue_ms=40) as stream:
        inject_ticks(vc, testing, 4, voiced=False)
        c2, c3 = drain(stream)
        assert (c2.seq, c2.dropped_ms) == (2, 40)
        assert (c3.seq, c3.dropped_ms) == (3, 0)
        st = stream.stats()
        assert (st.dropped_chunks, st.dropped_ms, st.delivered_chunks) == (2, 40, 2)
        assert stream.dropped_ms == 40


async def test_drop_newest_reports_dropped_ms(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    async with vc.listen_mixed(songbird.GPT_LIVE, max_queue_ms=40, overflow="drop_newest") as stream:
        inject_ticks(vc, testing, 4, voiced=False)
        c0, c1 = drain(stream)
        assert (c0.seq, c1.seq) == (0, 1)
        inject_ticks(vc, testing, 1, voiced=False)
        (c4,) = drain(stream)
        assert (c4.seq, c4.dropped_ms) == (4, 40)


async def test_async_iteration_and_read(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    stream = vc.listen_mixed(songbird.GPT_LIVE)
    inject_ticks(vc, testing, 3, voiced=False)
    got = [await stream.read(timeout=1), await anext(stream)]
    async for c in stream:
        got.append(c)
        break
    assert [c.seq for c in got] == [0, 1, 2]
    with pytest.raises(TimeoutError):
        await stream.read(timeout=0.05)
    stream.close()
    stream.close()  # idempotent
    with pytest.raises(StopAsyncIteration):
        await anext(stream)


async def test_waiting_reader_is_woken_by_the_pump(connected_vc: HarnessClient, testing: Any) -> None:
    import asyncio

    vc = connected_vc
    async with vc.listen_mixed(songbird.GPT_LIVE) as stream:
        reader = asyncio.create_task(stream.read(timeout=2))
        await pump_settle()
        assert not reader.done()  # armed, waiting on the wake id
        inject_ticks(vc, testing, 1, voiced=False)
        chunk = await reader
        assert chunk.seq == 0


async def test_set_include_rehomes_without_gap(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    mapped(vc, testing, (SSRC_A, A), (SSRC_B, B))
    pkt = voice_packets(testing)[0]
    async with vc.listen_mixed(songbird.GPT_LIVE) as stream:
        tick(vc, testing, [(SSRC_A, pkt), (SSRC_B, pkt)])
        stream.set_include([A])
        for _ in range(3):
            tick(vc, testing, [(SSRC_A, pkt), (SSRC_B, pkt)])
        stream.set_exclude([A])
        stream.set_include(None)
        for _ in range(3):
            tick(vc, testing, [(SSRC_A, pkt), (SSRC_B, pkt)])
        chunks = drain(stream)
    assert [c.seq for c in chunks] == list(range(7))
    assert chunks[0].speakers == frozenset({A, B})
    assert chunks[-1].speakers == frozenset({B})
    assert frozenset({A}) in [c.speakers for c in chunks[1:4]]
    with pytest.raises(ValueError):
        vc.listen_user(A).set_include([A])


async def test_floor_policy(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    mapped(vc, testing, (SSRC_A, A), (SSRC_B, B))
    pkt = voice_packets(testing)[0]
    async with vc.listen_mixed(songbird.GPT_LIVE, policy="floor") as stream:
        tick(vc, testing, [(SSRC_A, pkt)])
        for _ in range(3):
            tick(vc, testing, [(SSRC_A, pkt), (SSRC_B, pkt)])
        chunks = drain(stream)
    assert all(c.speakers == frozenset({A}) for c in chunks)


async def test_ignore_bots_after_classification(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    guild: Any = vc.guild  # the fake guild
    guild.members[BOT] = FakeUser(BOT, bot=True)
    mapped(vc, testing, (SSRC_A, A), (SSRC_BOT, BOT))
    vc.harness.step(20)  # UserMapped → Control.ClassifyUsers
    await pump_settle()  # the pump answers from the member cache
    pkt = voice_packets(testing)[0]
    async with vc.listen_mixed(songbird.GPT_LIVE) as stream:
        tick(vc, testing, [(SSRC_A, pkt), (SSRC_BOT, pkt)])
        (c,) = drain(stream)
    assert c.speakers == frozenset({A})


async def test_user_streams(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    mapped(vc, testing, (SSRC_A, A))
    pkt = voice_packets(testing)[0]
    async with vc.listen_users(songbird.PCM16_48K, chunk_ms=20) as users:
        tick(vc, testing, [(SSRC_A, pkt)])
        stream = await anext(users)
        assert (stream.user_id, stream.kind) == (A, "user")
        tick(vc, testing, [(SSRC_A, pkt)])
        testing.inject_user_left(vc._conn, A)
        vc.harness.step(20)
        chunks = [c async for c in stream]
        assert chunks and all(c.user_id == A for c in chunks)
        assert stream.close_reason == "user_left"
        assert users.dropped == 0
    with pytest.raises(StopAsyncIteration):
        await anext(users)


async def test_buffer_protocol(connected_vc: HarnessClient, testing: Any) -> None:
    np = pytest.importorskip("numpy")
    vc = connected_vc
    mapped(vc, testing, (SSRC_A, A))
    streams = {
        fmt.encoding: vc.listen_user(A, fmt) for fmt in (songbird.PCM16_48K, songbird.F32_48K, songbird.MULAW_8K)
    }
    inject_ticks(vc, testing, 1, ssrc=SSRC_A)
    chunks = {enc: drain(s)[0] for enc, s in streams.items()}
    pcm = chunks["pcm16le"]
    mv = memoryview(pcm)
    assert (mv.format, mv.itemsize, mv.readonly, mv.nbytes) == ("h", 2, True, len(pcm))
    assert mv.tolist() == list(np.frombuffer(pcm, "<i2"))
    assert np.shares_memory(np.frombuffer(pcm, "<i2"), memoryview(pcm))
    assert bytes(pcm) == pcm.to_bytes()
    assert pcm.to_base64() == base64.b64encode(bytes(pcm)).decode()
    assert memoryview(chunks["f32le"]).format == "f"
    assert memoryview(chunks["mulaw"]).format == "B"
    assert pcm.rms_dbfs > -60 and 0 < pcm.peak <= 1.0


async def test_chunk_outlives_its_stream(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    stream = vc.listen_mixed(songbird.GPT_LIVE)
    inject_ticks(vc, testing, 1)
    (chunk,) = drain(stream)
    view = memoryview(chunk)
    saved = bytes(view)
    stream.close()
    del stream
    gc.collect()
    vc.harness.step(40)
    assert bytes(view) == saved == chunk.to_bytes()


async def test_arrow_export(connected_vc: HarnessClient, testing: Any) -> None:
    pa = pytest.importorskip("pyarrow")
    np = pytest.importorskip("numpy")
    vc = connected_vc
    mapped(vc, testing, (SSRC_A, A))
    async with vc.listen_user(A, songbird.PCM16_48K) as stream:
        inject_ticks(vc, testing, 3, ssrc=SSRC_A)
        chunk = stream.read_nowait()
        assert chunk is not None
        arr = pa.array(chunk)
        assert arr.type == pa.int16()
        assert arr.to_pylist() == np.frombuffer(chunk, "<i2").tolist()
        batch = stream.read_batch(10)
        assert batch is not None and batch.num_rows == 2
        rb = batch.to_pyarrow()
        assert rb.num_rows == 2
        assert rb.column("seq").to_pylist() == [1, 2]
        assert stream.read_batch() is None


async def test_arro3_import(connected_vc: HarnessClient, testing: Any) -> None:
    arro3 = pytest.importorskip("arro3.core")
    vc = connected_vc
    async with vc.listen_mixed(songbird.F32_48K) as stream:
        inject_ticks(vc, testing, 1, voiced=False)
        chunk = stream.read_nowait()
    arr = arro3.Array.from_arrow(chunk)
    assert len(arr) == 960


async def test_full_rtp_path(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    mapped(vc, testing, (SSRC_A, A))
    opus = voice_packets(testing, 40)
    async with vc.listen_user(A, songbird.GPT_LIVE) as stream:
        for seq, (ext, pad, dave) in enumerate([(True, 4, True), (False, 0, True)]):
            pkt = testing.rtp_wrap(opus[seq], seq, SSRC_A, extension=ext, padding=pad, dave_trailer=dave)
            testing.inject_voice_tick_rtp(vc._conn, [(SSRC_A, pkt)])
            vc.harness.step(20)
        testing.inject_voice_tick_rtp(vc._conn, [(SSRC_A, b"\x00garbage")])
        vc.harness.step(20)
        chunks = drain(stream)
    assert [c.speaking for c in chunks] == [True, True, False]
    assert vc.stats()["bad_packets"] >= 1


async def test_receive_limits(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    with pytest.raises(songbird.LimitExceeded):
        vc.listen_mixed(chunk_ms=0)
    with pytest.raises(songbird.LimitExceeded):
        vc.listen_mixed(chunk_ms=1_001)
    with pytest.raises(songbird.LimitExceeded):
        vc.listen_mixed(max_queue_ms=10**15)
    with pytest.raises(songbird.LimitExceeded):
        vc.events(max_queue=0)
    with pytest.raises(ValueError):
        vc.listen_mixed(overflow="explode")  # type: ignore
    with pytest.raises(songbird.InvalidAudio):
        vc.listen_mixed("pcm16_11k")


async def test_receive_budget(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    streams = []
    with pytest.raises(songbird.LimitExceeded):
        for _ in range(64):
            streams.append(vc.listen_mixed(songbird.F32_48K_STEREO, chunk_ms=1_000, max_queue_ms=60_000))
    assert streams  # some fit before the 64 MiB budget ran out
    for s in streams:
        s.close()


async def test_streams_end_on_disconnect(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    stream = vc.listen_mixed(songbird.GPT_LIVE)
    inject_ticks(vc, testing, 2, voiced=False)
    await vc.disconnect()
    got = [c async for c in stream]  # queued chunks stay readable, then the stream ends
    assert len(got) == 2
    assert stream.closed and stream.close_reason == "connection_closed"
