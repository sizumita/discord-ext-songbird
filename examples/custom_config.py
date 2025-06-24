import discord
from discord.ext import songbird
import os

client = discord.Client(intents=discord.Intents.default())

# Change channel id you want to join
CHANNEL_ID = 1313754366368550953


@client.event
async def on_ready():
    print("ready")
    channel = client.get_channel(CHANNEL_ID)
    if not isinstance(channel, discord.VoiceChannel):
        return
    await channel.connect(cls=songbird.SongbirdClient.WithConfig(songbird.ConfigBuilder.send_only()))


client.run(os.environ["DISCORD_BOT_TOKEN"])
