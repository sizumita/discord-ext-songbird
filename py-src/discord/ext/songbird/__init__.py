from . import native
from .client import SongbirdClient as SongbirdClient
from .native import receive

__author__ = "sizumita"
__version__ = native.VERSION

__all__ = (
    "native",
    "receive",
    "SongbirdClient",
)
