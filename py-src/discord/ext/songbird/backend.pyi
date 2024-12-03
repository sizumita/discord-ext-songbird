from typing import Dict, Any, List, Callable, Type, Optional, Coroutine

UpdateHook = Callable[[Optional[int], bool, bool], Coroutine[Any, Any, None]]


class SongbirdError(Exception):
    ...


class JoinError(SongbirdError):
    ...


class ConnectionNotInitialized(SongbirdError):
    ...


class SongbirdClient:
    def __init__(self, channel_id: int) -> None:
        ...

    async def start(self, update_hook: UpdateHook, client_id: int, guild_id: int) -> None:
        ...

    async def on_server_update(self, endpoint: str, token: str) -> None:
        ...

    async def on_voice_state_update(self, session_id: str, channel_id: Optional[int]) -> None:
        ...

    async def connect(self, timeout: float, self_deaf: bool, self_mute: bool) -> None:
        ...

    async def leave(self) -> None:
        ...
