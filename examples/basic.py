import discord
from discord.ext import streaming

client = discord.Client(intents=discord.Intents.default())

CHANNEL_ID = 0

@client.event
async def on_ready():
    channel = client.get_channel(CHANNEL_ID)
    stream_client: streaming.StreamingClient = await channel.connect(cls=streaming.StreamingClient)
    sink = stream_client.sink()
