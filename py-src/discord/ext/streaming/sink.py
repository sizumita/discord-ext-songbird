from .abc import AsyncStreamWriterABC
from .packet import Packet

class PacketStream(AsyncStreamWriterABC[Packet]):
    async def __anext__(self) -> Packet:
        pass

class SsrcPacketSink:
    def get_ssrc_stream(self, ssrc: int) -> AsyncStreamWriterABC[Packet]:
        pass
