# discord-ext-songbird

![GitHub License](https://img.shields.io/github/license/sizumita/discord-ext-songbird)
![GitHub Release](https://img.shields.io/github/v/release/sizumita/discord-ext-songbird)
![PyPI - Version](https://img.shields.io/pypi/v/discord-ext-songbird)
![PyPI - Python Version](https://img.shields.io/pypi/pyversions/discord-ext-songbird)


Library to replace the voice backend of [discord.py](https://github.com/rapptz/discord.py) with [Songbird](https://github.com/serenity-rs/songbird).

# Installation

```bash
$ python -m pip install discord-ext-songbird
```

# Quick Example

```python
import discord
from discord.ext.songbird import SongbirdClient

client = discord.Client()

@client.event
async def on_ready():
    print("ready")
    channel: discord.VoiceChannel = client.get_channel(CHANNEL_ID)
    voice_client: SongbirdClient = await channel.connect(cls=SongbirdClient)
    source = songbird.RawBufferSource(...) # passes io.BufferIOBase
    track = songbird.Track(source).set_volume(0.8)

    await voice_client.queue.enqueue(track)

client.run(...)
```

More examples are on `examples` folder.

# Todo

- [ ] Voice Sending 
  - [ ] Multi Codec Support
    - [x] Wav
  - [ ] Sharded Bot Support
  - [ ] Stream input
- [ ] Voice Receiving
  - [ ] Sink Model
    - [ ] Multi ssrc stream
