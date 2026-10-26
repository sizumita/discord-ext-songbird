"""Music bot: the sequential track queue (design §7.10, §9.8).

Commands (in a guild text channel, while in a voice channel):

* ``!play <path>``   — queue a local audio file (wav/mp3/ogg/flac/…, see ``supported_codecs()``)
* ``!stream <url>``  — queue an HTTP audio stream; it is fetched and decoded only once it is current
* ``!skip``, ``!pause``, ``!resume``, ``!stop``, ``!queue``, ``!volume <0-200>``, ``!leave``

Environment: ``DISCORD_BOT_TOKEN``; optionally ``MUSIC_DIR`` (``!play`` paths are relative to it).
Requires ``discord.py`` (``aiohttp`` comes with it).
"""

from __future__ import annotations

import os
from collections.abc import AsyncIterator
from pathlib import Path

import aiohttp
import discord
from discord.ext import songbird
from discord.ext.songbird import SourceError, TrackHandle, load_audio_file

MUSIC_DIR = Path(os.environ.get("MUSIC_DIR", "."))
volumes: dict[int, float] = {}  # guild id → volume applied to new entries
titles: dict[TrackHandle, str] = {}


async def http_stream(url: str, chunk_size: int = 16_384) -> AsyncIterator[bytes]:
    """The body of ``url`` as it arrives (fed to the queue's decode thread)."""
    async with aiohttp.ClientSession() as session, session.get(url) as resp:
        resp.raise_for_status()
        async for chunk in resp.content.iter_chunked(chunk_size):
            yield chunk


async def voice_for(msg: discord.Message) -> songbird.SongbirdClient | None:
    """The guild's songbird client, connecting to the author's channel if needed."""
    guild = msg.guild
    if guild is None or not isinstance(msg.author, discord.Member):
        return None
    if isinstance(guild.voice_client, songbird.SongbirdClient):
        return guild.voice_client
    if msg.author.voice is None or msg.author.voice.channel is None:
        await msg.reply("Join a voice channel first.")
        return None
    return await songbird.connect(msg.author.voice.channel, self_deaf=True)


def describe(vc: songbird.SongbirdClient) -> str:
    q = vc.queue
    lines = []
    if q.current is not None:
        lines.append(f"▶ {titles.get(q.current, '?')}")
    lines += [f"{i + 1}. {titles.get(t, '?')}" for i, t in enumerate(q)]
    return "\n".join(lines) or "The queue is empty."


intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)


@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.guild is None or not msg.content.startswith("!"):
        return
    cmd, _, arg = msg.content.partition(" ")
    if cmd not in {"!play", "!stream", "!skip", "!pause", "!resume", "!stop", "!queue", "!volume", "!leave"}:
        return
    vc = await voice_for(msg)
    if vc is None:
        return
    volume = volumes.get(msg.guild.id, 1.0)
    match cmd:
        case "!play":
            try:
                source = await load_audio_file(MUSIC_DIR / arg.strip())  # off the event loop
            except (OSError, SourceError) as e:
                await msg.reply(f"Cannot play {arg!r}: {e}")
                return
            handle = vc.enqueue(source, volume=volume)
            titles[handle] = f"{arg.strip()} ({(source.duration_ms or 0) // 1000} s)"
            await msg.reply(f"Queued {titles[handle]}")
        case "!stream":
            handle = vc.queue.enqueue_stream(http_stream(arg.strip()), volume=volume)
            titles[handle] = arg.strip()
            await msg.reply(f"Queued stream {arg.strip()}")
        case "!skip":
            vc.queue.skip()
        case "!pause":
            vc.queue.pause()
        case "!resume":
            vc.queue.resume()
        case "!stop":
            vc.queue.stop()
        case "!queue":
            await msg.reply(describe(vc))
        case "!volume":
            try:
                volumes[msg.guild.id] = max(0.0, min(2.0, int(arg) / 100))
            except ValueError:
                await msg.reply("Usage: !volume <0-200>")
                return
            if vc.queue.current is not None:
                vc.queue.current.set_volume(volumes[msg.guild.id])
        case "!leave":
            await vc.disconnect()


if __name__ == "__main__":
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
