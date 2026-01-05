from . import native
from .client import SongbirdClient as SongbirdClient
from .native import player as player
from .native import receive as receive
from .native.player import InputBase, Queue, Track, TrackHandle
from .native.player.input import AudioInput, RawPCMInput, StreamInput, SupportedCodec

__author__ = "sizumita"
__version__ = native.VERSION

__all__ = (
    "native",
    "receive",
    "player",
    "SongbirdClient",
    "InputBase",
    "Queue",
    "Track",
    "TrackHandle",
    "SupportedCodec",
    "AudioInput",
    "RawPCMInput",
    "StreamInput",
)
