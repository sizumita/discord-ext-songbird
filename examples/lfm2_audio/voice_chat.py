"""Realtime voice chat bot with LFM2.5-Audio-1.5B-JP in interleaved mode: speech in, speech out.

Type ``!chat`` while in a voice channel: the bot joins and talks with the channel. Say something;
when you pause, it answers in speech and posts the text of its answer. Talk over it to interrupt
it. ``!stop`` ends it (so does the connection closing).

* ``listen_mixed(policy="floor")`` gives one speaker at a time, in 16 kHz mono float32 (the rate
  of the model's audio encoder). An utterance ends after ``END_SILENCE_MS`` without speech.
* The utterance goes to the model as audio, with the last ``HISTORY_TURNS`` exchanges as context.
  In interleaved mode the model emits a few text tokens, then a few audio frames, and so on, so the
  first audio arrives after a handful of tokens. Each 80 ms frame is decoded as soon as it exists
  (Mimi, streaming) and written to a ``PlaybackStream`` in the decoder's own format (24 kHz mono
  float32; the mixer converts it).
* Barge-in: once someone has spoken ``BARGE_IN_MS`` over an answer, ``clear()`` silences it within
  one or two ticks and generation stops. The answer is written under its own tag, so the
  ``ClearResult`` says how much of it was heard, and only that part stays in the history.
* Generation runs on one inference thread, never on the event loop, and the hand-off between
  them is bounded.

Setup: see ``README.md`` in this directory. Environment: ``DISCORD_BOT_TOKEN``, optionally
``LFM_MODEL`` and ``LFM_DEVICE``.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import os
import threading
from collections import deque
from collections.abc import AsyncGenerator, Awaitable, Callable, Generator
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from typing import Any

import discord
from discord.ext import songbird
from discord.ext.songbird import AudioChunk, AudioFormat, StreamClosed, VoiceConfig

log = logging.getLogger("lfm2_audio.voice_chat")

MODEL_ID = os.environ.get("LFM_MODEL", "LiquidAI/LFM2.5-Audio-1.5B-JP")
SYSTEM_PROMPT = "Respond with interleaved text and audio."  # the model's fixed prompt for this mode
IN_FMT = AudioFormat(16_000, 1, "f32le")  # what the model's audio encoder takes
OUT_FMT = AudioFormat(24_000, 1, "f32le")  # what the Mimi decoder produces
FRAME_MS = 80  # one Mimi audio frame
CHUNK_MS = 40
END_SILENCE_MS = 600  # this much silence ends an utterance: the bot answers
MIN_SPEECH_MS = 300  # utterances with less voiced audio are ignored (coughs, "うん")
MAX_UTTERANCE_MS = 20_000
BARGE_IN_MS = 400  # this much speech over an answer interrupts it
HISTORY_TURNS = 4  # exchanges kept as context (the whole context is prefilled for every answer)
PREBUFFER_MS = 240
MAX_NEW_TOKENS = 1_024  # text tokens + audio frames per answer

#: One model, one device: every inference job runs on this single thread, one after another.
INFERENCE = ThreadPoolExecutor(max_workers=1, thread_name_prefix="lfm")


def pick_device() -> str:
    import torch

    if torch.cuda.is_available():
        return "cuda"
    if torch.backends.mps.is_available():
        return "mps"
    return "cpu"


@dataclass
class Turn:
    """One exchange, kept as model tokens so that later answers see it as context."""

    user_pcm: bytearray  # the utterance, IN_FMT
    tokens: list[Any] = field(default_factory=list)  # the answer in generation order: text ids (int) and audio frames

    def keep_heard(self, heard_ms: int) -> None:
        """After an interruption: keep the answer only up to the last audio frame that was heard."""
        frames = 0
        for i, token in enumerate(self.tokens):
            if not isinstance(token, int):
                frames += 1
                if frames * FRAME_MS > heard_ms:
                    del self.tokens[i:]
                    return


class ChatModel:
    """LFM2.5-Audio in interleaved mode with a streaming Mimi decoder. Blocking: use it on
    :data:`INFERENCE`.

    ``torch`` and ``liquid_audio`` are imported here rather than at the top of the file, so the bot
    module can be imported (and tested) without them.
    """

    def __init__(self, model_id: str = MODEL_ID, device: str | None = None) -> None:
        from liquid_audio import LFM2AudioModel, LFM2AudioProcessor

        device = device or os.environ.get("LFM_DEVICE") or pick_device()
        self.processor = LFM2AudioProcessor.from_pretrained(model_id, device=device).eval()
        self.model = LFM2AudioModel.from_pretrained(model_id, device=device).eval()
        self.mimi = self.processor.mimi.eval()  # works frame by frame on any device

    def respond(self, history: list[Turn], pcm: bytearray) -> Generator[tuple[Any, Any]]:
        """Answer the utterance ``pcm`` after ``history``.

        Yields ``(token, audio)`` for every generated token: a text id (``int``) with ``None``, or an
        audio frame with its 80 ms of ``OUT_FMT`` audio (a float32 array; ``None`` for the
        end-of-audio frame).
        """
        import torch
        from liquid_audio import ChatState, LFMModality

        chat = ChatState(self.processor)
        chat.new_turn("system")
        chat.add_text(SYSTEM_PROMPT)
        chat.end_turn()
        for turn in history:
            self._add_utterance(chat, turn.user_pcm)
            chat.new_turn("assistant")
            frames = [t for t in turn.tokens if not isinstance(t, int)]
            chat.append(
                text=torch.tensor([[t for t in turn.tokens if isinstance(t, int)]], dtype=torch.long),
                audio_out=torch.stack(frames, 1) if frames else chat.audio_out.new_empty((8, 0)),
                modality_flag=torch.tensor(
                    [LFMModality.TEXT if isinstance(t, int) else LFMModality.AUDIO_OUT for t in turn.tokens],
                    dtype=torch.long,
                ),
            )
            chat.end_turn()
        self._add_utterance(chat, pcm)
        chat.new_turn("assistant")
        tokens = self.model.generate_interleaved(
            **chat, max_new_tokens=MAX_NEW_TOKENS, audio_temperature=1.0, audio_top_k=4
        )
        with torch.no_grad(), self.mimi.streaming(1):
            for t in tokens:
                if t.numel() == 1:
                    yield int(t), None
                elif (t == 2048).any():
                    yield t, None  # end of audio: part of the history, not played
                else:
                    yield t, self.mimi.decode(t[None, :, None])[0, 0].float().clamp_(-1, 1).cpu().numpy()

    def _add_utterance(self, chat: Any, pcm: bytearray) -> None:
        import torch

        chat.new_turn("user")
        chat.add_audio(torch.frombuffer(pcm, dtype=torch.float32).unsqueeze(0), IN_FMT.sample_rate)
        chat.end_turn()

    def text(self, turn: Turn) -> str:
        """The text of ``turn``'s answer (cheap: no model call)."""
        ids = [t for t in turn.tokens if isinstance(t, int)]
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


