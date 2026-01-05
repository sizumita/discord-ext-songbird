from . import native
from .client import SongbirdClient as SongbirdClient
from .native import error as error
from .native import model as model
from .native import player as player
from .native import receive as receive

InputBase = player.InputBase
AudioInput = player.AudioInput
RawPCMInput = player.RawPCMInput
StreamInput = player.StreamInput
SupportedCodec = player.SupportedCodec
Track = player.Track
TrackHandle = player.TrackHandle
Queue = player.Queue

PySongbirdError = error.PySongbirdError
PyPlayerError = error.PyPlayerError
PyJoinError = error.PyJoinError
PyControlError = error.PyControlError

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
    "PySongbirdError",
    "PyPlayerError",
    "PyJoinError",
    "PyControlError",
)
