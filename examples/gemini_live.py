"""Gemini Live (``BidiGenerateContent`` WebSocket) voice bridge (design §9.13).

Type ``!gemini`` while in a voice channel. Input is 16 kHz PCM16 in ~100 ms chunks, output 24 kHz
PCM16; playback is flushed when the server reports ``interrupted``. The Discord streams survive
provider reconnects (``goAway``): after each new ``setup`` the uplink calls ``mic.skip_to_live()``.

Environment: ``DISCORD_BOT_TOKEN``, ``GEMINI_API_KEY``, optionally ``GEMINI_MODEL``. Requires
``discord.py`` and ``websockets`` (raw WebSocket; no SDK).
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
from typing import Any

import discord
import websockets
from discord.ext import songbird
from discord.ext.songbird import GEMINI_LIVE_INPUT, GEMINI_LIVE_OUTPUT, AudioStream, PlaybackFull, PlaybackStream
from websockets.asyncio.client import connect as ws_connect

log = logging.getLogger("gemini_live")
LIVE_URL = (
    "wss://generativelanguage.googleapis.com/ws/"
    "google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent"
)
DEFAULT_MODEL = "models/gemini-live-2.5-flash-preview"


async def gemini_session(
    mic: AudioStream,
    speaker: PlaybackStream,
    *,
    url: str = LIVE_URL,
    api_key: str | None = None,
    model: str = DEFAULT_MODEL,
) -> str:
    """One provider session over existing Discord streams. Returns ``"go_away"`` when the server
    asks the client to reconnect, otherwise why it ended."""
    headers = {"x-goog-api-key": api_key or os.environ["GEMINI_API_KEY"]}
    async with ws_connect(url, additional_headers=headers, max_size=None) as ws:
        await ws.send(json.dumps({"setup": {"model": model, "generationConfig": {"responseModalities": ["AUDIO"]}}}))
        async with asyncio.timeout(10):
            while "setupComplete" not in json.loads(await ws.recv()):
                pass
        mic.skip_to_live()  # the new session starts at the next chunk

        async def uplink() -> None:
            try:
                async for chunk in mic:
                    await ws.send(
                        json.dumps(
                            {
                                "realtimeInput": {
                                    "audio": {"data": chunk.to_base64(), "mimeType": GEMINI_LIVE_INPUT.mime_type}
                                }
                            }
                        )
                    )
            except websockets.ConnectionClosed:
                return

        def handle(msg: dict[str, Any]) -> str | None:
            if "goAway" in msg:
                return "go_away"
            content = msg.get("serverContent") or {}
            if content.get("interrupted"):
                speaker.clear()
            for part in (content.get("modelTurn") or {}).get("parts", []):
                if (blob := part.get("inlineData")) is not None:
                    try:
                        speaker.write_base64_nowait(blob["data"])
                    except PlaybackFull:
                        log.warning("playback overrun; audio dropped")
            return None

        async with asyncio.TaskGroup() as tg:
            up = tg.create_task(uplink())
            try:
                async for raw in ws:
                    if (reason := handle(json.loads(raw))) is not None:
                        return reason
            except websockets.ConnectionClosed:
                pass
            finally:
                up.cancel()
        return "websocket_closed"


async def gemini_bridge(
    vc: songbird.SongbirdClient,
    *,
    url: str = LIVE_URL,
    api_key: str | None = None,
    model: str = DEFAULT_MODEL,
    max_sessions: int = 10,
) -> str:
    """Bridge ``vc`` to Gemini Live, reconnecting on ``goAway``; returns why it ended."""
    reason = "not_started"
    async with (
        vc.listen_mixed(GEMINI_LIVE_INPUT, chunk_ms=100) as mic,
        vc.open_playback(GEMINI_LIVE_OUTPUT) as speaker,
    ):
        for _ in range(max_sessions):
            reason = await gemini_session(mic, speaker, url=url, api_key=api_key, model=model)
            if reason != "go_away":
                break
        await speaker.drain(timeout=5)
    return reason


intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)


@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.content != "!gemini" or not isinstance(msg.author, discord.Member):
        return
    if msg.author.voice is None or msg.author.voice.channel is None:
        await msg.reply("Join a voice channel first.")
        return
    vc = await songbird.connect(msg.author.voice.channel, config=songbird.VoiceConfig.low_latency())
    try:
        model = os.environ.get("GEMINI_MODEL", DEFAULT_MODEL)
        await msg.channel.send(f"session ended: {await gemini_bridge(vc, model=model)}")
    finally:
        await vc.disconnect()


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