class Reply:
    """One spoken answer: generated on the inference thread, played under its own tag."""

    def __init__(
        self,
        model: Any,
        speaker: songbird.PlaybackStream,
        history: list[Turn],
        turn: Turn,
        tag: str,
        post: Callable[[str], Awaitable[Any]],
    ) -> None:
        self.speaker, self.turn, self.tag = speaker, turn, tag
        self.task = asyncio.create_task(self._run(model, history, post))

    async def _run(self, model: Any, history: list[Turn], post: Callable[[str], Awaitable[Any]]) -> None:
        try:
            pieces = iterate_on_worker(lambda: model.respond(history, self.turn.user_pcm))
            async with contextlib.aclosing(pieces):
                async for token, pcm in pieces:
                    self.turn.tokens.append(token)
                    if pcm is not None:
                        await self.speaker.write(pcm, tag=self.tag)  # waits while the buffer is full
            self.speaker.end_segment(self.tag)
            if text := model.text(self.turn):
                await post(text)  # while the end of the answer is still playing
            await self.speaker.drain()
        except StreamClosed:
            pass  # the connection closed while answering
        except Exception:
            log.exception("answering failed")

    def interrupt(self) -> None:
        """Someone talks over the answer: silence it, stop generating, and keep only what was heard."""
        result = self.speaker.clear()  # late writes of this tag are dropped from now on
        self.task.cancel()
        seg = next((s for s in result.segments if s.tag == self.tag), None) or self.speaker.segment(self.tag)
        self.turn.keep_heard(0 if seg is None else seg.heard_ms)


async def converse(
    vc: songbird.SongbirdClient,
    model: Any,
    post: Callable[[str], Awaitable[Any]],
    stop: asyncio.Event,
) -> int:
    """Talk with the voice channel until ``stop`` is set or the connection closes.

    ``model`` is a :class:`ChatModel` (or anything with its blocking ``respond`` and ``text``).
    ``post(text)`` is called with the text of every answer. Returns the number of answers started.
    """
    history: deque[Turn] = deque(maxlen=HISTORY_TURNS)
    utterances = Utterances(end_silence_ms=END_SILENCE_MS, min_speech_ms=MIN_SPEECH_MS, max_ms=MAX_UTTERANCE_MS)
    reply: Reply | None = None
    answers = 0
    async with (
        vc.listen_mixed(IN_FMT, chunk_ms=CHUNK_MS, policy="floor") as mic,
        vc.open_playback(OUT_FMT, prebuffer_ms=PREBUFFER_MS) as speaker,
    ):

        async def stopper() -> None:
            await stop.wait()
            mic.close()  # ends the `async for` below at once

        st = asyncio.create_task(stopper())
        try:
            async for chunk in mic:  # continuous: silence while nobody speaks
                if reply is not None and reply.task.done():
                    reply = None  # answered and played
                pcm = utterances.feed(chunk)
                if reply is not None and utterances.voiced_ms >= BARGE_IN_MS:
                    reply.interrupt()
                    reply = None
                if pcm is not None and reply is None:  # (a short "うん" during an answer is ignored)
                    turn = Turn(pcm)
                    reply = Reply(model, speaker, list(history), turn, f"answer-{answers}", post)
                    history.append(turn)  # filled in, and cut if interrupted, by the reply
                    answers += 1
        finally:
            st.cancel()
            if reply is not None:
                reply.task.cancel()
                await asyncio.gather(reply.task, return_exceptions=True)
    return answers


intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)
sessions: dict[int, asyncio.Event] = {}
chat_model: ChatModel | None = None  # loaded in __main__


@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.guild is None or not isinstance(msg.author, discord.Member):
        return
    if msg.content == "!chat" and msg.author.voice and msg.author.voice.channel and msg.guild.id not in sessions:
        assert chat_model is not None
        vc = await songbird.connect(msg.author.voice.channel, config=VoiceConfig.low_latency())
        stop = sessions[msg.guild.id] = asyncio.Event()

        async def post(text: str) -> None:
            await msg.channel.send(text, allowed_mentions=discord.AllowedMentions.none())

        await msg.channel.send("話しかけてください。答えている途中に話すと割り込めます。`!stop` で終了します。")
        try:
            await converse(vc, chat_model, post, stop)
        finally:
            sessions.pop(msg.guild.id, None)
            await vc.disconnect()
    elif msg.content == "!stop" and (ev := sessions.get(msg.guild.id)) is not None:
        ev.set()


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    chat_model = ChatModel()  # downloads the model from Hugging Face on first use
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
