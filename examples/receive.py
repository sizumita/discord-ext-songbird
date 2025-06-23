# WIP
from discord.ext.songbird import VoiceEventReceiver


class Receiver(VoiceEventReceiver):
    async def act(self) -> int:
        return 3
