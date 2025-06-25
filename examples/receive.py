import os
import discord
from discord.ext import songbird


class VoiceReceiver(songbird.VoiceReceiver):
    def __init__(self):
        self.ssrc_to_user = {}
        super().__init__()

    def voice_tick(self, tick) -> None:
        """Handle incoming voice tick."""
        for ssrc, voice_data in tick.speaking:
            print(f"Received voice data from SSRC: {ssrc}")
            if voice_data.packet:
                print(f"  RTP sequence: {voice_data.packet.sequence}")
                print(f"  RTP timestamp: {voice_data.packet.timestamp}")
                print(f"  RTP payload length: {len(voice_data.packet.payload)} bytes")
                print(f"  RTP packet length: {len(voice_data.packet.packet)} bytes")
            if voice_data.decoded_voice:
                print(f"  Decoded voice data length: {len(voice_data.decoded_voice)} bytes (16-bit PCM)")

        if tick.silent:
            print(f"Silent SSRCs: {tick.silent}")

    def speaking_update(self, ssrc: int, user_id: int, speaking: bool) -> None:
        """Handle speaking state updates."""
        if user_id:
            self.ssrc_to_user[ssrc] = user_id
        if speaking:
            user_str = f"User {user_id}" if user_id else "Unknown user"
            print(f"{user_str} (SSRC: {ssrc}) started speaking")
        else:
            user_str = f"User {user_id}" if user_id else "Unknown user"
            print(f"{user_str} (SSRC: {ssrc}) stopped speaking")

    def driver_connect(self) -> None:
        """Handle driver connection."""
        print("Voice driver connected")

    def driver_disconnect(self) -> None:
        """Handle driver disconnection."""
        print("Voice driver disconnected")

    def driver_reconnect(self) -> None:
        """Handle driver reconnection."""
        print("Voice driver reconnected")


client = discord.Client(intents=discord.Intents.all())


@client.event
async def on_ready():
    print(f"Bot logged in as {client.user}")
    print("Join a voice channel and use '!join' to make the bot join")


@client.event
async def on_message(message):
    if message.author.bot:
        return

    if message.content == "!join":
        if message.author.voice and message.author.voice.channel:
            channel = message.author.voice.channel
            cfg = songbird.ConfigBuilder().decode_mode(songbird.DecodeMode.Decode)
            voice_client = await channel.connect(cls=songbird.SongbirdClient.WithConfig(cfg))

            # Create and register the voice receiver
            receiver = VoiceReceiver()
            await voice_client.register_receiver(receiver)

            await message.reply(f"Joined {channel.name} and started listening for voice data!")
        else:
            await message.reply("You need to be in a voice channel first!")

    elif message.content == "!leave":
        if message.guild.voice_client:
            await message.guild.voice_client.disconnect(force=False)
            await message.reply("Left the voice channel!")
        else:
            await message.reply("I'm not in a voice channel!")


if __name__ == "__main__":
    client.run(os.environ["DISCORD_BOT_TOKEN"])
