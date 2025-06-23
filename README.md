# discord-ext-songbird

![GitHub License](https://img.shields.io/github/license/sizumita/discord-ext-songbird)
![GitHub Release](https://img.shields.io/github/v/release/sizumita/discord-ext-songbird)
![PyPI - Version](https://img.shields.io/pypi/v/discord-ext-songbird)
![PyPI - Python Version](https://img.shields.io/pypi/pyversions/discord-ext-songbird)

A high-performance voice backend for [discord.py](https://github.com/rapptz/discord.py) powered by [Songbird](https://github.com/serenity-rs/songbird), written in Rust for superior audio processing and lower latency.

## Features

- **High Performance**: Built with Rust for optimal performance and minimal resource usage
- **Discord.py Integration**: Drop-in replacement for discord.py's native voice client
- **Audio Queue Management**: Built-in track queuing and playback control
- **Voice Receiving**: Real-time voice data reception and processing
- **Flexible Audio Sources**: Support for various audio input formats
- **Volume Control**: Per-track volume adjustment
- **Python 3.10+ Support**: Compatible with modern Python versions

## Installation

Install from PyPI:

```bash
pip install discord-ext-songbird
```

## Quick Start

```python
import io
import discord
from discord.ext import songbird

# Initialize Discord client
client = discord.Client(intents=discord.Intents.default())

@client.event
async def on_ready():
    print(f"Logged in as {client.user}")
    
    # Connect to a voice channel
    channel = client.get_channel(YOUR_VOICE_CHANNEL_ID)
    voice_client = await channel.connect(cls=songbird.SongbirdClient)
    
    # Load audio data
    with open("audio.wav", "rb") as f:
        audio_data = io.BytesIO(f.read())
    
    # Create and play a track
    source = songbird.RawBufferSource(audio_data)
    track = songbird.Track(source).set_volume(0.5)
    
    await voice_client.queue.enqueue(track)

client.run("YOUR_BOT_TOKEN")
```

## API Reference

### SongbirdClient

The main voice client class that implements discord.py's `VoiceProtocol`.

```python
# Connect to a voice channel
voice_client = await voice_channel.connect(cls=songbird.SongbirdClient)

# Access the audio queue
await voice_client.queue.enqueue(track)

# Access the player controls
await voice_client.player.stop()
await voice_client.player.pause()
await voice_client.player.resume()
```

### Track Management

Create and configure audio tracks:

```python
# Create a track from raw audio data
source = songbird.RawBufferSource(audio_buffer)
track = songbird.Track(source)

# Set volume (0.0 to 1.0)
track = track.set_volume(0.8)

# Enqueue for playback
await voice_client.queue.enqueue(track)
```

### Audio Sources

Supported audio source types:

- `RawBufferSource`: For raw audio data from `io.BytesIO` objects
- More source types coming soon

### Voice Receiving

Receive and process voice data from other users by creating a custom receiver:

```python
class MyVoiceReceiver(songbird.VoiceReceiver):
    def voice_tick(self, tick):
        """Handle incoming voice data."""
        for ssrc, voice_data in tick.speaking:
            if voice_data.decoded_voice:
                # Process decoded PCM audio data
                audio_data = voice_data.decoded_voice
                print(f"Received {len(audio_data)} bytes of audio from SSRC {ssrc}")
    
    def speaking_update(self, ssrc: int, user_id: int, speaking: bool):
        """Handle speaking state changes."""
        user_str = f"User {user_id}" if user_id else "Unknown user"
        status = "started" if speaking else "stopped"
        print(f"{user_str} (SSRC: {ssrc}) {status} speaking")

# Register the receiver
receiver = MyVoiceReceiver()
await voice_client.register_receiver(receiver)
```

### Configuration

Customize voice connection settings:

```python
from discord.ext.songbird import ConfigBuilder, PyCryptoMode, PyDecodeMode

config = (ConfigBuilder()
    .crypto_mode(PyCryptoMode.Normal)
    .decode_mode(PyDecodeMode.Decode)
    .build())

voice_client = await channel.connect(cls=songbird.SongbirdClient, config=config)
```

## Examples

See the [examples](examples/) directory for more comprehensive usage examples:

- [basic.py](examples/basic.py) - Basic voice playback
- [track_handling.py](examples/track_handling.py) - Advanced track management  
- [custom_config.py](examples/custom_config.py) - Custom voice configuration
- [receive.py](examples/receive.py) - Voice receiving functionality

## Requirements

- Python 3.10 or higher
- discord.py[voice]
- A Discord bot token

## Development

To set up the development environment:

```bash
# Clone the repository
git clone https://github.com/sizumita/discord-ext-songbird.git
cd discord-ext-songbird

uv sync --all-extras --dev --no-install-project
uvx maturin develop 
# or
just build
```

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- [Songbird](https://github.com/serenity-rs/songbird) - The Rust voice library powering this project
- [discord.py](https://github.com/rapptz/discord.py) - The Python Discord API wrapper
- [PyO3](https://github.com/PyO3/pyo3) - Rust bindings for Python
