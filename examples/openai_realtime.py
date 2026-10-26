"""OpenAI Realtime (``gpt-realtime`` family) voice bridge (design §9.13).

Type ``!realtime`` while in a voice channel. Server VAD runs on OpenAI's side; when the caller
starts speaking (``input_audio_buffer.speech_started``) the bot clears its playback and truncates
every interrupted item at the audio that was actually heard, so the conversation history matches
what the caller heard.

Environment: ``DISCORD_BOT_TOKEN``, ``OPENAI_API_KEY``. Requires ``discord.py`` and ``websockets``
(raw WebSocket; no SDK).
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
from discord.ext.songbird import OPENAI_REALTIME, PlaybackFull, VoiceConfig
from websockets.asyncio.client import connect as ws_connect

log = logging.getLogger("openai_realtime")
REALTIME_URL = "wss://api.openai.com/v1/realtime?model=gpt-realtime"


async def realtime_bridge(
    vc: songbird.SongbirdClient,
    *,
    url: str = REALTIME_URL,
    api_key: str | None = None,
    instructions: str = "You are a concise, friendly voice assistant in a Discord voice channel.",
) -> str:
    """Bridge ``vc`` to one Realtime session until it ends; returns why it ended."""
    headers = {"Authorization": f"Bearer {api_key or os.environ['OPENAI_API_KEY']}"}
    async with (
        ws_connect(url, additional_headers=headers, max_size=None) as ws,
        vc.listen_mixed(OPENAI_REALTIME, chunk_ms=40, max_queue_ms=1_000) as mic,
        vc.open_playback(OPENAI_REALTIME) as speaker,
    ):
        await ws.send(
            json.dumps(
                {
                    "type": "session.update",
                    "session": {
                        "type": "realtime",
                        "instructions": instructions,
                        "audio": {
                            "input": {
                                "format": OPENAI_REALTIME.openai_format,
                                "turn_detection": {"type": "server_vad"},
                            },
                            "output": {"format": OPENAI_REALTIME.openai_format},
                        },
                    },
                }
            )
        )
        mic.skip_to_live()

        async def uplink() -> None:
            try:
                async for chunk in mic:
                    await ws.send(json.dumps({"type": "input_audio_buffer.append", "audio": chunk.to_base64()}))
            except websockets.ConnectionClosed:
                return

        async def barge_in() -> None:
            # Every open item (even one whose queue is momentarily empty) is truncated at what was
            # heard; late deltas of these items are dropped by clear(reject_late=True).
            for item in speaker.clear().items:
                await ws.send(
                    json.dumps(
                        {
                            "type": "conversation.item.truncate",
                            "item_id": item.tag,
                            "content_index": 0,
                            "audio_end_ms": item.heard_ms,  # ≤ written by construction
                        }
                    )
                )

        async def handle(ev: dict[str, Any]) -> str | None:
            match ev.get("type"):
                case "response.output_audio.delta":
                    try:  # late deltas of cleared items are dropped
                        speaker.write_base64_nowait(ev["delta"], tag=ev["item_id"])
                    except PlaybackFull:
                        log.warning("playback overrun; delta dropped")
                case "response.output_audio.done":
                    speaker.end_item(ev["item_id"])  # fully sent; `complete` once played
                case "input_audio_buffer.speech_started":
                    await barge_in()
                case "response.output_audio_transcript.done":
                    log.info("assistant: %s", ev.get("transcript"))
                case "conversation.item.input_audio_transcription.completed":
                    log.info("caller: %s", ev.get("transcript"))
                case "error":
                    log.error("realtime error: %s", ev.get("error"))
            return None

        async def downlink() -> str:
            try:
                async for raw in ws:
                    if (reason := await handle(json.loads(raw))) is not None:
                        return reason
            except websockets.ConnectionClosed:
                pass
            return "websocket_closed"

        async with asyncio.TaskGroup() as tg:
            up = tg.create_task(uplink())
            try:
                reason = await downlink()
            finally:
                up.cancel()
        await speaker.drain(timeout=5)
        return reason


intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)


@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.content != "!realtime" or not isinstance(msg.author, discord.Member):
        return
    if msg.author.voice is None or msg.author.voice.channel is None:
        await msg.reply("Join a voice channel first.")
        return
    vc = await songbird.connect(msg.author.voice.channel, config=VoiceConfig.low_latency())
    try:
        await msg.channel.send(f"session ended: {await realtime_bridge(vc)}")
    finally:
        await vc.disconnect()


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
