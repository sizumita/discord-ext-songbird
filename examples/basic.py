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
sine: io.BytesIO = io.BytesIO()

duration = 5
sample_rate = 44100
frequency = 440.0
amplitude = 0.9

t = np.linspace(0, duration, int(sample_rate * duration), endpoint=False)
signal = amplitude * np.sin(2 * np.pi * frequency * t)
pcm_i16 = np.clip(signal * 32767, -32768, 32767).astype(np.int16)

with wave.open(sine, "wb") as f:
    f.setnchannels(1)
    f.setframerate(sample_rate)
    f.setsampwidth(2)  # 16-bit
    f.writeframes(pcm_i16.tobytes())

source = pyarrow.array(pyarrow.py_buffer(sine.getbuffer()), type=pyarrow.uint8())

@client.event
async def on_ready():
    assert client.user is not None
    print("Logged in as")
    print(client.user.name)
    print(client.user.id)

    ch = client.get_channel(int(os.environ["CHANNEL_ID"]))
    if isinstance(ch, discord.VoiceChannel):
        discord.PCMAudio
        vc = await ch.connect(cls=songbird.SongbirdClient)

        data = songbird.player.AudioInput(source, songbird.player.SupportedCodec.WAVE)
        track = songbird.player.Track(data)
        handle = await vc.play(track)


client.run(os.environ["DISCORD_BOT_TOKEN"])
