import io
from abc import ABC
from typing import Any, Callable, Optional, Coroutine, TYPE_CHECKING, Self, Dict, List

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
    def crypto_mode(self, mode: CryptoMode) -> Self: ...
    def decode_mode(self, mode: DecodeMode) -> Self: ...
    def decode_channels(self, channel: Channels) -> Self: ...

class CryptoMode:
    class Aes256Gcm: ...
    class XChaCha20Poly1305: ...

class DecodeMode:
    class Pass: ...
    class Decrypt: ...
    class Decode: ...

class Channels:
    class Mono: ...
    class Stereo: ...

class RtpData:
    """Raw RTP packet data."""

    @property
    def sequence(self) -> int:
        """The RTP sequence number."""
        ...

    @property
    def timestamp(self) -> int:
        """The RTP timestamp."""
        ...

    @property
    def payload(self) -> bytes:
        """The RTP payload (audio data)."""
        ...

    @property
    def packet(self) -> bytes:
        """The complete RTP packet."""
        ...

class VoiceData:
    """Voice data received from a user."""

    @property
    def packet(self) -> Optional[RtpData]:
        """Raw RTP packet data if available."""
        ...

    @property
    def decoded_voice(self) -> Optional[bytes]:
        """Decoded PCM audio data if available."""
        ...

class VoiceTick:
    """A single tick of voice data from all speaking users."""

    @property
    def speaking(self) -> Dict[int, VoiceData]:
        """Dictionary mapping SSRC to voice data for all currently speaking users."""
        ...

    @property
    def silent(self) -> List[int]:
        """List of SSRCs that are silent in this tick."""
        ...

class VoiceReceiver(ABC):
    """Base class for receiving voice data from Discord voice channels."""

    def voice_tick(self, tick: VoiceTick) -> None:
        """Called when a voice tick is received from the voice connection.

        This method is called periodically (typically every 20ms) with voice data
        from all users currently speaking in the voice channel. Each tick contains
        both raw RTP packet data and decoded PCM audio data when available.

        Parameters
        ----------
        tick: VoiceTick
            The voice tick containing audio data from all speaking users.

            - tick.speaking: Dict mapping SSRC (Synchronization Source) to VoiceData
              for each user currently transmitting audio in this tick.
            - tick.silent: List of SSRCs that were speaking in previous ticks but
              are silent in this tick.

            For each VoiceData in tick.speaking:
            - packet: Raw RTP packet data including sequence number, timestamp,
              and payload. Available when decode_mode is Pass or Decrypt.
            - decoded_voice: Decoded 16-bit PCM audio data at 48kHz sample rate.
              Available when decode_mode is Decode. The data is interleaved for
              stereo channels or mono for single channel.

        Note
        ----
        The SSRC (Synchronization Source) is a unique identifier for each audio
        stream. Use speaking_update() to map SSRCs to Discord user IDs.
        """
        pass

    def speaking_update(self, ssrc: int, user_id: int, speaking: bool) -> None:
        """Called when a user starts or stops speaking.

        Parameters
        ----------
        ssrc: int
            The synchronization source identifier.
        user_id: int
            The Discord user ID.
        speaking: bool
            Whether the user is speaking.
        """
        pass

    def driver_connect(self) -> None:
        """Called when the driver successfully connects to voice."""
        pass

    def driver_disconnect(self) -> None:
        """Called when the driver disconnects from voice."""
        pass

    def driver_reconnect(self) -> None:
        """Called when the driver reconnects to voice."""
        pass
