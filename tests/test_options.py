"""Creation-time options of the public wrappers reach the native layer.

The Python wrappers pass most options positionally to native methods whose parameters all have
defaults, so a swapped or dropped argument would otherwise go unnoticed: each option here changes
an observable result.
"""

from __future__ import annotations

import asyncio
from typing import Any

import pytest
from conftest import FakeUser, HarnessClient, drain, pump_settle, run_isolated, tone, voice_packets
from discord.ext import songbird
from discord.ext.songbird import ConnectionClosed, LimitExceeded, StreamClosed

A, B, BOT = 7_000_000_000_000_001, 7_000_000_000_000_002, 7_000_000_000_000_003
SSRC_A, SSRC_B, SSRC_BOT = 101, 102, 103
FMT = songbird.PCM16_24K


async def three_speakers(vc: HarnessClient, testing: Any) -> bytes:
    """Maps A, B and a bot (classified from the member cache); returns a voiced packet."""
    guild: Any = vc.guild  # the fake guild
    guild.members[BOT] = FakeUser(BOT, bot=True)
    for ssrc, user in ((SSRC_A, A), (SSRC_B, B), (SSRC_BOT, BOT)):
        testing.inject_map(vc._conn, ssrc, user)
    vc.harness.step(20)  # UserMapped → Control.ClassifyUsers
    await pump_settle()  # the pump answers from the member cache
    return voice_packets(testing)[0]


def everyone_speaks(vc: HarnessClient, testing: Any, pkt: bytes) -> None:
    testing.inject_tick(vc._conn, [(SSRC_A, pkt), (SSRC_B, pkt), (SSRC_BOT, pkt)], [])
    vc.harness.step(20)


