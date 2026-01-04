import asyncio
import logging
import os

import aiohttp
import discord
from discord.ext import songbird
from discord.ext.songbird import player

FORMAT = "%(levelname)s %(name)s %(asctime)-15s %(filename)s:%(lineno)d %(message)s"
logging.basicConfig(format=FORMAT)
logging.getLogger().setLevel(logging.DEBUG)
client = discord.Client(intents=discord.Intents.default())


async def feed_radio(buffer: asyncio.StreamReader):
    async with aiohttp.ClientSession() as session:
        async with session.get("https://listen-ssvcbfbs.sharp-stream.com/ssvcbfbs21.aac") as r:
            r.raise_for_status()
            async for chunk in r.content.iter_chunked(12 * 1024):
                buffer.feed_data(chunk)


@client.event
async def on_ready():
    assert client.user is not None
    print("Logged in as")
    print(client.user.name)
    print(client.user.id)

    ch = client.get_channel(int(os.environ["CHANNEL_ID"]))
    if isinstance(ch, discord.VoiceChannel):
        vc = await ch.connect(cls=songbird.SongbirdClient)
        buffer = asyncio.StreamReader()
        asyncio.create_task(feed_radio(buffer))

        i = player.input.StreamInput(buffer, player.input.SupportedCodec.AAC)
        print(asyncio.get_running_loop())
        handle = await vc.play(player.Track(i))
        handle.play()


client.run(os.environ["DISCORD_BOT_TOKEN"])
