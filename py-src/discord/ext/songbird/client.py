from typing import Union

from .backend import SongbirdBackend
import discord
from discord.types.voice import VoiceServerUpdate as VoiceServerUpdatePayload, GuildVoiceState as GuildVoiceStatePayload


class SongbirdClient(discord.VoiceProtocol):
    channel: Union[discord.VoiceChannel, discord.StageChannel]

    def __init__(self, client: discord.Client, channel: discord.abc.Connectable) -> None:
        super().__init__(client, channel)

        channel_id = getattr(channel, "id", None)
        assert channel_id is not None
        self.songbird = SongbirdBackend(channel_id)

    async def connect(self, *, timeout: float, reconnect: bool, self_deaf: bool = False, self_mute: bool = False) -> None:
        guild_id, key_type = self.channel._get_voice_client_key()
        assert key_type == "guild_id"
        await self.songbird.start(self.update_hook, self.client.application_id, guild_id)
        await self.songbird.connect(timeout, self_deaf, self_mute)

    async def disconnect(self, *, force: bool) -> None:
        await self.songbird.leave()

    async def on_voice_state_update(self, data: GuildVoiceStatePayload) -> None:
        channel_id = None if data['channel_id'] is None else int(data['channel_id'])
        session_id = data['session_id']
        await self.songbird.on_voice_state_update(session_id, channel_id)

    async def on_voice_server_update(self, data: VoiceServerUpdatePayload) -> None:
        await self.songbird.on_server_update(data["endpoint"], data["token"])

    async def update_hook(self, channel_id: int, self_mute: bool, self_deaf: bool) -> None:
        await self.channel.guild.change_voice_state(channel=None if channel_id == -1 else discord.Object(id=channel_id), self_mute=self_mute, self_deaf=self_deaf)

