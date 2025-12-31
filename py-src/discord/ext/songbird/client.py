from __future__ import annotations
from typing import Union, Optional
from .native import SongbirdImpl

import discord
from discord.types.voice import VoiceServerUpdate as VoiceServerUpdatePayload, GuildVoiceState as GuildVoiceStatePayload  # type: ignore


class SongbirdClient(discord.VoiceProtocol, SongbirdImpl):
    channel: Union[discord.VoiceChannel, discord.StageChannel]

    def __init__(self, client: discord.Client, channel: discord.abc.Connectable) -> None:
        super().__init__(client, channel)

    async def connect(
        self, *, timeout: float, reconnect: bool, self_deaf: bool = False, self_mute: bool = False
    ) -> None:
        await SongbirdImpl.connect(self, timeout, reconnect, self_deaf, self_mute)

    async def disconnect(self, *, force: bool) -> None:
        """|coro|

        Disconnect from the voice channel.

        Parameters
        ----------
        force: bool
            Whether to force the disconnect.

        Returns
        -------
        """
        await SongbirdImpl.disconnect(self, force)
        self.cleanup()

    async def on_voice_state_update(self, data: GuildVoiceStatePayload) -> None:
        channel_id = None if data["channel_id"] is None else int(data["channel_id"])
        session_id = data["session_id"]
        await self.update_state(session_id, channel_id)

    async def on_voice_server_update(self, data: VoiceServerUpdatePayload) -> None:
        await self.update_server(data["endpoint"], data["token"])

    async def update_hook(self, channel_id: Optional[int], self_mute: bool, self_deaf: bool) -> None:
        await self.channel.guild.change_voice_state(
            channel=None if channel_id is None else discord.Object(id=channel_id),
            self_mute=self_mute,
            self_deaf=self_deaf,
        )
