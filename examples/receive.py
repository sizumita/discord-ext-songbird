import asyncio
import os
import discord
from discord.ext import songbird


class VoiceReceiver(songbird.VoiceReceiver):
    def __init__(self):
        self.ssrc_to_user = {}
        
    async def voice_packet(self, ssrc: int, packet: songbird.VoicePacket) -> None:
        """Handle incoming voice packets."""
        print(f"Received voice packet from SSRC: {ssrc}")
        print(f"  Sequence: {packet.sequence}, Timestamp: {packet.timestamp}")
        print(f"  RTP data length: {len(packet.rtp_data)} bytes")
        print(f"  Opus data length: {len(packet.opus_data)} bytes")
        if packet.decoded_voice:
            print(f"  Decoded voice data length: {len(packet.decoded_voice)} bytes (16-bit PCM)")
    
    async def speaking_update(self, ssrc: int, user_id: int, speaking: bool) -> None:
        """Handle speaking state updates."""
        if user_id:
            self.ssrc_to_user[ssrc] = user_id
        if speaking:
            user_str = f"User {user_id}" if user_id else "Unknown user"
            print(f"{user_str} (SSRC: {ssrc}) started speaking")
        else:
            user_str = f"User {user_id}" if user_id else "Unknown user"
            print(f"{user_str} (SSRC: {ssrc}) stopped speaking")
    
    async def driver_connect(self) -> None:
        """Handle driver connection."""
        print("Voice driver connected")
    
    async def driver_disconnect(self) -> None:
        """Handle driver disconnection."""
        print("Voice driver disconnected")
    
    async def driver_reconnect(self) -> None:
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
            voice_client = await channel.connect(cls=songbird.SongbirdClient)
            
            # Create and register the voice receiver
            receiver = VoiceReceiver()
            await voice_client.register_receiver(receiver)
            
            await message.reply(f"Joined {channel.name} and started listening for voice data!")
        else:
            await message.reply("You need to be in a voice channel first!")
    
    elif message.content == "!leave":
        if message.guild.voice_client:
            await message.guild.voice_client.disconnect()
            await message.reply("Left the voice channel!")
        else:
            await message.reply("I'm not in a voice channel!")


if __name__ == "__main__":
    token = os.environ.get("DISCORD_BOT_TOKEN")
    if not token:
        print("Please set DISCORD_BOT_TOKEN environment variable")
        exit(1)
    
    client.run(token)
