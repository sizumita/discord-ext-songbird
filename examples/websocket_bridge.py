"""Generic WebSocket audio bridge (design §9.12): Discord voice <-> a service that streams raw PCM.

The protocol is deliberately minimal and not tied to any particular service:

* bot -> service: one binary frame per received chunk of raw audio (``input_format``, ``chunk_ms``).
* service -> bot: binary frames of raw audio in ``output_format``, played as they arrive.
* service -> bot: the text frame ``"clear"`` is an interruption: queued playback is discarded.

Adapting it to a specific service usually only changes the framing (wrapping chunks in the
service's own messages and unwrapping the audio it sends back); the library calls stay the same.
Pick the formats and chunk size the service expects; 16 or 24 kHz mono PCM16 is common for speech.

Type ``!bridge`` while in a voice channel; ``!stop`` ends it. Environment: ``DISCORD_BOT_TOKEN``,
``BRIDGE_URL`` (e.g. ``ws://localhost:8765``), optionally ``BRIDGE_FORMAT`` (a format spec such as
``pcm16_16k``; default ``pcm16_48k_stereo``, Discord's native format) and ``BRIDGE_CHUNK_MS``
(default 20). Requires ``discord.py`` and ``websockets``.
"""

from __future__ import annotations

import asyncio
import logging
import os
from typing import Any

import discord
import websockets
from discord.ext import songbird
from discord.ext.songbird import PCM16_48K_STEREO, FormatLike, PlaybackFull, StreamClosed, VoiceConfig
from websockets.asyncio.client import connect as ws_connect

log = logging.getLogger("websocket_bridge")

#: When several ends happen together, report the first of these (deterministic).
_REASONS = ("stopped", "voice_closed", "websocket_closed")


async def bridge(
    vc: songbird.SongbirdClient,
    url: str,
    *,
    input_format: FormatLike = PCM16_48K_STEREO,
    output_format: FormatLike | None = None,
    chunk_ms: int = 20,
    user: Any | None = None,
    stop: asyncio.Event | None = None,
) -> str:
    """Stream ``vc``'s audio to the WebSocket at ``url`` and play what comes back.

    Parameters
    ----------
    vc : SongbirdClient
        A connected client.
    url : str
        The WebSocket to connect to.
    input_format : AudioFormat or str
        Format of the audio sent to the service.
    output_format : AudioFormat or str or None
        Format of the audio the service sends back (``None``: same as ``input_format``).
    chunk_ms : int
        Duration of each binary frame sent.
    user : user or None
        Send only this member's voice; ``None`` sends the channel mix.
    stop : asyncio.Event or None
        Ends the bridge when set.

    Returns
    -------
    str
        Why it ended: ``"websocket_closed"``, ``"voice_closed"`` or ``"stopped"``.
    """
    out_format = input_format if output_format is None else output_format
    async with (
        ws_connect(url, max_size=None) as ws,
        vc.listen(input_format, user=user, chunk_ms=chunk_ms, max_queue_ms=1_000) as mic,
        vc.open_playback(out_format) as speaker,
    ):
        mic.skip_to_live()  # drop what was captured while connecting

        async def uplink() -> str:
            try:
                async for chunk in mic:  # continuous: silence while nobody speaks
                    await ws.send(bytes(chunk))  # one binary frame per chunk
            except websockets.ConnectionClosed:
                return "websocket_closed"
            return "voice_closed"

        async def downlink() -> str:
            try:
                async for message in ws:
                    if isinstance(message, str):
                        if message == "clear":
                            speaker.clear()  # interruption: silent within one or two ticks
                        continue
                    try:
                        speaker.write_nowait(message)  # never blocks the reader
                    except PlaybackFull:
                        log.warning("playback overrun; %d bytes dropped", len(message))
            except websockets.ConnectionClosed:
                pass
            except StreamClosed:
                return "voice_closed"  # the voice connection closed while audio was arriving
            return "websocket_closed"

        async def stopped() -> str:
            await (stop or asyncio.Event()).wait()
            return "stopped"

        tasks = [asyncio.create_task(c) for c in (uplink(), downlink(), stopped())]
        try:
            done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        finally:
            for t in tasks:
                t.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
        reason = min((t.result() for t in done), key=_REASONS.index)  # re-raises an unexpected error
        if reason == "websocket_closed":
            await speaker.drain(timeout=5)  # let the audio already received finish playing
        return reason


intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)
sessions: dict[int, asyncio.Event] = {}


@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.guild is None or not isinstance(msg.author, discord.Member):
        return
    if msg.content == "!bridge" and msg.author.voice and msg.author.voice.channel and msg.guild.id not in sessions:
        vc = await songbird.connect(msg.author.voice.channel, config=VoiceConfig.low_latency())
        stop = sessions[msg.guild.id] = asyncio.Event()
        try:
            reason = await bridge(
                vc,
                os.environ["BRIDGE_URL"],
                input_format=os.environ.get("BRIDGE_FORMAT", "pcm16_48k_stereo"),
                chunk_ms=int(os.environ.get("BRIDGE_CHUNK_MS", "20")),
                stop=stop,
            )
            await msg.channel.send(f"bridge ended: {reason}")
        finally:
            sessions.pop(msg.guild.id, None)
            await vc.disconnect()
    elif msg.content == "!stop" and (ev := sessions.get(msg.guild.id)) is not None:
        ev.set()


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
