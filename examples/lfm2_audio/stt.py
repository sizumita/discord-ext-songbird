"""Speech-to-text bot with LFM2.5-Audio-1.5B-JP: posts what everyone in a voice channel says.

Type ``!transcribe`` while in a voice channel: the bot joins (muted) and posts one message per
utterance, ``**name**: text``, in the text channel the command came from. ``!stop`` ends it (so does
the connection closing).

* ``listen_users`` yields one continuous stream per member, in 16 kHz mono float32: the rate the
  model's audio encoder works at, so the audio goes into the model unchanged.
* ``chunk.speaking`` marks voiced audio. An utterance runs from its first voiced chunk until
  ``END_SILENCE_MS`` without speech (or ``MAX_UTTERANCE_MS``); shorter blips are ignored.
* Recognition runs on one inference thread, never on the event loop. Finished utterances wait in a
  bounded queue; when the model falls that far behind, new ones are dropped with a warning.

Setup: see ``README.md`` in this directory. Environment: ``DISCORD_BOT_TOKEN``, optionally
``LFM_MODEL`` and ``LFM_DEVICE``.
"""

from __future__ import annotations

import asyncio
import logging
import os
from collections.abc import Awaitable, Callable
from concurrent.futures import ThreadPoolExecutor
from typing import Any

import discord
from discord.ext import songbird
from discord.ext.songbird import AudioChunk, AudioFormat, AudioStream

log = logging.getLogger("lfm2_audio.stt")

MODEL_ID = os.environ.get("LFM_MODEL", "LiquidAI/LFM2.5-Audio-1.5B-JP")
IN_FMT = AudioFormat(16_000, 1, "f32le")  # what the model's audio encoder takes
CHUNK_MS = 100
END_SILENCE_MS = 800  # this much silence ends an utterance
MIN_SPEECH_MS = 300  # utterances with less voiced audio are ignored (coughs, clicks)
MAX_UTTERANCE_MS = 30_000  # longer speech is cut into pieces of this length
MAX_PENDING = 16  # finished utterances waiting for the model
MAX_TEXT_TOKENS = 512

#: One model, one device: every inference job runs on this single thread, one after another.
INFERENCE = ThreadPoolExecutor(max_workers=1, thread_name_prefix="lfm")


def pick_device() -> str:
    import torch

    if torch.cuda.is_available():
        return "cuda"
    if torch.backends.mps.is_available():
        return "mps"
    return "cpu"


class Transcriber:
    """LFM2.5-Audio in ASR mode. Blocking: use it on :data:`INFERENCE`.

    ``torch`` and ``liquid_audio`` are imported here rather than at the top of the file, so the bot
    module can be imported (and tested) without them.
    """

    def __init__(self, model_id: str = MODEL_ID, device: str | None = None) -> None:
        from liquid_audio import LFM2AudioModel, LFM2AudioProcessor

        device = device or os.environ.get("LFM_DEVICE") or pick_device()
        self.processor = LFM2AudioProcessor.from_pretrained(model_id, device=device).eval()
        self.model = LFM2AudioModel.from_pretrained(model_id, device=device).eval()

    def transcribe(self, pcm: bytearray) -> str:
        """Text of ``pcm`` (``IN_FMT`` audio)."""
        import torch
        from liquid_audio import ChatState

        chat = ChatState(self.processor)
        chat.new_turn("system")
        chat.add_text("Perform ASR in japanese.")  # the model's fixed ASR prompt
        chat.end_turn()
        chat.new_turn("user")
        chat.add_audio(torch.frombuffer(pcm, dtype=torch.float32).unsqueeze(0), IN_FMT.sample_rate)
        chat.end_turn()
        chat.new_turn("assistant")
        ids = [int(t) for t in self.model.generate_sequential(**chat, max_new_tokens=MAX_TEXT_TOKENS) if t.numel() == 1]
        return self.processor.text.decode(ids, skip_special_tokens=True).strip()


