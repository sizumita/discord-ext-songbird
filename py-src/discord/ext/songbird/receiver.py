from __future__ import annotations
from abc import ABC, abstractmethod
from typing import Optional, Dict, Any, List
import discord


class VoicePacket:
    """Represents a voice packet received from Discord."""
    
    def __init__(self, ssrc: int, sequence: Optional[int], timestamp: Optional[int], 
                 opus_data: bytes, rtp_data: bytes, decoded_voice: Optional[bytes]):
        self.ssrc = ssrc
        self.sequence = sequence
        self.timestamp = timestamp
        self.opus_data = opus_data
        self.rtp_data = rtp_data
        self.decoded_voice = decoded_voice


class VoiceReceiver(ABC):
    """Base class for receiving voice data from Discord voice channels."""
    
    @abstractmethod
    async def voice_packet(self, ssrc: int, packet: VoicePacket) -> None:
        """Called when a voice packet is received.
        
        Parameters
        ----------
        ssrc: int
            The synchronization source identifier of the user who sent this packet.
        packet: VoicePacket
            The voice packet containing audio data.
        """
        pass
    
    async def speaking_update(self, ssrc: int, user_id: int, speaking: bool) -> None:
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
    
    
    async def driver_connect(self) -> None:
        """Called when the driver successfully connects to voice."""
        pass
    
    async def driver_disconnect(self) -> None:
        """Called when the driver disconnects from voice."""
        pass
    
    async def driver_reconnect(self) -> None:
        """Called when the driver reconnects to voice."""
        pass