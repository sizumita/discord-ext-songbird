import io

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
    channel = client.get_channel(CHANNEL_ID)
    if not isinstance(channel, discord.VoiceChannel):
        return
    voice_client: songbird.SongbirdClient = await channel.connect(cls=songbird.SongbirdClient)
    source = songbird.RawBufferSource(sine)
    track = songbird.Track(source).set_volume(0.5)

    await voice_client.queue.enqueue(track)


client.run(os.environ["DISCORD_BOT_TOKEN"])
