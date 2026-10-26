"""GPT-Live (``gpt-live-1``) voice bridge (design §9.11).

Type ``!live`` while in a voice channel: the bot joins and bridges the channel to a GPT-Live session
over its primary WebSocket. The uplink is the continuous, silence-filled mix of the channel (the
first speaker holds the floor); the model's audio is played back with wall-clock gap filling, and a
local barge-in drops the rest of an interrupted utterance.

Environment: ``DISCORD_BOT_TOKEN``, ``OPENAI_API_KEY``. Requires ``discord.py`` and ``websockets``.

The protocol has no handshake acknowledgement: the client sends ``session.start`` and streams
``session.input_audio.append`` right away; only the first server message is subject to a timeout.
``start_ms`` on output deltas is optional; gap filling and gap-based barge-in apply when present.
"""

from __future__ import annotations

import asyncio
import collections
import json
import logging
import os
from typing import Any

import discord
import websockets
from discord.ext import songbird
from discord.ext.songbird import GPT_LIVE, PlaybackFull, VoiceConfig, VoiceEvent
from websockets.asyncio.client import connect as ws_connect

log = logging.getLogger("gpt_live")
LIVE_URL = "wss://api.openai.com/v1/live/sessions"
CHUNK_MS = 40
MAX_FILL_MS = 2_000  # longest uplink gap we cover with silence; beyond it we skip to live


async def gpt_live_bridge(
    vc: songbird.SongbirdClient,
    focus: int | None,
    *,
    url: str = LIVE_URL,
    api_key: str | None = None,
) -> str:
    """Bridge ``vc`` to one GPT-Live session until it ends; returns why it ended."""
    headers = {"Authorization": f"Bearer {api_key or os.environ['OPENAI_API_KEY']}"}
    async with (
        ws_connect(url, additional_headers=headers, max_size=None) as ws,
        # Single-caller model: mix only the focused member, else the first speaker holds the floor.
        vc.listen_mixed(
            GPT_LIVE,
            chunk_ms=CHUNK_MS,
            max_queue_ms=1_000,
            include={focus} if focus else None,
            policy="floor",
        ) as mic,
        vc.open_playback(GPT_LIVE, max_buffer_ms=60_000) as speaker,  # starts paused: silent until a delta
        vc.events(kinds=[VoiceEvent.SpeakingStarted]) as events,
    ):
        await ws.send(
            json.dumps(
                {
                    "type": "session.start",
                    "session": {
                        "model": "gpt-live-1",
                        "instructions": "You are a concise, friendly voice assistant in a Discord voice channel.",
                        "audio": {"format": GPT_LIVE.openai_format},
                    },
                }
            )
        )
        mic.skip_to_live()  # session time 0 == the next chunk (start_ms == 0)
        sent_ms = 0  # the provider's input timeline: what we appended
        spoken: collections.deque[tuple[int, int, frozenset[int]]] = collections.deque(maxlen=1_500)  # 60 s
        fill = GPT_LIVE.silence_base64(CHUNK_MS)  # one chunk of silence, encoded once

        async def append(b64: str, ms: int) -> None:
            nonlocal sent_ms
            await ws.send(json.dumps({"type": "session.input_audio.append", "audio": b64}))
            sent_ms += ms

        async def uplink() -> None:  # continuous, silence-filled, real-time paced; no acks
            try:
                async for chunk in mic:
                    if chunk.dropped_ms > MAX_FILL_MS:  # long uplink stall: catch up, accept the discontinuity
                        mic.skip_to_live(reset_clock=False)
                    else:  # keep the session timeline aligned, chunk by chunk
                        for _ in range(chunk.dropped_ms // CHUNK_MS):
                            await append(fill, CHUNK_MS)
                    spoken.append((sent_ms, sent_ms + CHUNK_MS, chunk.speakers))
                    await append(chunk.to_base64(), CHUNK_MS)
            except websockets.ConnectionClosed:
                return  # the downlink reports why the session ended

        async def local_barge_in() -> None:  # optional: the model is full duplex and yields itself
            async for ev in events:
                match ev:
                    case VoiceEvent.SpeakingStarted(user_id=uid) if focus in (None, uid) and speaker.buffered_ms > 400:
                        # Drop the rest of the interrupted utterance (deltas contiguous on the session
                        # timeline); the next reply starts after a gap and plays normally.
                        speaker.clear(reject_until_gap_ms=250)

        def who(ms: int) -> list[int]:  # transcript start_ms → Discord speaker(s)
            return sorted(next((s for a, b, s in reversed(spoken) if a <= ms < b), frozenset()))

        def handle(ev: dict[str, Any]) -> str | None:
            match ev.get("type"):
                case "session.output_audio.delta":
                    try:  # never await in the reader task
                        speaker.write_base64_nowait(ev["delta"], at_ms=ev.get("start_ms"))
                    except PlaybackFull:
                        log.warning("playback overrun; delta dropped")
                case "session.input_transcript.delta":
                    log.info("caller %s: %s", who(ev.get("start_ms", sent_ms)), ev.get("delta"))
                case "session.output_transcript.delta":
                    log.info("assistant: %s", ev.get("delta"))
                case "session.closed":
                    return str(ev.get("reason", "closed"))
                case "error":
                    raise RuntimeError(f"gpt-live error: {ev}")  # fatal: the session is unusable
            return None

        async def downlink() -> str:
            try:
                async with asyncio.timeout(10):  # the only handshake check: any first server message
                    first = json.loads(await ws.recv())
                if (reason := handle(first)) is not None:
                    return reason
                async for raw in ws:
                    if (reason := handle(json.loads(raw))) is not None:
                        return reason
            except websockets.ConnectionClosed:
                pass
            return "websocket_closed"

        async with asyncio.TaskGroup() as tg:
            up = tg.create_task(uplink())  # right after session.start: nothing to wait for
            bi = tg.create_task(local_barge_in())
            try:
                reason = await downlink()
            finally:
                up.cancel()
                bi.cancel()
        await speaker.drain(timeout=5)  # let the last words play out
        return reason


intents = discord.Intents.default()
intents.message_content = True
bot = discord.Client(intents=intents)


@bot.event
async def on_message(msg: discord.Message) -> None:
    if msg.content != "!live" or not isinstance(msg.author, discord.Member):
        return
    if msg.author.voice is None or msg.author.voice.channel is None:
        await msg.reply("Join a voice channel first.")
        return
    vc = await songbird.connect(msg.author.voice.channel, config=VoiceConfig.low_latency())
    try:
        await msg.channel.send(f"session ended: {await gpt_live_bridge(vc, msg.author.id)}")
    finally:
        await vc.disconnect()


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    bot.run(os.environ["DISCORD_BOT_TOKEN"])
