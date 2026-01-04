import asyncio
import io
import logging
import os
import wave

import discord
import numpy as np
import pyarrow
from discord.ext import songbird
from discord.ext.songbird import receive

FORMAT = "%(levelname)s %(name)s %(asctime)-15s %(filename)s:%(lineno)d %(message)s"
logging.basicConfig(format=FORMAT)
logging.getLogger().setLevel(logging.DEBUG)
client = discord.Client(intents=discord.Intents.default())

# Creating a sine wave
sine: io.BytesIO

with open("examples/mono.wav", "rb") as f:
    sine = io.BytesIO(f.read())
sine_wav = pyarrow.array(pyarrow.py_buffer(sine.getbuffer()), pyarrow.uint8())

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

        data = songbird.player.AudioInput(signal, songbird.player.SupportedCodec.PCM)
        data2 = songbird.player.AudioInput(sine_wav, songbird.player.SupportedCodec.WAVE)
        track = songbird.player.Track(data)
        track2 = songbird.player.Track(data2)
        handle = await vc.play(track)
        handle2 = await vc.play(track2)


client.run(os.environ["DISCORD_BOT_TOKEN"])
