"""Contract checks that run against both native modules (``SONGBIRD_TEST_NATIVE=real`` and
``fake``): behaviours where the pure-Python fake once differed from the real module, so a test or a
Python-layer change written against the fake cannot silently rely on fake-only semantics."""

from __future__ import annotations

from typing import Any

import pytest
from conftest import HarnessClient, drain, inject_ticks, voice_packets
from discord.ext import songbird
from discord.ext.songbird import InvalidAudio, LimitExceeded, RawPcm, StreamClosed, VoiceEvent

A = 7_000_000_000_000_001
SSRC_A = 101


async def test_close_discards_queued_chunks(connected_vc: HarnessClient, testing: Any) -> None:
    """An explicit ``close()`` frees what is queued and ends iteration at once; a close by the
    router (or the connection, ``test_streams_end_on_disconnect``) keeps the chunks readable."""
    vc = connected_vc
    stream = vc.listen_mixed(songbird.PCM16_24K)
    inject_ticks(vc, testing, 5, voiced=False)
    assert stream.queued_ms == 100
    stream.close()
    assert stream.closed and stream.close_reason == "closed"
    assert stream.queued_ms == 0
    assert [c async for c in stream] == []
    with pytest.raises(StreamClosed):
        stream.read_nowait()

    testing.inject_map(vc._conn, SSRC_A, A)
    pkt = voice_packets(testing)[0]
    users = vc.listen_users(songbird.PCM16_24K)
    for _ in range(3):
        testing.inject_tick(vc._conn, [(SSRC_A, pkt)], [])
        vc.harness.step(20)
    per_user = await anext(users)
    users.close()  # closes the streams it created, which keep their queued chunks
    assert per_user.closed
    assert len([c async for c in per_user]) == 3


async def test_event_stream_close_discards_queued_events(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    events = vc.events(kinds=[VoiceEvent.UserMapped])
    testing.inject_map(vc._conn, SSRC_A, A)
    vc.harness.step(20)
    events.close()
    assert [e async for e in events] == []
    with pytest.raises(StreamClosed):
        events.read_nowait()


async def test_user_id_zero_is_rejected(connected_vc: HarnessClient) -> None:
    with pytest.raises(ValueError, match="non-zero"):
        connected_vc.listen_user(0)


def test_raw_pcm_rejects_a_partial_frame() -> None:
    with pytest.raises(InvalidAudio):
        RawPcm(b"\x00\x01\x02", songbird.PCM16_24K)
    with pytest.raises(InvalidAudio):
        RawPcm(bytes(6), songbird.PCM16_48K_STEREO)  # 1.5 frames
    with pytest.raises(InvalidAudio):
        RawPcm.from_samples(bytes(6), 48_000, 2, "pcm16le")
    assert RawPcm(bytes(8), songbird.PCM16_48K_STEREO).duration_ms == 0  # 2 frames


async def test_read_batch_cap(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    stream = vc.listen_mixed(songbird.PCM16_24K)
    with pytest.raises(LimitExceeded):
        stream.read_batch(6_001)
    with pytest.raises(LimitExceeded):
        stream.read_batch(0)
    assert stream.read_batch(6_000) is None  # nothing queued
    inject_ticks(vc, testing, 2, voiced=False)
    assert len(drain(stream)) == 2
    stream.close()
