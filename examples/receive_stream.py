import asyncio
import logging
import os

import discord
from discord.ext import songbird
from discord.ext.songbird import receive

FORMAT = "%(levelname)s %(name)s %(asctime)-15s %(filename)s:%(lineno)d %(message)s"
logging.basicConfig(format=FORMAT)
logging.getLogger().setLevel(logging.DEBUG)
client = discord.Client(intents=discord.Intents.default())


@client.event
async def on_ready():
    assert client.user is not None
    print("Logged in as")
    print(client.user.name)
    print(client.user.id)

    ch = client.get_channel(int(os.environ["CHANNEL_ID"]))
    if isinstance(ch, discord.VoiceChannel):
        vc = await ch.connect(cls=songbird.SongbirdClient)

        sink = receive.StreamSink()
        vc.listen(sink)

        async with sink.stream() as stream:
            async for msg in stream:
                print("speakings: ", msg.speaking_keys(), "silents: ", msg.silent_keys())


client.run(os.environ["DISCORD_BOT_TOKEN"])
