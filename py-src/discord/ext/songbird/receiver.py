from __future__ import annotations
from abc import ABC, abstractmethod


class VoiceReceiver(ABC):
    """Base class for receiving voice data from Discord voice channels."""

    @abstractmethod
    def voice_tick(self, tick) -> None:
        """Called when a voice tick is received.

        Parameters
        ----------
        tick: VoiceTick
            The voice tick containing audio data from all speaking users.
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
