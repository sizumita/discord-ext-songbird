import os
import discord
from discord.ext import songbird

client = discord.Client(intents=discord.Intents.default())


@client.event
async def on_ready():
    print("Logged in as")
    print(client.user.name)
    print(client.user.id)

    ch: discord.abc.Connectable = client.get_channel(int(os.environ["CHANNEL_ID"]))
    await ch.connect(cls=songbird.SongbirdClient)


client.run(os.environ["DISCORD_BOT_TOKEN"])
