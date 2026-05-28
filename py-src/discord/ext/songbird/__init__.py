from . import native
from .client import SongbirdClient as SongbirdClient
from .native import error as error
from .native import model as model
from .native import player as player
from .native import receive as receive

InputBase = player.InputBase
AudioInput = player.AudioInput
OpusPacketInput = player.OpusPacketInput
OpusPacketStreamInput = player.OpusPacketStreamInput
RawPCMInput = player.RawPCMInput
StreamInput = player.StreamInput
Track = player.Track
TrackHandle = player.TrackHandle
Queue = player.Queue
supported_codecs = player.supported_codecs

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
    "AudioInput",
    "OpusPacketInput",
    "OpusPacketStreamInput",
    "RawPCMInput",
    "StreamInput",
    "supported_codecs",
    "PySongbirdError",
    "PyPlayerError",
    "PyJoinError",
    "PyControlError",
)
