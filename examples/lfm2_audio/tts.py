"""Read-aloud bot with LFM2.5-Audio-1.5B-JP: speaks every message posted in a text channel.

Type ``!tts`` in a text channel while in a voice channel: the bot joins and reads each new message
of that text channel aloud, one at a time. ``!skip`` cuts the message being read; ``!stop`` ends it
(so does the connection closing).

* Speech is streamed: the model generates 80 ms audio frames, each is decoded as soon as it exists
  and written to a ``PlaybackStream`` in the decoder's own format (24 kHz mono float32; the mixer
  converts it to Discord's). Reading starts long before the whole message is synthesised.
* Generation runs on one inference thread. Frames reach the event loop through a small bounded
  hand-off, and ``await speaker.write()`` waits while the playback buffer is full, so the model
  never runs further ahead than the buffer.
* Each message is written under its own tag. ``!skip`` calls ``clear()``: playback is silent within
  one or two ticks, and audio of that message still being generated is dropped.
* The bot never listens, so it connects with ``VoiceConfig(receive=False)``: nothing is decoded.

Setup: see ``README.md`` in this directory. Environment: ``DISCORD_BOT_TOKEN``, optionally
``LFM_MODEL`` and ``LFM_DEVICE``.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import os
import re
import threading
from collections.abc import AsyncGenerator, Callable, Generator
from concurrent.futures import ThreadPoolExecutor
from typing import Any

import discord
from discord.ext import songbird
from discord.ext.songbird import AudioFormat, StreamClosed, VoiceConfig, VoiceEvent

log = logging.getLogger("lfm2_audio.tts")

MODEL_ID = os.environ.get("LFM_MODEL", "LiquidAI/LFM2.5-Audio-1.5B-JP")
OUT_FMT = AudioFormat(24_000, 1, "f32le")  # what the Mimi decoder produces
PREBUFFER_MS = 240  # smooths the start of each message if generation is barely faster than real time
MAX_CHARS = 120  # longer messages are cut
MAX_PENDING = 20  # messages waiting to be read; more are skipped
MAX_AUDIO_FRAMES = 500  # 80 ms each: at most 40 s of speech per message

#: One model, one device: every inference job runs on this single thread, one after another.
INFERENCE = ThreadPoolExecutor(max_workers=1, thread_name_prefix="lfm")


def pick_device() -> str:
    import torch

    if torch.cuda.is_available():
        return "cuda"
    if torch.backends.mps.is_available():
        return "mps"
    return "cpu"


class Synthesizer:
    """LFM2.5-Audio in TTS mode with a streaming Mimi decoder. Blocking: use it on :data:`INFERENCE`.

    ``torch`` and ``liquid_audio`` are imported here rather than at the top of the file, so the bot
    module can be imported (and tested) without them.
    """

    def __init__(self, model_id: str = MODEL_ID, device: str | None = None) -> None:
        from liquid_audio import LFM2AudioModel, LFM2AudioProcessor

        device = device or os.environ.get("LFM_DEVICE") or pick_device()
        self.processor = LFM2AudioProcessor.from_pretrained(model_id, device=device).eval()
        self.model = LFM2AudioModel.from_pretrained(model_id, device=device).eval()
        self.mimi = self.processor.mimi.eval()  # works frame by frame on any device

    def synthesize(self, text: str) -> Generator[Any]:
        """Speech for ``text``: yields 80 ms ``OUT_FMT`` buffers (float32 arrays) as they are generated."""
        import torch
        from liquid_audio import ChatState

        chat = ChatState(self.processor)
        chat.new_turn("system")
        chat.add_text("Perform TTS in japanese.")  # the model's fixed TTS prompt
        chat.end_turn()
        chat.new_turn("user")
        chat.add_text(text)
        chat.end_turn()
        chat.new_turn("assistant")
        tokens = self.model.generate_sequential(
            **chat, max_new_tokens=MAX_AUDIO_FRAMES, audio_temperature=0.8, audio_top_k=64
        )
        with torch.no_grad(), self.mimi.streaming(1):
            for t in tokens:
                if t.numel() == 1:
                    continue  # a text token (<|audio_start|>)
                if (t == 2048).any():
                    return  # end of audio
                yield self.mimi.decode(t[None, :, None])[0, 0].float().clamp_(-1, 1).cpu().numpy()


async def iterate_on_worker[T](job: Callable[[], Generator[T]], *, max_ahead: int = 16) -> AsyncGenerator[T]:
    """Run the blocking generator ``job()`` on :data:`INFERENCE` and yield its items on the event loop.

    The worker runs at most ``max_ahead`` items ahead of the consumer, then waits. Closing this
    iterator (``break``, cancellation) makes the worker stop after its current item.
    """
    loop = asyncio.get_running_loop()
    items: asyncio.Queue[tuple[str, Any]] = asyncio.Queue()  # bounded by `room`
    room = threading.Semaphore(max_ahead)
    stop = threading.Event()

    def send(kind: str, value: Any) -> bool:
        try:
            loop.call_soon_threadsafe(items.put_nowait, (kind, value))
        except RuntimeError:  # the event loop is closed
            return False
        return True

    def work() -> None:
        if stop.is_set():
            return  # closed while this job waited for the inference thread
        try:
            with contextlib.closing(job()) as gen:
                for item in gen:
                    while not room.acquire(timeout=0.1):
                        if stop.is_set():
                            return
                    if stop.is_set() or not send("item", item):
                        return
        except Exception as exc:
            send("error", exc)
        else:
            send("end", None)

    loop.run_in_executor(INFERENCE, work)
    try:
        while True:
            kind, value = await items.get()
            if kind == "end":
                return
            if kind == "error":
                raise value
            room.release()
            yield value
    finally:
        stop.set()


async def until_closed(vc: songbird.SongbirdClient) -> None:
    """Return once the voice connection has closed for good."""
    async with vc.events(kinds=[VoiceEvent.Disconnected]) as events:
        async for ev in events:
            if not ev.will_retry:
                return


class Reader:
    """Reads queued texts aloud through one playback stream, one message at a time.

    ``model`` is a :class:`Synthesizer` (or anything with a blocking ``synthesize(text)`` generator of
    ``OUT_FMT`` buffers).
    """

    def __init__(self, vc: songbird.SongbirdClient, model: Any, *, max_pending: int = MAX_PENDING) -> None:
        self.vc = vc
        self.model = model
        self.texts: asyncio.Queue[str] = asyncio.Queue(max_pending)
        self.speaker: songbird.PlaybackStream | None = None
        self._current: asyncio.Task[None] | None = None

    def say(self, text: str) -> bool:
        """Queue ``text``; ``False`` if too many messages are waiting or the reader has stopped."""
        try:
            self.texts.put_nowait(text)
        except asyncio.QueueFull, asyncio.QueueShutDown:
            return False
        return True

    def skip(self) -> None:
        """Cut the message being read."""
        if self._current is not None and not self._current.done():
            if self.speaker is not None:
                self.speaker.clear()  # silent now; late writes of its tag are dropped
            self._current.cancel()  # stops the generation too

    def stop(self) -> None:
        """Stop reading: forget the queued messages and cut the current one."""
        self.texts.shutdown(immediate=True)
        self.skip()

    async def run(self) -> int:
        """Read until :meth:`stop` or the connection closes. Returns the number of messages read."""
        read = 0
        async with self.vc.open_playback(OUT_FMT, prebuffer_ms=PREBUFFER_MS) as speaker:
            self.speaker = speaker
            closed = asyncio.create_task(until_closed(self.vc))
            closed.add_done_callback(lambda _: self.stop())
            try:
                while True:
                    try:
                        text = await self.texts.get()
                    except asyncio.QueueShutDown:
                        return read
                    self._current = asyncio.create_task(self._speak(speaker, text, f"msg-{read}"))
                    read += 1
                    await asyncio.wait([self._current])  # a skip cancels this task, not run()
                    if not self._current.cancelled() and (exc := self._current.exception()) is not None:
                        if isinstance(exc, StreamClosed):
                            return read  # the connection closed while reading
                        log.error("reading a message failed", exc_info=exc)
            finally:
                closed.cancel()
                if self._current is not None:
                    self._current.cancel()

    async def _speak(self, speaker: songbird.PlaybackStream, text: str, tag: str) -> None:
        async with contextlib.aclosing(iterate_on_worker(lambda: self.model.synthesize(text))) as audio:
            async for pcm in audio:
                await speaker.write(pcm, tag=tag)  # waits while the buffer is full
        speaker.end_segment(tag)
        await speaker.drain()  # one message at a time, so !skip only ever cuts this one


URL = re.compile(r"https?://\S+")
CUSTOM_EMOJI = re.compile(r"<a?:(\w+):\d+>")


def speech_text(msg: discord.Message) -> str:
    """What to say for ``msg``: mentions as names, links and custom emoji shortened, length capped."""
    text = CUSTOM_EMOJI.sub(r"\1", URL.sub("URL", msg.clean_content))
    text = " ".join(text.split())
    return text if len(text) <= MAX_CHARS else text[:MAX_CHARS] + "、以下略"


intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)
readers: dict[int, tuple[int, Reader]] = {}  # guild id -> (text channel id, reader)
synthesizer: Synthesizer | None = None  # loaded in __main__


@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.guild is None or not isinstance(msg.author, discord.Member) or msg.author.bot:
        return  # bots are never read, the bot's own messages included
    entry = readers.get(msg.guild.id)
    if msg.content == "!tts" and msg.author.voice and msg.author.voice.channel and entry is None:
        assert synthesizer is not None
        vc = await songbird.connect(msg.author.voice.channel, config=VoiceConfig(receive=False), self_deaf=True)
        reader = Reader(vc, synthesizer)
        readers[msg.guild.id] = (msg.channel.id, reader)
        await msg.channel.send("このチャンネルの読み上げを始めます。`!skip` で読み飛ばし、`!stop` で終了します。")
        try:
            await reader.run()
        finally:
            readers.pop(msg.guild.id, None)
            await vc.disconnect()
    elif entry is None or msg.channel.id != entry[0]:
        return
    elif msg.content == "!stop":
        entry[1].stop()
    elif msg.content == "!skip":
        entry[1].skip()
    elif not msg.content.startswith("!") and (text := speech_text(msg)):
        if not entry[1].say(text):
            log.warning("%d messages are already waiting; skipped one", MAX_PENDING)


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    synthesizer = Synthesizer()  # downloads the model from Hugging Face on first use
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
