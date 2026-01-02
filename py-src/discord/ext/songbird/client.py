from __future__ import annotations

from typing import Optional, Union

import discord
from discord.types.voice import GuildVoiceState as GuildVoiceStatePayload
from discord.types.voice import VoiceServerUpdate as VoiceServerUpdatePayload

from .native import SongbirdImpl


class SongbirdClient(discord.VoiceProtocol, SongbirdImpl):
    """VoiceProtocol implementation backed by the native Songbird backend.

    This class is the public entry point used with `discord.VoiceChannel.connect`
    and delegates the heavy lifting to the Rust extension via `SongbirdImpl`.
    """

    channel: Union[discord.VoiceChannel, discord.StageChannel]

    def __init__(self, client: discord.Client, channel: discord.abc.Connectable) -> None:
        """Initialize a Songbird voice client.

        Parameters
        ----------
        client : discord.Client
            The Discord client instance.
        channel : discord.abc.Connectable
            Voice channel or stage channel to connect to.
        """
        super().__init__(client, channel)

    async def connect(
        self, *, timeout: float, reconnect: bool, self_deaf: bool = False, self_mute: bool = False
    ) -> None:
        """|coro|

        Connect to the voice channel.

        Parameters
        ----------
        timeout : float
            Gateway connection timeout in seconds.
        reconnect : bool
            Whether to attempt reconnects (currently ignored).
        self_deaf : bool, optional
            Whether to deafen this account after connecting.
        self_mute : bool, optional
            Whether to mute this account after connecting.

        Returns
        -------
        None
        """
        await SongbirdImpl.connect(self, timeout=timeout, reconnect=reconnect, self_deaf=self_deaf, self_mute=self_mute)

    async def disconnect(self, *, force: bool) -> None:
        """|coro|

        Disconnect from the voice channel.

        Parameters
        ----------
        force : bool
            Whether to force the disconnect.

        Returns
        -------
        None
        """
        await SongbirdImpl.disconnect(self, force=force)
        self.cleanup()

    async def on_voice_state_update(self, data: GuildVoiceStatePayload) -> None:
        """Handle VOICE_STATE_UPDATE events from discord.py.

        Parameters
        ----------
        data : GuildVoiceStatePayload
            Raw voice state update payload.

        Returns
        -------
        None
        """
        channel_id = None if data["channel_id"] is None else int(data["channel_id"])
        session_id = data["session_id"]
        await self.update_state(session_id, channel_id)

    async def on_voice_server_update(self, data: VoiceServerUpdatePayload) -> None:
        """Handle VOICE_SERVER_UPDATE events from discord.py.

        Parameters
        ----------
        data : VoiceServerUpdatePayload
            Raw voice server update payload.

        Returns
        -------
        None
        """
        if data["endpoint"] is None:
            raise ValueError("server update failed")
        await self.update_server(data["endpoint"], data["token"])

    async def update_hook(self, channel_id: Optional[int], self_mute: bool, self_deaf: bool) -> None:
        """|coro|

        Apply local voice state changes.

        Parameters
        ----------
        channel_id : int | None
            Channel ID, or None if disconnecting.
        self_mute : bool
            Whether the account is self-muted.
        self_deaf : bool
            Whether the account is self-deafened.

        Returns
        -------
        None
        """
        await self.channel.guild.change_voice_state(
            channel=None if channel_id is None else discord.Object(id=channel_id),
            self_mute=self_mute,
            self_deaf=self_deaf,
        )
