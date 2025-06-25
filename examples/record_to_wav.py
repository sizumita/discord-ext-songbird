import os
import discord
from discord.ext import songbird
import struct
import datetime
from pathlib import Path


class VoiceRecorder(songbird.VoiceReceiver):
    def __init__(self):
        self.ssrc_to_user = {}
        self.ssrc_files = {}  # Map SSRC to file handles
        self.ssrc_data_sizes = {}  # Track data size for each SSRC
        self.active_ssrcs = set()  # Track SSRCs that have been seen
        self.recording_dir = Path("target/recordings")
        self.recording_dir.mkdir(exist_ok=True)

        # Create session directory with timestamp
        self.session_dir = self.recording_dir / datetime.datetime.now().strftime("%Y%m%d_%H%M%S")
        self.session_dir.mkdir(exist_ok=True)

        # Audio parameters
        self.sample_rate = 48000
        self.channels = 2
        self.bits_per_sample = 16
        self.bytes_per_sample = self.bits_per_sample // 8
        self.bytes_per_frame = self.channels * self.bytes_per_sample

        # 20ms of silence at 48kHz stereo 16-bit
        self.silence_frame = b"\x00" * (self.sample_rate // 50 * self.bytes_per_frame)

        super().__init__()

    def __del__(self):
        print("Voice recorder destroyed")

    def _write_wav_header(self, file, num_samples, sample_rate=48000, channels=2, bits_per_sample=16):
        """Write WAV header to file."""
        # Calculate sizes
        bytes_per_sample = bits_per_sample // 8
        block_align = channels * bytes_per_sample
        byte_rate = sample_rate * block_align
        data_size = num_samples * block_align
        file_size = 36 + data_size

        # Write RIFF header
        file.write(b"RIFF")
        file.write(struct.pack("<I", file_size))
        file.write(b"WAVE")

        # Write fmt chunk
        file.write(b"fmt ")
        file.write(struct.pack("<I", 16))  # Chunk size
        file.write(struct.pack("<H", 1))  # Audio format (1 = PCM)
        file.write(struct.pack("<H", channels))
        file.write(struct.pack("<I", sample_rate))
        file.write(struct.pack("<I", byte_rate))
        file.write(struct.pack("<H", block_align))
        file.write(struct.pack("<H", bits_per_sample))

        # Write data chunk header
        file.write(b"data")
        file.write(struct.pack("<I", data_size))

    def _get_or_create_file(self, ssrc):
        """Get or create a file for the given SSRC."""
        if ssrc not in self.ssrc_files:
            # Create filename with SSRC and user info if available
            user_id = self.ssrc_to_user.get(ssrc, "unknown")
            filename = f"ssrc_{ssrc}_user_{user_id}.wav"
            filepath = self.session_dir / filename

            # Open file and write placeholder header
            file = open(filepath, "wb")
            self._write_wav_header(file, 0)  # Write header with 0 samples initially

            self.ssrc_files[ssrc] = file
            self.ssrc_data_sizes[ssrc] = 0

            print(f"Created recording file: {filepath}")

        return self.ssrc_files[ssrc]

    def voice_tick(self, tick) -> None:
        """Handle incoming voice tick and save decoded audio."""
        # Track all SSRCs that are speaking
        speaking_ssrcs = set()

        for ssrc, voice_data in tick.speaking:
            speaking_ssrcs.add(ssrc)
            self.active_ssrcs.add(ssrc)

            # Get or create file for this SSRC
            file = self._get_or_create_file(ssrc)

            if voice_data.decoded_voice:
                # Write the decoded PCM data
                file.write(voice_data.decoded_voice)
                # Update data size (decoded_voice is already in bytes)
                self.ssrc_data_sizes[ssrc] += len(voice_data.decoded_voice)
            else:
                # No decoded voice data, write silence
                file.write(self.silence_frame)
                self.ssrc_data_sizes[ssrc] += len(self.silence_frame)

        # Handle silent SSRCs (not speaking but previously active)
        if tick.silent:
            for ssrc in tick.silent:
                if ssrc in self.active_ssrcs:
                    file = self._get_or_create_file(ssrc)
                    # Write silence for this SSRC
                    file.write(self.silence_frame)
                    self.ssrc_data_sizes[ssrc] += len(self.silence_frame)

    def speaking_update(self, ssrc: int, user_id: int, speaking: bool) -> None:
        """Handle speaking state updates."""
        if user_id:
            self.ssrc_to_user[ssrc] = user_id

            # If we already have a file for this SSRC with unknown user, rename it
            if ssrc in self.ssrc_files and f"ssrc_{ssrc}_user_unknown.wav" in str(self.ssrc_files[ssrc].name):
                old_file = self.ssrc_files[ssrc]
                old_path = Path(old_file.name)
                new_path = old_path.parent / f"ssrc_{ssrc}_user_{user_id}.wav"

                # Close the file, rename it, and reopen
                old_file.close()
                old_path.rename(new_path)
                self.ssrc_files[ssrc] = open(new_path, "r+b")
                print(f"Renamed recording file to: {new_path}")

        if speaking:
            user_str = f"User {user_id}" if user_id else "Unknown user"
            print(f"{user_str} (SSRC: {ssrc}) started speaking")
        else:
            user_str = f"User {user_id}" if user_id else "Unknown user"
            print(f"{user_str} (SSRC: {ssrc}) stopped speaking")

    def driver_connect(self) -> None:
        """Handle driver connection."""
        print("Voice driver connected - recording started")

    def driver_disconnect(self) -> None:
        """Handle driver disconnection."""
        print("Voice driver disconnected - finalizing recordings")
        self.finalize_recordings()

    def driver_reconnect(self) -> None:
        """Handle driver reconnection."""
        print("Voice driver reconnected")

    def finalize_recordings(self):
        """Finalize all WAV files by updating their headers with correct sizes."""
        for ssrc, file in self.ssrc_files.items():
            if not file.closed:
                data_size = self.ssrc_data_sizes[ssrc]
                if data_size > 0:
                    # Seek to beginning and rewrite header with correct size
                    file.seek(0)
                    # Assuming 48kHz, 2 channels, 16-bit (4 bytes per sample)
                    num_samples = data_size // 4
                    self._write_wav_header(file, num_samples)

                    print(f"Finalized recording for SSRC {ssrc}: {data_size} bytes ({num_samples} samples)")

                file.close()

        print(f"All recordings saved to: {self.session_dir}")

        # Clear the dictionaries
        self.ssrc_files.clear()
        self.ssrc_data_sizes.clear()


client = discord.Client(intents=discord.Intents.all())


@client.event
async def on_ready():
    print(f"Bot logged in as {client.user}")
    print("Join a voice channel and use '!join' to make the bot join and start recording")
    print("Use '!leave' to make the bot leave and save all recordings")


@client.event
async def on_message(message):
    if message.author.bot:
        return

    if message.content == "!join":
        if message.author.voice and message.author.voice.channel:
            channel: discord.VoiceChannel = message.author.voice.channel
            # Enable decode mode to receive decoded audio
            cfg = songbird.ConfigBuilder().decode_mode(songbird.DecodeMode.Decode)
            voice_client = await channel.connect(cls=songbird.SongbirdClient.WithConfig(cfg))

            # Create and register the voice recorder
            recorder = VoiceRecorder()
            await voice_client.register_receiver(recorder)

            await message.reply(f"Joined {channel.name} and started recording! Recordings will be saved when I leave.")
        else:
            await message.reply("You need to be in a voice channel first!")

    elif message.content == "!leave":
        if message.guild.voice_client:
            # The recorder will finalize recordings in driver_disconnect
            await message.guild.voice_client.disconnect(force=False)
            await message.reply("Left the voice channel and saved all recordings!")
        else:
            await message.reply("I'm not in a voice channel!")


if __name__ == "__main__":
    client.run(os.environ["DISCORD_BOT_TOKEN"])
