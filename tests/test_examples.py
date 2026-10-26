"""Examples are importable and their bridges run against local fake servers (§9.10 rule 6, §13.3
``test_examples.py``)."""

from __future__ import annotations

import asyncio
import base64
import contextlib
import importlib.util
import json
import sys
import wave
from collections.abc import AsyncGenerator, Awaitable, Callable
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest
from conftest import ROOT, HarnessClient, World, connect_detached, pump_settle, tone, voice_packets, wait_for
from discord.ext import songbird

EXAMPLES = ["gpt_live", "openai_realtime", "gemini_live", "record", "music"]
SPEAKER, SPEAKER_SSRC = 7_000_000_000_000_001, 101


def load_example(name: str) -> ModuleType:
    path = ROOT / "examples" / f"{name}.py"
    spec = importlib.util.spec_from_file_location(f"songbird_example_{name}", path)
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = mod
    spec.loader.exec_module(mod)
    return mod


@pytest.mark.parametrize("name", EXAMPLES)
def test_examples_import_without_running(name: str, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("DISCORD_BOT_TOKEN", raising=False)
    mod = load_example(name)  # importing must neither raise KeyError nor start a bot
    assert hasattr(mod, "bot")


def b64_pcm(ms: int) -> str:
    return base64.b64encode(tone(ms, rate=24_000, channels=1)).decode()


@contextlib.asynccontextmanager
async def ws_server(handler: Callable[[Any], Awaitable[None]]) -> AsyncGenerator[str]:
    from websockets.asyncio.server import serve

    async with serve(handler, "127.0.0.1", 0) as server:
        port = server.sockets[0].getsockname()[1]
        yield f"ws://127.0.0.1:{port}"


class Clock:
    """Drives a detached connection in real time: 20 ms of manual time every few real ms, and
    optionally the playback's mixer harness."""

    def __init__(self, vc: HarnessClient) -> None:
        self.vc = vc
        self.mixers: list[Any] = []
        self.pulling = False
        self._task = asyncio.get_running_loop().create_task(self._run())

    async def _run(self) -> None:
        while True:
            conn = self.vc._conn
            if conn is not None and not conn.closed:
                self.vc.harness.step(20)
            if self.pulling:
                for m in self.mixers:
                    m.pull(1)
            await asyncio.sleep(0.002)

    async def stop(self) -> None:
        self._task.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await self._task


def playback_of(vc: HarnessClient) -> songbird.PlaybackStream | None:
    return next((c for c in list(vc._children) if isinstance(c, songbird.PlaybackStream)), None)


async def test_gpt_live_bridge(world: World, testing: Any) -> None:
    gpt_live = load_example("gpt_live")
    vc = await connect_detached(world)
    testing.inject_map(vc._conn, SPEAKER_SSRC, SPEAKER)
    log: dict[str, Any] = {"appends": []}
    deltas_sent, phase2, done = asyncio.Event(), asyncio.Event(), asyncio.Event()

    async def handler(ws: Any) -> None:
        log["auth"] = ws.request.headers.get("Authorization")
        log["start"] = json.loads(await ws.recv())
        log["appends_at_start"] = len(log["appends"])

        async def collect() -> None:
            with contextlib.suppress(Exception):
                async for raw in ws:
                    m = json.loads(raw)
                    if m["type"] == "session.input_audio.append":
                        log["appends"].append(len(base64.b64decode(m["audio"])))

        collector = asyncio.create_task(collect())
        delta = b64_pcm(40)
        # No handshake acknowledgement: deltas straight away, with a 500 ms gap in the middle.
        for i in range(15):
            await ws.send(json.dumps({"type": "session.output_audio.delta", "delta": delta, "start_ms": i * 40}))
        for i in range(15):
            await ws.send(
                json.dumps({"type": "session.output_audio.delta", "delta": delta, "start_ms": 1_100 + i * 40})
            )
        deltas_sent.set()
        await phase2.wait()
        # Contiguous with the interrupted utterance (rejected), then a new reply after a gap.
        await ws.send(json.dumps({"type": "session.output_audio.delta", "delta": delta, "start_ms": 1_700}))
        await ws.send(json.dumps({"type": "session.output_audio.delta", "delta": delta, "start_ms": 2_500}))
        await ws.send(json.dumps({"type": "session.output_transcript.delta", "delta": "hi"}))
        await done.wait()
        await ws.close()
        collector.cancel()

    clock = Clock(vc)
    try:
        async with ws_server(handler) as url:
            bridge = asyncio.create_task(gpt_live.gpt_live_bridge(vc, None, url=url, api_key="test"))
            await asyncio.wait_for(deltas_sent.wait(), 5)
            await wait_for(lambda: (pb := playback_of(vc)) is not None and pb.buffered_ms > 400, 5)
            speaker = playback_of(vc)
            assert speaker is not None
            clock.mixers.append(testing.MixerHarness(speaker._pb))
            assert speaker.stats().gap_filled_ms > 0  # start_ms gaps became silence
            # Local barge-in: a caller starts speaking while the model talks.
            pkt = voice_packets(testing)[0]
            testing.inject_tick(vc._conn, [(SPEAKER_SSRC, pkt)], [])
            await wait_for(lambda: speaker.buffered_ms == 0, 5)
            written = speaker.written_ms
            phase2.set()
            await wait_for(lambda: speaker.written_ms == written + 40, 5)
            assert speaker.stats().rejected_ms >= 40  # the contiguous rest was dropped
            await wait_for(lambda: len(log["appends"]) >= 5, 5)
            done.set()
            clock.pulling = True
            reason = await asyncio.wait_for(bridge, 10)  # a clean return: no ExceptionGroup
    finally:
        await clock.stop()
    assert reason == "websocket_closed"
    assert log["auth"] == "Bearer test"
    start = log["start"]
    assert start["type"] == "session.start"
    assert start["session"]["model"] == "gpt-live-1"
    assert start["session"]["audio"]["format"] == songbird.GPT_LIVE.openai_format
    assert log["appends"] and set(log["appends"]) == {songbird.GPT_LIVE.bytes_for(40)}  # continuous 40 ms
    await vc.disconnect()


async def test_openai_realtime_truncates_open_items(world: World, testing: Any) -> None:
    realtime = load_example("openai_realtime")
    vc = await connect_detached(world)
    log: dict[str, Any] = {}
    played, done = asyncio.Event(), asyncio.Event()

    async def handler(ws: Any) -> None:
        log["update"] = json.loads(await ws.recv())
        delta = b64_pcm(40)
        for _ in range(10):
            await ws.send(json.dumps({"type": "response.output_audio.delta", "item_id": "item_1", "delta": delta}))
        await played.wait()
        await ws.send(json.dumps({"type": "input_audio_buffer.speech_started"}))
        async for raw in ws:
            m = json.loads(raw)
            if m["type"] == "conversation.item.truncate":
                log["truncate"] = m
                break
        await ws.send(json.dumps({"type": "response.output_audio.delta", "item_id": "item_1", "delta": delta}))
        await done.wait()
        await ws.close()

    clock = Clock(vc)
    try:
        async with ws_server(handler) as url:
            bridge = asyncio.create_task(realtime.realtime_bridge(vc, url=url, api_key="test"))
            await wait_for(lambda: (pb := playback_of(vc)) is not None and pb.written_ms == 400, 5)
            speaker = playback_of(vc)
            assert speaker is not None
            mixer = testing.MixerHarness(speaker._pb)
            mixer.pull(3)
            vc.harness.step(100)
            played.set()
            await wait_for(lambda: "truncate" in log, 5)
            await wait_for(lambda: speaker.stats().rejected_ms >= 40, 5)  # the late delta was dropped
            done.set()
            clock.mixers.append(mixer)
            clock.pulling = True
            reason = await asyncio.wait_for(bridge, 10)
    finally:
        await clock.stop()
    assert reason == "websocket_closed"
    assert log["update"]["type"] == "session.update"
    t = log["truncate"]
    assert (t["item_id"], t["content_index"]) == ("item_1", 0)
    assert 0 < t["audio_end_ms"] <= 400
    await vc.disconnect()


async def test_record_returns_when_the_connection_closes(world: World, testing: Any, tmp_path: Path) -> None:
    record = load_example("record")
    vc = await connect_detached(world)
    testing.inject_map(vc._conn, SPEAKER_SSRC, SPEAKER)
    out = tmp_path / "rec"
    task = asyncio.create_task(record.record(vc, out, asyncio.Event()))  # no !stop will come
    await pump_settle()
    pkt = voice_packets(testing)[0]
    for _ in range(30):
        testing.inject_tick(vc._conn, [(SPEAKER_SSRC, pkt)], [])
        vc.harness.step(20)
        await asyncio.sleep(0)
    await pump_settle()
    await wait_for(lambda: (out / f"{SPEAKER}.wav").exists(), 5)
    await vc.disconnect()
    await asyncio.wait_for(task, 5)
    with wave.open(str(out / f"{SPEAKER}.wav"), "rb") as w:
        assert (w.getframerate(), w.getnchannels(), w.getsampwidth()) == (48_000, 1, 2)
        assert w.getnframes() >= 48_000 * 200 // 1000
    events = (out / "events.jsonl").read_text().splitlines()
    assert any(json.loads(line)["event"] == "SpeakingStarted" for line in events)


async def test_record_stops_on_request(world: World, testing: Any, tmp_path: Path) -> None:
    record = load_example("record")
    vc = await connect_detached(world)
    stop = asyncio.Event()
    task = asyncio.create_task(record.record(vc, tmp_path / "rec", stop))
    await pump_settle()
    stop.set()
    await asyncio.wait_for(task, 5)
    assert vc.is_connected()
    await vc.disconnect()
