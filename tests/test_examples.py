"""Examples are importable and run against the detached harness and a local fake WebSocket server
(§9.10 rule 6, §13.3 ``test_examples.py``)."""

from __future__ import annotations

import asyncio
import contextlib
import importlib.util
import json
import sys
import time
import wave
from collections.abc import AsyncGenerator, Awaitable, Callable
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest
from conftest import ROOT, HarnessClient, World, connect_detached, pump_settle, voice_packets, wait_for
from discord.ext import songbird

EXAMPLES = ["echo", "websocket_bridge", "record", "music", "lfm2_audio/stt", "lfm2_audio/tts", "lfm2_audio/voice_chat"]
SPEAKER, SPEAKER_SSRC = 7_000_000_000_000_001, 101


def load_example(name: str) -> ModuleType:
    path = ROOT / "examples" / f"{name}.py"
    spec = importlib.util.spec_from_file_location(f"songbird_example_{name.replace('/', '_')}", path)
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


async def test_echo_returns_when_the_connection_closes_mid_speech(world: World, testing: Any) -> None:
    """Voiced chunks still queued when the connection closes are delivered, but the playback is
    already closed: echo() must end cleanly instead of raising StreamClosed."""
    echo = load_example("echo")
    vc = await connect_detached(world)
    testing.inject_map(vc._conn, SPEAKER_SSRC, SPEAKER)
    task = asyncio.create_task(echo.echo(vc, SPEAKER, asyncio.Event(), delay_ms=100))
    await wait_for(lambda: playback_of(vc) is not None, 5)
    pkt = voice_packets(testing)[0]
    for _ in range(10):  # queued, not yet read: no await between the ticks and the close
        testing.inject_tick(vc._conn, [(SPEAKER_SSRC, pkt)], [])
        vc.harness.step(20)
    await vc.disconnect()
    echoed = await asyncio.wait_for(task, 5)
    assert 0 <= echoed <= 200


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


@pytest.mark.parametrize("attempt", range(4))
async def test_websocket_bridge_reports_voice_closed_while_audio_arrives(world: World, attempt: int) -> None:
    """The service keeps sending audio when the voice connection closes: the bridge returns
    ``"voice_closed"`` every time (no StreamClosed from the downlink, no arbitrary pick)."""
    ws_bridge = load_example("websocket_bridge")
    fmt = songbird.PCM16_16K
    vc = await connect_detached(world)
    frame = fmt.silence(20)

    async def chatty_server(ws: Any) -> None:
        with contextlib.suppress(Exception):
            while True:
                await ws.send(frame)
                await asyncio.sleep(0.002)

    clock = Clock(vc)
    try:
        async with ws_server(chatty_server) as url:
            task = asyncio.create_task(ws_bridge.bridge(vc, url, input_format=fmt))
            await wait_for(lambda: (pb := playback_of(vc)) is not None and pb.written_ms > 0, 5)
            await vc.disconnect()
            assert await asyncio.wait_for(task, 5) == "voice_closed"
    finally:
        await clock.stop()


async def test_websocket_bridge_downlink_ends_cleanly_when_playback_closes(world: World) -> None:
    """Deterministic version of the race above: audio arrives for a playback that is already closed
    (as after a voice disconnect); the downlink reports it instead of raising StreamClosed."""
    ws_bridge = load_example("websocket_bridge")
    fmt = songbird.PCM16_16K
    vc = await connect_detached(world)
    frame = fmt.silence(20)

    async def chatty_server(ws: Any) -> None:
        with contextlib.suppress(Exception):
            while True:
                await ws.send(frame)
                await asyncio.sleep(0.002)

    clock = Clock(vc)
    try:
        async with ws_server(chatty_server) as url:
            task = asyncio.create_task(ws_bridge.bridge(vc, url, input_format=fmt))
            await wait_for(lambda: (pb := playback_of(vc)) is not None and pb.written_ms > 0, 5)
            speaker = playback_of(vc)
            assert speaker is not None
            speaker.close()
            assert await asyncio.wait_for(task, 5) == "voice_closed"
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


# ── examples/lfm2_audio: the bot logic with a fake model in place of LFM2.5-Audio ─────────────────