class Utterances:
    """Cuts a continuous stream into utterances, using ``chunk.speaking``."""

    def __init__(self, *, end_silence_ms: int, min_speech_ms: int, max_ms: int) -> None:
        self.end_silence_ms, self.min_speech_ms, self.max_ms = end_silence_ms, min_speech_ms, max_ms
        self._reset()

    def _reset(self) -> None:
        self.pcm = bytearray()
        self.ms = self.voiced_ms = self.silent_ms = 0

    def feed(self, chunk: AudioChunk) -> bytearray | None:
        """Add one chunk; returns the audio of an utterance that just ended."""
        if not self.pcm and not chunk.speaking:
            return None  # waiting for speech
        self.pcm += memoryview(chunk).cast("B")
        self.ms += chunk.duration_ms
        self.voiced_ms += chunk.voiced_ms
        self.silent_ms = 0 if chunk.speaking else self.silent_ms + chunk.duration_ms
        if self.silent_ms >= self.end_silence_ms or self.ms >= self.max_ms:
            return self.flush()
        return None

    def flush(self) -> bytearray | None:
        """End the current utterance now (``None`` if there is none worth keeping)."""
        pcm, voiced = self.pcm, self.voiced_ms
        self._reset()
        return pcm if voiced >= self.min_speech_ms else None


async def transcribe(
    vc: songbird.SongbirdClient,
    model: Any,
    post: Callable[[int, str], Awaitable[Any]],
    stop: asyncio.Event,
) -> int:
    """Call ``post(user_id, text)`` for every utterance until ``stop`` is set or the connection closes.

    ``model`` is a :class:`Transcriber` (or anything with a blocking ``transcribe(pcm) -> str``).
    Utterances already queued are still transcribed at the end. Returns the number posted.
    """
    jobs: asyncio.Queue[tuple[int, bytearray]] = asyncio.Queue(MAX_PENDING)
    posted = 0

    async def cut(stream: AudioStream) -> None:
        user_id = stream.user_id
        assert user_id is not None  # listen_users streams are per user
        utterances = Utterances(end_silence_ms=END_SILENCE_MS, min_speech_ms=MIN_SPEECH_MS, max_ms=MAX_UTTERANCE_MS)

        def queue(pcm: bytearray | None) -> None:
            if pcm is None:
                return
            try:
                jobs.put_nowait((user_id, pcm))
            except asyncio.QueueFull:
                log.warning("recognition is %d utterances behind; dropped one of user %d", MAX_PENDING, user_id)

        async with stream:
            async for chunk in stream:  # continuous: silence while the user is quiet
                queue(utterances.feed(chunk))
        queue(utterances.flush())  # the stream ended mid-utterance (!stop, user left, connection closed)

    async def recognise() -> None:
        nonlocal posted
        loop = asyncio.get_running_loop()
        while True:
            try:
                user_id, pcm = await jobs.get()
            except asyncio.QueueShutDown:
                return  # every queued utterance is done
            try:
                text = await loop.run_in_executor(INFERENCE, model.transcribe, pcm)
                if text:
                    await post(user_id, text)
                    posted += 1
            except Exception:
                log.exception("transcription failed")

    async with vc.listen_users(IN_FMT, chunk_ms=CHUNK_MS, ignore_bots=True) as users:
        async with asyncio.TaskGroup() as tg:
            tg.create_task(recognise())
            async with asyncio.TaskGroup() as cutters:

                async def stopper() -> None:
                    await stop.wait()
                    users.close()  # ends the router and every per-user stream

                st = cutters.create_task(stopper())
                async for stream in users:  # one stream per member, as they first speak
                    cutters.create_task(cut(stream))
                st.cancel()
            jobs.shutdown()  # no more utterances: the recogniser finishes the queued ones, then returns
    return posted


intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)
sessions: dict[int, asyncio.Event] = {}
transcriber: Transcriber | None = None  # loaded in __main__


@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.guild is None or not isinstance(msg.author, discord.Member):
        return
    guild = msg.guild
    if msg.content == "!transcribe" and msg.author.voice and msg.author.voice.channel and guild.id not in sessions:
        assert transcriber is not None
        vc = await songbird.connect(msg.author.voice.channel, self_mute=True)
        stop = sessions[guild.id] = asyncio.Event()

        async def post(user_id: int, text: str) -> None:
            member = guild.get_member(user_id)
            name = member.display_name if member else str(user_id)
            await msg.channel.send(f"**{name}**: {text}", allowed_mentions=discord.AllowedMentions.none())

        await msg.channel.send("文字起こしを始めます。`!stop` で終了します。")
        try:
            await transcribe(vc, transcriber, post, stop)
        finally:
            sessions.pop(guild.id, None)
            await vc.disconnect()
    elif msg.content == "!stop" and (ev := sessions.get(guild.id)) is not None:
        ev.set()


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    transcriber = Transcriber()  # downloads the model from Hugging Face on first use
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
