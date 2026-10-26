"""Memory sources, track handles and the queue (§7.9, §7.10, §9.8, §13.3 ``test_sources.py``)."""

from __future__ import annotations

import asyncio
import io
import time
import wave
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any

import pytest
from conftest import HarnessClient, pump_settle, tone
from discord.ext import songbird
from discord.ext.songbird import (
    AudioFile,
    InvalidAudio,
    LimitExceeded,
    OpusPackets,
    RawPcm,
    SourceError,
    TrackEnd,
    TrackError,
    TrackHandle,
    load_audio_file,
)

SILENT_FRAME = b"\xf8\xff\xfe"
TEN_MS_FRAME = b"\xf0\xff\xfe"  # CELT FB 10 ms: 480 samples


def wav_bytes(ms: int = 200, rate: int = 24_000) -> bytes:
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(tone(ms, rate=rate, channels=1))
    return buf.getvalue()


def test_audio_file_probe() -> None:
    f = AudioFile(wav_bytes(200))
    assert f.nbytes == len(wav_bytes(200))
    assert isinstance(f.codec, str) and f.codec
    assert (f.sample_rate, f.channels) == (24_000, 1)
    assert f.duration_ms == 200
    assert AudioFile(bytearray(wav_bytes(20))).duration_ms == 20
    with pytest.raises(SourceError):
        AudioFile(b"definitely not audio" * 10)


def test_audio_file_from_path(tmp_path: Path) -> None:
    p = tmp_path / "a.wav"
    p.write_bytes(wav_bytes(100))
    assert AudioFile.from_path(p).duration_ms == 100
    assert AudioFile.from_path(str(p)).duration_ms == 100
    with pytest.raises(FileNotFoundError):
        AudioFile.from_path(tmp_path / "missing.wav")
    with pytest.raises(LimitExceeded):
        AudioFile.from_path(p, max_bytes=16)


async def test_load_audio_file(tmp_path: Path) -> None:
    p = tmp_path / "a.wav"
    p.write_bytes(wav_bytes(60))
    f = await load_audio_file(p)
    assert f.duration_ms == 60


def test_audio_file_from_arrow() -> None:
    pa = pytest.importorskip("pyarrow")
    data = wav_bytes(40)
    assert AudioFile.from_arrow(pa.array(list(data), pa.uint8())).duration_ms == 40
    assert AudioFile.from_arrow(pa.array([data], pa.binary())).duration_ms == 40
    with pytest.raises(InvalidAudio):
        AudioFile.from_arrow(pa.array([data, data], pa.binary()))


def test_raw_pcm() -> None:
    src = RawPcm(tone(100, rate=24_000, channels=1), songbird.PCM16_24K)
    assert src.format == songbird.PCM16_24K and src.duration_ms == 100
    assert RawPcm(bytes(800), "mulaw").duration_ms == 100
    with pytest.raises(TypeError):
        RawPcm("not bytes", songbird.PCM16_24K)  # type: ignore


def test_raw_pcm_from_samples_any_rate() -> None:
    np = pytest.importorskip("numpy")
    samples = np.zeros(44_100 * 2, np.float32)  # 1 s of 44.1 kHz stereo
    src = RawPcm.from_samples(samples, 44_100, 2)
    assert src.format.sample_rate == 48_000
    assert abs(src.duration_ms - 1_000) <= 1
    assert RawPcm.from_samples(np.zeros(1_600, np.int16), 16_000, 1, "pcm16le").duration_ms == 100
    for rate in (7_999, 192_001):
        with pytest.raises(LimitExceeded):
            RawPcm.from_samples(b"", rate, 2)
    pa = pytest.importorskip("pyarrow")
    arr = pa.array(np.zeros(4_800, np.float32))
    assert RawPcm.from_samples(arr, 48_000, 1).duration_ms == 100


def test_opus_packets() -> None:
    p = OpusPackets([SILENT_FRAME] * 5)
    assert len(p) == 5 and p.duration_ms == 100
    with pytest.raises(InvalidAudio):
        OpusPackets([TEN_MS_FRAME])
    with pytest.raises(InvalidAudio):
        OpusPackets([b""])
    with pytest.raises(InvalidAudio):
        OpusPackets([b"\xf8" * 1_276])
    pa = pytest.importorskip("pyarrow")
    assert len(OpusPackets.from_arrow(pa.array([SILENT_FRAME] * 3, pa.binary()))) == 3


