import io
from typing import Any, Callable, Optional, Coroutine, TYPE_CHECKING, Self

if TYPE_CHECKING:
    from .track import Track

UpdateHook = Callable[[Optional[int], bool, bool], Coroutine[Any, Any, None]]

class SongbirdError(Exception): ...
class JoinError(SongbirdError): ...
class ConnectionNotInitialized(SongbirdError): ...

class SongbirdBackend:
    queue: QueueHandler

    def __init__(self, channel_id: int) -> None: ...
    async def start(self, config: ConfigBuilder, update_hook: UpdateHook, client_id: int, guild_id: int) -> None: ...
    async def on_server_update(self, endpoint: str, token: str) -> None: ...
    async def on_voice_state_update(self, session_id: str, channel_id: Optional[int]) -> None: ...
    async def connect(self, timeout: float, self_deaf: bool, self_mute: bool) -> None: ...
    async def leave(self) -> None: ...
    async def mute(self, mute: bool) -> None: ...
    async def deafen(self, deaf: bool) -> None: ...
    def is_mute(self) -> bool: ...
    def is_deaf(self) -> bool: ...
    async def move_to(self, channel_id: int) -> None: ...
    async def play_source(self, source: AudioSource) -> PlayerHandler: ...
    async def register_receiver(self, receiver: Any) -> None: ...

class SourceComposed: ...

class AudioSource:
    def get_source(self) -> SourceComposed: ...

class RawBufferSource(AudioSource):
    """
    Creates an AudioSource from raw data source.
    The source must be a Stream of either pcm, wav, mp3, or ogg opus format.
    """
    def __init__(self, source: io.BufferedIOBase): ...

class PlayerHandler:
    """
    A handler to control the playing track. One handler is created per track.
    """

    @property
    def queue(self) -> QueueHandler:
        """
        Returns the queue that this handler belongs to.

        Returns
        -------
        QueueHandler
        """
        ...

    def play(self) -> None:
        """
        Start playing the track that this handler is handling.

        Returns
        -------
        None
        """
        ...

    def pause(self) -> None:
        """
        Pause playing the track that this handler is handling.

        Returns
        -------
        None
        """
        ...

    def stop(self) -> None:
        """
        Stop playing the track that this handler is handling.

        Returns
        -------
        None
        """
        ...

    def set_volume(self, volume: float) -> None:
        """
        Set the volume of the track that this handler is handling.

        Parameters
        ----------
        volume : float
            The volume to set between 0.0 and 1.0

        Returns
        -------
        None
        """
        ...

    def enable_loop(self) -> None:
        """
        Enable infinite looping for the track that this handler is handling.

        Returns
        -------
        None
        """
        ...

    def disable_loop(self) -> None:
        """
        Disable infinitelooping for the track that this handler is handling.

        Returns
        -------
        None
        """
        ...

    def loop_for(self, count: int) -> None:
        """
        Enable finite looping for the track that this handler is handling.

        Parameters
        ----------
        count : int
            The number of times to loop the track.

        Returns
        -------
        None
        """
        ...

class QueueHandler:
    def enqueue(self, track: Track) -> None:
        """
        Add a track to the queue.

        Parameters
        ----------
        track : Track
            The track to add to the queue.

        Returns
        -------
        None
        """
        ...

    def dequeue(self, index: int) -> None:
        """

        Exclude track from the queue.

        Parameters
        ----------
        index

        Returns
        -------

        """
        ...
    def skip(self) -> None:
        """

        Skip current playing track.

        Returns
        -------
        """
        ...
    def stop(self) -> None:
        """
        Stop playing from the queue.
        Returns
        -------
        """
        ...
    def resume(self) -> None:
        """
        Resumes playing the queue.

        Returns
        -------
        """
        ...

class IntoTrack:
    def __init__(self, source: AudioSource, volume: float, is_loop: bool, loop_count: Optional[int] = None) -> None: ...

class PyVoicePacket:
    ssrc: int
    sequence: Optional[int]
    timestamp: Optional[int]
    @property
    def opus_data(self) -> bytes: ...
    @property
    def rtp_data(self) -> bytes: ...
    @property
    def decoded_voice(self) -> Optional[bytes]: ...

class ConfigBuilder:
    def __init__(self) -> None: ...
    @classmethod
    def send_only(cls) -> Self: ...
    def crypto_mode(self, mode: PyCryptoMode) -> Self: ...
    def decode_mode(self, mode: PyDecodeMode) -> Self: ...
    def decode_channels(self, channel: PyChannels) -> Self: ...

class PyCryptoMode:
    class Aes256Gcm: ...
    class XChaCha20Poly1305: ...

class PyDecodeMode:
    class Pass: ...
    class Decrypt: ...
    class Decode: ...

class PyChannels:
    class Mono: ...
    class Stereo: ...
