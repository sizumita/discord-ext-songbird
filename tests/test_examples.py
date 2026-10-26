"""Examples are importable and run against the detached harness and a local fake WebSocket server
(§9.10 rule 6, §13.3 ``test_examples.py``)."""

from __future__ import annotations

import asyncio
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
from conftest import ROOT, HarnessClient, World, connect_detached, pump_settle, voice_packets, wait_for
from discord.ext import songbird

EXAMPLES = ["echo", "websocket_bridge", "record", "music"]
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


async def test_echo_plays_back_voiced_audio(world: World, testing: Any) -> None:
    echo = load_example("echo")
    vc = await connect_detached(world)
    testing.inject_map(vc._conn, SPEAKER_SSRC, SPEAKER)
    stop = asyncio.Event()
    task = asyncio.create_task(echo.echo(vc, SPEAKER, stop, delay_ms=100))
    await wait_for(lambda: playback_of(vc) is not None, 5)
    speaker = playback_of(vc)
    assert speaker is not None and speaker.format == songbird.PCM16_48K_STEREO
    pkt = voice_packets(testing)[0]
    for _ in range(10):  # 200 ms of speech
        testing.inject_tick(vc._conn, [(SPEAKER_SSRC, pkt)], [])
        vc.harness.step(20)
        await asyncio.sleep(0)
    vc.harness.step(200)  # then 200 ms of silence: nothing more is written
    await wait_for(lambda: speaker.written_ms >= 200, 5)
    clock = Clock(vc)
    try:
        clock.mixers.append(testing.MixerHarness(speaker._pb))
        clock.pulling = True
        await wait_for(lambda: speaker.played_ms > 0, 5)
        stop.set()
        echoed = await asyncio.wait_for(task, 10)
    finally:
        await clock.stop()
    assert 200 <= echoed < 1_000  # voiced chunks only, not the silence fill
    await vc.disconnect()


async def test_echo_returns_when_the_connection_closes(world: World) -> None:
    echo = load_example("echo")
    vc = await connect_detached(world)
    task = asyncio.create_task(echo.echo(vc, SPEAKER, asyncio.Event()))  # no !stop will come
    await pump_settle()
    await vc.disconnect()
    assert await asyncio.wait_for(task, 5) == 0


async def test_websocket_bridge_streams_binary_frames(world: World, testing: Any) -> None:
    ws_bridge = load_example("websocket_bridge")
    fmt = songbird.PCM16_16K
    vc = await connect_detached(world)
    log: dict[str, Any] = {"frames": [], "text": []}
    cleared, done = asyncio.Event(), asyncio.Event()

    async def echo_server(ws: Any) -> None:
        """A fake service: echoes every binary frame, interrupts after ten, closes when told."""

        async def finish() -> None:
            await done.wait()
            await ws.close()

        closer = asyncio.create_task(finish())
        with contextlib.suppress(Exception):
            async for message in ws:
                if isinstance(message, str):
                    log["text"].append(message)
                    continue
                log["frames"].append(len(message))
                await ws.send(message)
                if len(log["frames"]) == 10:
                    await ws.send("clear")
                    cleared.set()
        await closer

    clock = Clock(vc)
    try:
        async with ws_server(echo_server) as url:
            task = asyncio.create_task(ws_bridge.bridge(vc, url, input_format=fmt, chunk_ms=20))
            await asyncio.wait_for(cleared.wait(), 5)
            speaker = playback_of(vc)
            assert speaker is not None and speaker.format == fmt
            await wait_for(lambda: speaker.stats().clears == 1, 5)
            assert speaker.stats().discarded_ms > 0  # the echoed audio queued before "clear"
            written = speaker.written_ms
            await wait_for(lambda: speaker.written_ms > written, 5)  # echoes keep playing after it
            clock.mixers.append(testing.MixerHarness(speaker._pb))
            clock.pulling = True
            done.set()
            reason = await asyncio.wait_for(task, 10)
    finally:
        await clock.stop()
    assert reason == "websocket_closed"
    assert log["text"] == []  # audio only: one binary frame per chunk
    assert len(log["frames"]) >= 10 and set(log["frames"]) == {fmt.bytes_for(20)}
    await vc.disconnect()


async def test_websocket_bridge_stops_on_request(world: World) -> None:
    ws_bridge = load_example("websocket_bridge")
    vc = await connect_detached(world)
    stop = asyncio.Event()

    async def silent_server(ws: Any) -> None:
        with contextlib.suppress(Exception):
            async for _ in ws:
                pass

    clock = Clock(vc)
    try:
        async with ws_server(silent_server) as url:
            task = asyncio.create_task(ws_bridge.bridge(vc, url, stop=stop))
            await wait_for(lambda: playback_of(vc) is not None, 5)
            stop.set()
            assert await asyncio.wait_for(task, 5) == "stopped"
    finally:
        await clock.stop()
    assert vc.is_connected()
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


def test_music_paths_stay_inside_the_music_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    mod = load_example("music")
    monkeypatch.setattr(mod, "MUSIC_DIR", tmp_path)
    (tmp_path / "song.wav").write_bytes(b"")
    (tmp_path / "sub").mkdir()
    assert mod.music_path("song.wav") == (tmp_path / "song.wav").resolve()
    assert mod.music_path("sub/../song.wav") == (tmp_path / "song.wav").resolve()
    assert mod.music_path("../outside.wav") is None
    assert mod.music_path("/etc/passwd") is None
    assert mod.music_path("") is None