async def drive(vc: HarnessClient, testing: Any, ms: int, *, voiced: bool, mixer: Any | None = None) -> None:
    """``ms`` of manual time in 20 ms ticks, the speaker talking or not; ``mixer`` plays meanwhile."""
    pkt = voice_packets(testing)[0]
    for _ in range(ms // 20):
        if voiced:
            testing.inject_tick(vc._conn, [(SPEAKER_SSRC, pkt)], [])
        vc.harness.step(20)
        if mixer is not None:
            mixer.pull(1)
        await asyncio.sleep(0.001)


class FakeTranscriber:
    def __init__(self) -> None:
        self.sizes: list[int] = []

    def transcribe(self, pcm: bytearray) -> str:
        self.sizes.append(len(pcm))
        return f"utterance {len(self.sizes)}"


async def test_lfm2_stt_posts_one_transcript_per_utterance(world: World, testing: Any) -> None:
    stt = load_example("lfm2_audio/stt")
    vc = await connect_detached(world)
    testing.inject_map(vc._conn, SPEAKER_SSRC, SPEAKER)
    model, posts, stop = FakeTranscriber(), [], asyncio.Event()

    async def post(user_id: int, text: str) -> None:
        posts.append((user_id, text))

    task = asyncio.create_task(stt.transcribe(vc, model, post, stop))
    await pump_settle()
    await drive(vc, testing, 500, voiced=True)
    await drive(vc, testing, stt.END_SILENCE_MS + 400, voiced=False)  # hangover + end of utterance
    await wait_for(lambda: len(posts) == 1, 5)
    await drive(vc, testing, 200, voiced=True)  # a blip shorter than MIN_SPEECH_MS
    await drive(vc, testing, stt.END_SILENCE_MS + 400, voiced=False)
    stop.set()
    assert await asyncio.wait_for(task, 5) == 1
    assert posts == [(SPEAKER, "utterance 1")]
    assert stt.IN_FMT.bytes_for(500) <= model.sizes[0] <= stt.IN_FMT.bytes_for(500 + stt.END_SILENCE_MS + 400)
    assert vc.is_connected()
    await vc.disconnect()


async def test_lfm2_stt_transcribes_the_last_utterance_when_the_connection_closes(world: World, testing: Any) -> None:
    stt = load_example("lfm2_audio/stt")
    vc = await connect_detached(world)
    testing.inject_map(vc._conn, SPEAKER_SSRC, SPEAKER)
    model, posts = FakeTranscriber(), []

    async def post(user_id: int, text: str) -> None:
        posts.append((user_id, text))

    task = asyncio.create_task(stt.transcribe(vc, model, post, asyncio.Event()))  # no !stop will come
    await pump_settle()
    await drive(vc, testing, 500, voiced=True)
    await pump_settle()
    await vc.disconnect()  # mid-utterance
    assert await asyncio.wait_for(task, 5) == 1
    assert posts == [(SPEAKER, "utterance 1")]


class FakeSynthesizer:
    """80 ms frames of silence: ``frames`` per message, ``delay`` seconds apart (on the worker thread)."""

    def __init__(self, fmt: songbird.AudioFormat, frames: int = 5, delay: float = 0.0) -> None:
        self.fmt, self.frames, self.delay = fmt, frames, delay
        self.made: dict[str, int] = {}

    def synthesize(self, text: str) -> Any:
        self.made[text] = 0
        for _ in range(self.frames):
            if self.delay:
                time.sleep(self.delay)
            self.made[text] += 1
            yield self.fmt.silence(80)


async def test_lfm2_tts_reads_messages_one_after_another(world: World, testing: Any) -> None:
    tts = load_example("lfm2_audio/tts")
    vc = await connect_detached(world)
    model = FakeSynthesizer(tts.OUT_FMT)
    reader = tts.Reader(vc, model)
    task = asyncio.create_task(reader.run())
    assert reader.say("一通目") and reader.say("二通目")
    await wait_for(lambda: (pb := playback_of(vc)) is not None and pb.written_ms >= 400, 5)
    speaker = playback_of(vc)
    assert speaker is not None and speaker.format == tts.OUT_FMT
    assert list(model.made) == ["一通目"]  # the next message waits until this one has played
    clock = Clock(vc)
    try:
        clock.mixers.append(testing.MixerHarness(speaker._pb))
        clock.pulling = True
        await wait_for(lambda: speaker.written_ms >= 800, 10)
        assert list(model.made) == ["一通目", "二通目"]
        reader.stop()
        assert await asyncio.wait_for(task, 5) == 2
    finally:
        await clock.stop()
    assert not reader.say("もう遅い")
    await vc.disconnect()


async def test_lfm2_tts_skip_cuts_the_message_and_stops_its_generation(world: World, testing: Any) -> None:
    tts = load_example("lfm2_audio/tts")
    vc = await connect_detached(world)
    model = FakeSynthesizer(tts.OUT_FMT, frames=200, delay=0.005)
    reader = tts.Reader(vc, model)
    task = asyncio.create_task(reader.run())
    reader.say("長い文")
    await wait_for(lambda: (pb := playback_of(vc)) is not None and pb.written_ms >= 400, 5)
    speaker = playback_of(vc)
    assert speaker is not None
    reader.skip()
    assert speaker.stats().clears == 1
    made = model.made["長い文"]
    await asyncio.sleep(0.1)
    assert model.made["長い文"] <= made + 1 < 200  # the worker stopped after its current frame
    model.frames, model.delay = 5, 0.0
    reader.say("次")
    clock = Clock(vc)
    try:
        clock.mixers.append(testing.MixerHarness(speaker._pb))
        clock.pulling = True
        await wait_for(lambda: model.made.get("次") == 5, 5)
    finally:
        await clock.stop()
    await vc.disconnect()
    assert await asyncio.wait_for(task, 5) == 2  # the connection closed: run() returns


class FakeChatModel:
    """Answers with ``frames`` (text id, audio frame) pairs; records the history it was given."""

    def __init__(self, fmt: songbird.AudioFormat, frames: int = 10) -> None:
        self.fmt, self.frames = fmt, frames
        self.histories: list[list[int]] = []  # tokens per history turn, for every answer

    def respond(self, history: list[Any], pcm: bytearray) -> Any:
        assert pcm
        self.histories.append([len(turn.tokens) for turn in history])
        for i in range(self.frames):
            yield i, None
            yield ("frame", i), self.fmt.silence(80)

    def text(self, turn: Any) -> str:
        return f"{sum(isinstance(t, int) for t in turn.tokens)} words"


async def test_lfm2_voice_chat_answers_each_utterance_with_history(world: World, testing: Any) -> None:
    chat = load_example("lfm2_audio/voice_chat")
    vc = await connect_detached(world)
    testing.inject_map(vc._conn, SPEAKER_SSRC, SPEAKER)
    model, posts, stop = FakeChatModel(chat.OUT_FMT), [], asyncio.Event()

    async def post(text: str) -> None:
        posts.append(text)

    task = asyncio.create_task(chat.converse(vc, model, post, stop))
    await wait_for(lambda: playback_of(vc) is not None, 5)
    speaker = playback_of(vc)
    assert speaker is not None and speaker.format == chat.OUT_FMT
    mixer = testing.MixerHarness(speaker._pb)
    await drive(vc, testing, 500, voiced=True)
    await drive(vc, testing, chat.END_SILENCE_MS + 400, voiced=False)
    await wait_for(lambda: posts == ["10 words"], 5)
    await drive(vc, testing, 1_200, voiced=False, mixer=mixer)  # the answer (800 ms) plays out
    await drive(vc, testing, 500, voiced=True, mixer=mixer)
    await drive(vc, testing, chat.END_SILENCE_MS + 400, voiced=False, mixer=mixer)
    await wait_for(lambda: len(posts) == 2, 5)
    stop.set()
    assert await asyncio.wait_for(task, 5) == 2
    assert model.histories == [[], [20]]  # the second answer saw the whole first exchange
    assert speaker.stats().clears == 0
    await vc.disconnect()


async def test_lfm2_voice_chat_barge_in_keeps_only_what_was_heard(world: World, testing: Any) -> None:
    chat = load_example("lfm2_audio/voice_chat")
    vc = await connect_detached(world)
    testing.inject_map(vc._conn, SPEAKER_SSRC, SPEAKER)
    model, posts = FakeChatModel(chat.OUT_FMT, frames=100), []  # an 8 s answer

    async def post(text: str) -> None:
        posts.append(text)

    task = asyncio.create_task(chat.converse(vc, model, post, asyncio.Event()))
    await wait_for(lambda: playback_of(vc) is not None, 5)
    speaker = playback_of(vc)
    assert speaker is not None
    mixer = testing.MixerHarness(speaker._pb)
    await drive(vc, testing, 500, voiced=True)
    await drive(vc, testing, chat.END_SILENCE_MS + 400, voiced=False)
    await wait_for(lambda: len(posts) == 1, 5)
    await drive(vc, testing, 1_000, voiced=False, mixer=mixer)  # part of the answer plays
    await drive(vc, testing, 500, voiced=True, mixer=mixer)  # then the user talks over it
    assert speaker.stats().clears == 1
    await drive(vc, testing, chat.END_SILENCE_MS + 400, voiced=False, mixer=mixer)
    await wait_for(lambda: len(model.histories) == 2, 5)
    (kept,) = model.histories[1]
    assert 0 < kept < 2 * 100  # the interrupted answer is cut to what was heard
    await vc.disconnect()
    assert await asyncio.wait_for(task, 5) == 2
