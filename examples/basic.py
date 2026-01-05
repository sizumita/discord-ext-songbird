import logging
import os

import discord
import numpy as np
from discord.ext import songbird
from discord.ext.songbird import receive

FORMAT = "%(levelname)s %(name)s %(asctime)-15s %(filename)s:%(lineno)d %(message)s"
logging.basicConfig(format=FORMAT)
logging.getLogger().setLevel(logging.DEBUG)
client = discord.Client(intents=discord.Intents.default())

# Creating a sine wave
duration = 5
sample_rate = 48000
frequency = 440.0
amplitude = 0.9

t = np.linspace(0, duration, int(sample_rate * duration), endpoint=False)
signal = amplitude * np.sin(2 * np.pi * frequency * t)
signal = signal.astype(dtype=np.float32)


@client.event
async def on_ready():
    assert client.user is not None
    print("Logged in as")
    print(client.user.name)
    print(client.user.id)

    ch = client.get_channel(int(os.environ["CHANNEL_ID"]))
    if isinstance(ch, discord.VoiceChannel):
        vc = await ch.connect(cls=songbird.SongbirdClient)

        data = songbird.RawPCMInput(signal, sample_rate=sample_rate, channels=2)
        track = songbird.Track(data)
        handle = await vc.enqueue(track)


client.run(os.environ["DISCORD_BOT_TOKEN"])
