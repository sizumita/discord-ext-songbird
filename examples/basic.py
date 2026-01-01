import logging
import os

import discord
from discord.ext import songbird

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

        sink = songbird.native.receive.DefaultSink()
        vc.listen(sink)

        async for msg in sink:
            if msg.get_speakings() != {}:
                print("some speakers are speaking")


client.run(os.environ["DISCORD_BOT_TOKEN"])
