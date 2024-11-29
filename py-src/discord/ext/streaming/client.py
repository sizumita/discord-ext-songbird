from typing import Any
from .abc import AsyncStreamWriterABC

import discord
from discord.voice_state import VoiceConnectionState
from discord.gateway import DiscordVoiceWebSocket

class StreamingClient(discord.VoiceClient):
    def create_connection_state(self) -> VoiceConnectionState:
        return VoiceConnectionState(self, hook=self._hook)

    async def _hook(self, ws: DiscordVoiceWebSocket, data: dict[str, Any]):
        match data["d"]:
            case ws.CLIENT_CONNECT:
                pass

    def sink(self) -> AsyncStreamWriterABC[int]:
        pass
