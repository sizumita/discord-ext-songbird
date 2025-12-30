from __future__ import annotations
from typing import Union, Optional, Callable
from . import native

import discord
from discord.types.voice import VoiceServerUpdate as VoiceServerUpdatePayload, GuildVoiceState as GuildVoiceStatePayload  # type: ignore


class SongbirdClient(discord.VoiceProtocol, native.SongbirdImpl):
    channel: Union[discord.VoiceChannel, discord.StageChannel]

    def __init__(self, client: discord.Client, channel: discord.abc.Connectable) -> None:
        super().__init__(client, channel)

    async def connect(
        self, *, timeout: float, reconnect: bool, self_deaf: bool = False, self_mute: bool = False
    ) -> None:
        guild_id, key_type = self.channel._get_voice_client_key()
        assert key_type == "guild_id"
        assert self.client.application_id is not None

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
        await self.songbird.leave()
        self.cleanup()

    async def on_voice_state_update(self, data: GuildVoiceStatePayload) -> None:
        channel_id = None if data["channel_id"] is None else int(data["channel_id"])
        session_id = data["session_id"]
        await self.songbird.on_voice_state_update(session_id, channel_id)

    async def on_voice_server_update(self, data: VoiceServerUpdatePayload) -> None:
        await self.songbird.on_server_update(data["endpoint"], data["token"])

    async def update_hook(self, channel_id: Optional[int], self_mute: bool, self_deaf: bool) -> None:
        await self.channel.guild.change_voice_state(
            channel=None if channel_id is None else discord.Object(id=channel_id),
            self_mute=self_mute,
            self_deaf=self_deaf,
        )

    async def mute(self, mute: bool) -> None:
        """|coro|

        Mute or unmute this account.

        Parameters
        ----------
        mute: bool
            Whether to mute or unmute this account.

        Returns
        -------
        None
        """
        await self.songbird.mute(mute=mute)

    async def deafen(self, deaf: bool) -> None:
        """|coro|

        Deafen or undeafen this account.

        Parameters
        ----------
        deaf: bool
            Whether to deafen or undeafen this account.

        Returns
        -------
        None
        """
        await self.songbird.deafen(deaf=deaf)

    def is_mute(self) -> bool:
        """
        Return whether this account is muted.

        Returns
        -------
        bool
            True if this account is muted, False otherwise.
        """
        return self.songbird.is_mute()

    def is_deaf(self) -> bool:
        """
        Return whether this account is deaf.

        Returns
        -------
        bool
            True if this account is deaf, False otherwise.
        """
        return self.songbird.is_deaf()

    async def move_to(self, channel: Optional[discord.abc.Snowflake]) -> None:
        """|coro|

        Move this account to the specified channel.

        Parameters
        ----------
        channel: Optional[discord.abc.Snowflake]
            The channel to move this account to.

        Returns
        -------
        """
        if channel is None:
            await self.disconnect(force=True)
        else:
            await self.songbird.move_to(channel.id)
