import asyncio
import io

import discord
from discord.ext import songbird
import os

sine: io.BytesIO
with open("natori.wav", "rb") as f:
    sine = io.BytesIO(f.read())


client = discord.Client(intents=discord.Intents.default())

CHANNEL_ID = 1313754366368550953

@client.event
async def on_ready():
    print("ready")
    channel: discord.VoiceChannel = client.get_channel(CHANNEL_ID)
    voice_client: songbird.SongbirdClient = await channel.connect(cls=songbird.SongbirdClient)
    await asyncio.sleep(2)
    source = songbird.RawBufferSource(sine)
    print("is muted: ", await voice_client.is_mute())

    handler = await voice_client.play(source)
    print(handler)
    handler.play()


client.run(os.environ["DISCORD_BOT_TOKEN"])
