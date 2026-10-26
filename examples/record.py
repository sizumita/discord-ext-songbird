"""Recording bot (design §9.12): one timeline-aligned WAV per user plus a log of speaking events.

``!record`` in a voice channel starts recording into ``recordings/<guild>-<time>/``; ``!stop`` ends
it (so does the connection closing). Memory is bounded per user at 10 s of audio however slowly the
disk is written.

Environment: ``DISCORD_BOT_TOKEN``. Requires ``discord.py``.
"""

from __future__ import annotations

import asyncio
import json
import os
import time
import wave
from pathlib import Path

import discord
from discord.ext import songbird
from discord.ext.songbird import PCM16_48K, AudioStream, VoiceEvent

REC = PCM16_48K  # 48 kHz mono PCM16LE
ONE_S = REC.silence(1_000)  # silence(ms) is capped at 600 s, so long gaps go in pieces


def write_silence(w: wave.Wave_write, ms: int) -> None:
    for _ in range(ms // 1_000):
        w.writeframes(ONE_S)
    w.writeframes(REC.silence(ms % 1_000))


async def record_user(stream: AudioStream, out: Path, origin_ms: int) -> None:
    async with stream:
        with wave.open(str(out / f"{stream.user_id}.wav"), "wb") as w:
            w.setnchannels(REC.channels)
            w.setsampwidth(REC.sample_width)
            w.setframerate(REC.sample_rate)
            first = True
            async for chunk in stream:  # continuous: silence while the user is quiet
                if first:  # align every file on the shared receive timeline
                    write_silence(w, max(0, chunk.timeline_ms - origin_ms))
                    first = False
                if chunk.dropped_ms:  # only if the disk could not keep up
                    write_silence(w, chunk.dropped_ms)
                w.writeframes(chunk)  # buffer protocol: no intermediate bytes object


async def log_events(vc: songbird.SongbirdClient, out: Path) -> None:
    kinds = [VoiceEvent.SpeakingStarted, VoiceEvent.SpeakingStopped, VoiceEvent.UserLeft]
    with (out / "events.jsonl").open("a") as f:
        async with vc.events(kinds=kinds) as events:
            async for ev in events:
                f.write(
                    json.dumps({"event": type(ev).__name__, **{k: getattr(ev, k) for k in ev.__match_args__}}) + "\n"
                )


async def record(vc: songbird.SongbirdClient, out: Path, stop: asyncio.Event) -> None:
    """Record until ``stop`` is set or the connection closes."""
    out.mkdir(parents=True, exist_ok=True)
    origin = vc.timeline_ms
    async with vc.listen_users(REC, chunk_ms=200, max_queue_ms=10_000, ignore_bots=True) as users:
        async with asyncio.TaskGroup() as tg:
            logger = tg.create_task(log_events(vc, out))

            async def stopper() -> None:
                await stop.wait()
                users.close()  # ends the router and every per-user stream

            st = tg.create_task(stopper())
            async for stream in users:  # ends on !stop OR when the connection closes
                tg.create_task(record_user(stream, out, origin))
            st.cancel()  # never wait for a !stop that will not come
            logger.cancel()


intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)
sessions: dict[int, asyncio.Event] = {}


@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.guild is None or not isinstance(msg.author, discord.Member):
        return
    if msg.content == "!record" and msg.author.voice and msg.author.voice.channel and msg.guild.id not in sessions:
        vc = await msg.author.voice.channel.connect(cls=songbird.SongbirdClient, self_mute=True)
        stop = sessions[msg.guild.id] = asyncio.Event()
        try:
            await record(vc, Path("recordings") / f"{msg.guild.id}-{int(time.time())}", stop)
        finally:
            sessions.pop(msg.guild.id, None)
            await vc.disconnect()
    elif msg.content == "!stop" and (ev := sessions.get(msg.guild.id)) is not None:
        ev.set()


if __name__ == "__main__":
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