async def test_play_and_stop(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    t = vc.play(RawPcm(bytes(4_800), songbird.PCM16_24K), volume=0.5)
    assert isinstance(t, TrackHandle)
    assert (t.kind, t.ended, t.end_reason) == ("pcm", False, None)
    waiter = asyncio.create_task(t.wait_ended())
    t.stop()
    vc.harness.step(60)
    end = await asyncio.wait_for(waiter, 2)
    assert end == TrackEnd("stopped", None)
    assert t.ended and t.end_reason == "stopped"
    t2 = vc.play(AudioFile(wav_bytes()), loops=None, paused=True)
    assert t2.kind == "file"
    t2.set_volume(2.0)
    t2.set_loops(3)
    t2.pause()
    t2.resume()
    with pytest.raises(LimitExceeded):
        t2.set_volume(5.0)
    assert t2 != t and t2 == t2 and len({t, t2}) == 2


async def test_wait_ended_after_stop_all_and_close(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    a = vc.play(OpusPackets([SILENT_FRAME] * 50))
    pb = vc.open_playback()
    stop_all = asyncio.gather(a.wait_ended(), pb.track.wait_ended())
    vc.stop_all()
    vc.harness.step(60)
    ends = await asyncio.wait_for(stop_all, 2)
    assert {e.reason for e in ends} == {"stopped"}
    assert pb.closed
    b = vc.play(OpusPackets([SILENT_FRAME] * 50))
    waiter = asyncio.create_task(b.wait_ended())
    await pump_settle()
    assert vc._conn is not None
    vc._conn.close("requested")
    end = await asyncio.wait_for(waiter, 2)
    assert end.reason == "disconnected"


async def test_wait_ended_is_level_triggered(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    src = OpusPackets([SILENT_FRAME])
    handles = [vc.enqueue(src) for _ in range(2_000)]
    waiters = [asyncio.create_task(h.wait_ended()) for h in handles]
    await pump_settle()
    vc.queue.stop()
    vc.harness.step(60)
    time.sleep(0.2)  # a loop stall: every completion is already resolved when the pump runs
    ends = await asyncio.wait_for(asyncio.gather(*waiters), 5)
    assert {e.reason for e in ends} == {"stopped"}


async def test_seek_validation(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    t = vc.play(OpusPackets([SILENT_FRAME] * 10))
    for bad in (-1.0, float("nan"), float("inf"), 1e300):
        with pytest.raises(LimitExceeded):
            await t.seek(bad)
    with pytest.raises(LimitExceeded):
        await t.seek(1.0, timeout=-1)
    pb = vc.open_playback()
    with pytest.raises(TrackError):
        await pb.track.seek(1.0)
    with pytest.raises(TrackError):
        pb.track.set_loops(2)


@pytest.mark.fake_only
async def test_track_ops(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    t = vc.play(OpusPackets([SILENT_FRAME] * 10), volume=0.5)
    op = asyncio.create_task(t.info())
    await pump_settle()
    vc.harness.step(20)
    info = await asyncio.wait_for(op, 2)
    assert info.volume == 0.5 and info.playing == "play"
    seek = asyncio.create_task(t.seek(0.1))
    await pump_settle()
    vc.harness.step(20)
    assert await asyncio.wait_for(seek, 2) == pytest.approx(0.1)


async def test_queue(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    q = vc.queue
    assert q.current is None and len(q) == 0 and not q
    src = [OpusPackets([SILENT_FRAME] * (i + 1)) for i in range(4)]
    handles = [vc.enqueue(s) for s in src]
    vc.harness.step(20)
    assert q.current == handles[0]
    assert len(q) == 3 and list(q) == handles[1:]
    assert q[0] == handles[1] and q[-1] == handles[3]
    with pytest.raises(IndexError):
        _ = q[3]
    with pytest.raises(IndexError):
        q.dequeue(5)
    removed = q.dequeue(1)
    assert removed == handles[2]
    vc.harness.step(20)
    assert removed.end_reason == "stopped"
    q.skip()
    vc.harness.step(60)
    assert handles[0].end_reason == "stopped"
    assert q.current == handles[1]
    assert list(q) == [handles[3]]
    q.clear()
    vc.harness.step(20)
    assert len(q) == 0 and handles[3].end_reason == "stopped"
    assert q.current == handles[1]  # the current track keeps playing
    q.stop()
    vc.harness.step(60)
    assert q.current is None and handles[1].ended


async def test_queue_pending_entry_control(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    first = vc.enqueue(OpusPackets([SILENT_FRAME]))
    pending = vc.enqueue(OpusPackets([SILENT_FRAME]), volume=0.5, loops=2, paused=True)
    vc.harness.step(20)
    assert pending.pending and not first.pending
    pending.set_volume(1.5)
    pending.set_loops(None)
    pending.resume()
    with pytest.raises(TrackError):
        await pending.seek(1.0)
    with pytest.raises(TrackError):
        await pending.info()
    pending.stop()  # removes it from the queue
    vc.harness.step(20)
    assert pending.end_reason == "stopped"
    assert len(vc.queue) == 0


async def test_queue_paused_does_not_advance(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    a = vc.enqueue(OpusPackets([SILENT_FRAME]))
    b = vc.enqueue(OpusPackets([SILENT_FRAME]))
    vc.harness.step(20)
    vc.queue.pause()
    a.stop()
    vc.harness.step(60)
    assert vc.queue.current is None or vc.queue.current != b
    vc.queue.resume()
    vc.harness.step(60)
    assert vc.queue.current == b


async def test_enqueue_stream_is_fed_only_when_current(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    fed = asyncio.Event()

    async def feed() -> AsyncIterator[bytes]:
        fed.set()
        yield wav_bytes(40)

    first = vc.enqueue(OpusPackets([SILENT_FRAME]))
    stream = vc.queue.enqueue_stream(feed())
    assert stream.kind == "encoded_stream"
    vc.harness.step(20)
    await pump_settle()
    assert not fed.is_set()  # pending: no buffer, no thread, not fed
    first.stop()
    for _ in range(5):
        vc.harness.step(20)
        await pump_settle()
    await asyncio.wait_for(fed.wait(), 2)
    assert not stream.pending  # it became the current entry (it may already have finished)


async def test_play_limits(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    src = OpusPackets([SILENT_FRAME])
    tracks = [vc.play(src) for _ in range(32)]
    with pytest.raises(LimitExceeded):
        vc.play(src)
    for t in tracks:
        t.stop()
    with pytest.raises(TypeError):
        vc.play(b"raw bytes")  # type: ignore


async def test_encoded_stream_plumbing(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    stream = vc.open_encoded_stream(max_buffer_bytes=64 << 10)
    data = wav_bytes(100)

    async def chunks() -> AsyncIterator[bytes]:
        yield data[:1_000]
        yield data[1_000:]

    assert await stream.feed_from(chunks(), chunk_size=512) == len(data)
    assert stream.track.kind == "encoded_stream"
    stream.close()
    with pytest.raises(songbird.StreamClosed):
        stream.write_nowait(b"x")


async def test_opus_stream_plumbing(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    async with vc.open_opus_stream(max_packets=2) as stream:
        assert testing.track_log(stream.track._native) == []
        stream.send_nowait(SILENT_FRAME)
        assert testing.track_log(stream.track._native) == ["play"]  # the first send resumes it
        await stream.send(SILENT_FRAME)
        assert stream.buffered_ms == 40
        with pytest.raises(songbird.PlaybackFull):
            stream.send_nowait(SILENT_FRAME)
        with pytest.raises(InvalidAudio):
            stream.send_nowait(TEN_MS_FRAME)
        assert stream.clear() == 40
        stream.close()


async def test_queue_feed_ends_quietly_when_the_connection_closes(
    connected_vc: HarnessClient, caplog: pytest.LogCaptureFixture
) -> None:
    vc = connected_vc
    blocked = asyncio.Event()

    async def endless() -> AsyncIterator[bytes]:
        yield wav_bytes(40)
        blocked.set()
        await asyncio.Event().wait()  # an upstream that never sends again
        yield b""

    vc.queue.enqueue_stream(endless())
    for _ in range(3):
        vc.harness.step(20)
        await pump_settle()
    await asyncio.wait_for(blocked.wait(), 2)
    feeds = set(vc.queue._tasks)
    assert feeds
    with caplog.at_level("ERROR", logger="discord.ext.songbird"):
        await vc.disconnect()
        await asyncio.sleep(0.01)
    assert all(t.done() for t in feeds)  # the feed waiting on its source was cancelled
    assert not [r for r in caplog.records if r.levelname == "ERROR"], caplog.records
