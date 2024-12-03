import asyncio

import discord
from discord.ext import songbird
import os

client = discord.Client(intents=discord.Intents.default())

CHANNEL_ID = 1311019617740783747

@client.event
async def on_ready():
    channel: discord.VoiceChannel = client.get_channel(CHANNEL_ID)
    stream_client: songbird.StreamingClient = await channel.connect(cls=songbird.StreamingClient, self_deaf=True)

    await asyncio.sleep(5)
    await stream_client.disconnect(force=True)

client.run(os.environ["DISCORD_BOT_TOKEN"])
