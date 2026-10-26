"""Echo bot (design §9.11): listen to one member and play their voice back.

Type ``!echo`` while in a voice channel: the bot joins and repeats what you say, ``DELAY_MS`` later.
``!stop`` ends it (so does the connection closing).

It shows the two halves of streaming audio through the library:

* ``listen_user`` yields a continuous, silence-filled stream of ``AudioChunk`` objects; each chunk
  says whether the user was speaking during it.
* ``open_playback`` returns a ``PlaybackStream`` that plays whatever is written to it. Only voiced
  chunks are written, so the stream goes idle (and stops sending audio) between utterances, and
  ``prebuffer_ms`` holds the start of each utterance back by ``DELAY_MS``.

Both use Discord's native 48 kHz stereo PCM16 (the default format), so nothing is resampled.

Environment: ``DISCORD_BOT_TOKEN``. Requires ``discord.py``.
"""

from __future__ import annotations

import asyncio
import os
from typing import Any

import discord
from discord.ext import songbird
from discord.ext.songbird import PCM16_48K_STEREO, PlaybackFull, StreamClosed

FMT = PCM16_48K_STEREO  # the default of listen_*() and open_playback(); spelled out for clarity
DELAY_MS = 500  # 0..=1000


async def echo(vc: songbird.SongbirdClient, user: Any, stop: asyncio.Event, *, delay_ms: int = DELAY_MS) -> int:
    """Play ``user``'s voice back ``delay_ms`` late until ``stop`` is set or the connection closes.

    Returns the milliseconds of audio echoed.
    """
    async with (
        vc.listen_user(user, FMT, chunk_ms=20) as mic,
        vc.open_playback(FMT, prebuffer_ms=delay_ms, max_buffer_ms=max(2_000, 4 * delay_ms)) as speaker,
    ):

        async def stopper() -> None:
            await stop.wait()
            mic.close()  # ends the `async for` below once the queued chunks are read

        async with asyncio.TaskGroup() as tg:
            st = tg.create_task(stopper())
            async for chunk in mic:  # ends on !stop OR when the connection closes
                if not chunk.speaking:
                    continue  # silence fill: write nothing, so playback idles between utterances
                try:
                    speaker.write_nowait(chunk)  # buffer protocol: no intermediate bytes object
                except PlaybackFull:
                    pass  # the buffer is bounded; drop rather than fall further behind
                except StreamClosed:
                    break  # the connection closed while chunks were still queued: nothing plays now
            st.cancel()
        await speaker.drain(timeout=delay_ms / 1000 + 1)
        return speaker.written_ms


intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)
sessions: dict[int, asyncio.Event] = {}


@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.guild is None or not isinstance(msg.author, discord.Member):
        return
    if msg.content == "!echo" and msg.author.voice and msg.author.voice.channel and msg.guild.id not in sessions:
        vc = await songbird.connect(msg.author.voice.channel)
        stop = sessions[msg.guild.id] = asyncio.Event()
        try:
            echoed = await echo(vc, msg.author, stop)
            await msg.channel.send(f"Echoed {echoed / 1000:.1f} s of audio.")
        finally:
            sessions.pop(msg.guild.id, None)
            await vc.disconnect()
    elif msg.content == "!stop" and (ev := sessions.get(msg.guild.id)) is not None:
        ev.set()


if __name__ == "__main__":
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
