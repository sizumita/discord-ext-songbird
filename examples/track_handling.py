import io
import asyncio

import discord
from discord.ext import songbird
import os

sine: io.BytesIO
with open("examples/mono.wav", "rb") as f:
    sine = io.BytesIO(f.read())


client = discord.Client(intents=discord.Intents.default())

# Change channel id you want to join
CHANNEL_ID = 1313754366368550953


@client.event
async def on_ready():
    print("ready")
    channel: discord.VoiceChannel = client.get_channel(CHANNEL_ID)
    voice_client = await channel.connect(cls=songbird.SongbirdClient)
    source = songbird.RawBufferSource(sine)
    track = songbird.Track(source)

    # enqueue returns track handler
    handler = await voice_client.queue.enqueue(track)

    # enabling infinite loop
    handler.enable_loop()

    await asyncio.sleep(1)

    handler.pause()


client.run(os.environ["DISCORD_BOT_TOKEN"])