async def test_listen_mixed_creation_filters(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pkt = await three_speakers(vc, testing)
    async with (
        vc.listen_mixed(FMT, include=[A, BOT]) as included,
        vc.listen_mixed(FMT, exclude=[A]) as excluded,
        vc.listen_mixed(FMT, ignore_bots=False) as with_bots,
        vc.listen_mixed(FMT) as default,
    ):
        everyone_speaks(vc, testing, pkt)
        speakers = {
            name: drain(s)[0].speakers
            for name, s in [
                ("included", included),
                ("excluded", excluded),
                ("with_bots", with_bots),
                ("default", default),
            ]
        }
    assert speakers == {
        "included": frozenset({A}),  # the bot is included but still ignored (ignore_bots=True)
        "excluded": frozenset({B}),
        "with_bots": frozenset({A, B, BOT}),
        "default": frozenset({A, B}),
    }


async def test_listen_users_creation_options(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    pkt = await three_speakers(vc, testing)
    async with (
        vc.listen_users(FMT, include=[A, BOT]) as only_a,
        vc.listen_users(FMT, ignore_bots=False, close_on_leave=False) as everyone,
    ):
        everyone_speaks(vc, testing, pkt)
        a = await anext(only_a)
        assert a.user_id == A
        kept = {s.user_id: s for s in [await anext(everyone) for _ in range(3)]}
        assert set(kept) == {A, B, BOT}
        await pump_settle()
        assert only_a._router.try_next() is None  # B is not included, the bot is ignored
        testing.inject_user_left(vc._conn, A)
        vc.harness.step(20)
        assert a.closed and a.close_reason == "user_left"  # close_on_leave defaults to True
        assert not kept[A].closed  # close_on_leave=False
        for s in kept.values():
            s.close()


async def test_user_streams_close_can_keep_its_streams(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    testing.inject_map(vc._conn, SSRC_A, A)
    pkt = voice_packets(testing)[0]
    for close_streams in (False, True):
        users = vc.listen_users(FMT)
        testing.inject_tick(vc._conn, [(SSRC_A, pkt)], [])
        vc.harness.step(20)
        stream = await anext(users)
        await users.aclose(close_streams=close_streams)
        assert stream.closed is close_streams
        with pytest.raises(StopAsyncIteration):
            await anext(users)
        stream.close()


async def test_open_playback_when_disconnected(connected_vc: HarnessClient, testing: Any) -> None:
    vc = connected_vc
    discard = vc.open_playback(FMT, when_disconnected="discard")
    buffer = vc.open_playback(FMT)  # "buffer"
    for pb in (discard, buffer):
        pb.write_nowait(tone(100, rate=24_000, channels=1))
    testing.inject_driver_event(vc._conn, "disconnect", reason="io")  # the voice link drops
    vc.harness.step(20)
    assert (discard.buffered_ms, buffer.buffered_ms) == (0, 100)
    discard.write_nowait(tone(40, rate=24_000, channels=1))  # dropped while the link is down
    assert discard.buffered_ms == 0 and discard.stats().rejected_ms == 40
    discard.close()
    buffer.close()


async def test_open_playback_auto_pause(connected_vc: HarnessClient, testing: Any) -> None:
    logs = {}
    for auto_pause in (True, False):
        pb = connected_vc.open_playback(FMT, idle_ms=60, auto_pause=auto_pause)
        mixer = testing.MixerHarness(pb._pb)
        pb.write_nowait(tone(40, rate=24_000, channels=1))
        for _ in range(10):  # 40 ms of audio, then idle
            mixer.pull(1)
            connected_vc.harness.step(20)
        assert pb.is_idle
        logs[auto_pause] = testing.track_log(pb.track._native)
        pb.close()
    assert logs[True][-1] == "pause"
    assert "pause" not in logs[False]


async def test_set_bitrate(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    vc.set_bitrate(64_000)
    vc.set_bitrate(None)
    for bad in (5_999, 510_001):
        with pytest.raises(LimitExceeded):
            vc.set_bitrate(bad)
    conn = vc._conn
    assert conn is not None
    await vc.disconnect()
    with pytest.raises(ConnectionClosed):
        conn.set_bitrate(64_000)


async def test_encoded_stream_finish(connected_vc: HarnessClient) -> None:
    vc = connected_vc
    stream = vc.open_encoded_stream()
    stream.finish()  # end of input: no more writes
    with pytest.raises(StreamClosed):
        stream.write_nowait(b"RIFF")
    stream.close()


async def test_track_error_reaches_track_end(connected_vc: HarnessClient, testing: Any) -> None:
    """songbird's ``TrackEvent::Error`` ends a track ``"error"`` with its message: through the
    ``TrackEnded`` event, ``end_reason`` and ``wait_ended()``'s ``TrackEnd``."""
    vc = connected_vc
    async with vc.events(kinds=[songbird.VoiceEvent.TrackEnded]) as events:
        track = vc.play(songbird.OpusPackets([b"\xf8\xff\xfe"] * 50))
        waiter = asyncio.create_task(track.wait_ended())
        await pump_settle()
        assert testing.fail_track(track._native, "decoder exploded")
        assert not testing.fail_track(track._native, "later")  # the first end wins
        vc.harness.step(20)
        end = await asyncio.wait_for(waiter, 2)
        ev = await asyncio.wait_for(anext(events), 2)
    assert end == songbird.TrackEnd("error", "decoder exploded")
    assert (track.end_reason, track.ended) == ("error", True)
    assert (ev.track_id, ev.reason) == (track.id, "error")


def test_configure_runtime_after_the_first_connection_raises() -> None:
    proc = run_isolated(
        """
        import socket
        from discord.ext import songbird
        from discord.ext.songbird import native

        songbird.configure_runtime(net_workers=3, rx_workers=2)  # before any connection
        r, w = socket.socketpair()
        conn = native.Connection(native.WakeBell(w.detach()), 1, 2, songbird.VoiceConfig().to_native(), True)
        try:
            songbird.configure_runtime(net_workers=2)
        except RuntimeError as e:
            print("RAISED", e)
        conn.close("requested")
        """
    )
    assert proc.returncode == 0, proc.stderr
    assert "RAISED" in proc.stdout
