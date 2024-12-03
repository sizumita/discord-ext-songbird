import asyncio

import discord
from discord.ext import songbird
import os

client = discord.Client(intents=discord.Intents.default())

CHANNEL_ID = 1311019617740783747

@client.event
async def on_ready():
    print("ready")
    channel: discord.VoiceChannel = client.get_channel(CHANNEL_ID)
    voice_client: songbird.SongbirdClient = await channel.connect(cls=songbird.SongbirdClient, self_deaf=True)

    await asyncio.sleep(5)
    await voice_client.disconnect(force=True)

client.run(os.environ["DISCORD_BOT_TOKEN"])
