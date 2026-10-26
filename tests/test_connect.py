"""discord.py VoiceProtocol behaviour (§6, §13.3 ``test_connect.py``).

Every test drives discord.py's own ``Connectable.connect`` against a detached native connection; the
fake guild records each op 4 and plays the gateway.
"""

from __future__ import annotations

import asyncio
from typing import Any

import pytest
from conftest import (
    CHANNEL_ID,
    GUILD_ID,
    OTHER_CHANNEL_ID,
    SSRC,
    USER_ID,
    HarnessClient,
    Op4,
    World,
    client_factory,
    connect_detached,
    pump_settle,
    wait_for,
)
from discord.ext import songbird
from discord.ext.songbird import (
    ConnectFailed,
    ConnectionClosed,
    ConnectTimeout,
    NotConnected,
    ReceiveDisabled,
    SongbirdClient,
    VoiceConfig,
    VoiceEvent,
    native,
)

pytestmark = pytest.mark.usefixtures("testing")


def registered(world: World) -> Any:
    return world.client._connection._get_voice_client(GUILD_ID)


async def test_one_op4_with_flags(world: World) -> None:
    vc = await connect_detached(world, self_mute=True, self_deaf=True)
    try:
        assert world.guild.op4 == [Op4(CHANNEL_ID, True, True)]
        assert vc.is_connected()
        st = vc.status
        assert (st.state, st.channel_id, st.ssrc) == ("connected", CHANNEL_ID, SSRC)
        assert (vc.self_mute, vc.self_deaf) == (True, True)
        assert registered(world) is vc
    finally:
        await vc.disconnect()
    assert world.guild.op4[-1] == Op4(None, False, False)
    assert registered(world) is None


async def test_user_id_is_client_user(world: World, monkeypatch: pytest.MonkeyPatch) -> None:
    seen: dict[str, Any] = {}

    class Boom(Exception):
        pass

    def fake_connection(bell: Any, guild_id: int, user_id: int, settings: Any, reconnect: bool) -> Any:
        seen.update(guild_id=guild_id, user_id=user_id, reconnect=reconnect)
        raise Boom

    monkeypatch.setattr(native, "Connection", fake_connection)
    with pytest.raises(Boom):
        await world.channel.connect(cls=SongbirdClient, timeout=1.0, reconnect=False)
    assert seen == {"guild_id": GUILD_ID, "user_id": USER_ID, "reconnect": False}
    assert registered(world) is None
    assert world.guild.op4 == []  # nothing was sent before the connection existed


async def test_explicit_flag_before_connect_wins(world: World, testing: Any) -> None:
    vc = HarnessClient(world.client, world.channel)
    await vc.set_self_mute(True)
    assert world.guild.op4 == []  # nothing sent before connect
    world.client._connection._add_voice_client(GUILD_ID, vc)

    async def drive() -> None:
        await wait_for(lambda: vc._conn is not None and vc._conn.status().state == "connecting")
        testing.inject_driver_event(vc._conn, "connect", ssrc=SSRC)

    task = asyncio.create_task(drive())
    await vc.connect(timeout=5, reconnect=True, self_mute=False)
    await task
    assert world.guild.op4 == [Op4(CHANNEL_ID, True, False)]
    await vc.disconnect()


async def test_flag_change_while_connecting_is_flushed_once(world: World, testing: Any) -> None:
    world.guild.send_server_update = False  # stay in "joining"
    task = asyncio.create_task(world.channel.connect(cls=client_factory(), timeout=5, reconnect=True))
    await wait_for(lambda: registered(world) is not None and registered(world)._conn is not None)
    vc: HarnessClient = registered(world)
    await world.guild.settle()
    await vc.set_self_mute(True)
    await vc.set_self_deaf(True)
    assert world.guild.op4 == [Op4(CHANNEL_ID, False, False)]  # only the join so far
    assert vc._conn is not None
    vc._conn.on_voice_server("token", "wss://fake-voice.discord.media")
    testing.inject_driver_event(vc._conn, "connect", ssrc=SSRC)
    assert await task is vc
    await wait_for(lambda: len(world.guild.op4) == 2)
    await pump_settle()
    assert world.guild.op4 == [Op4(CHANNEL_ID, False, False), Op4(CHANNEL_ID, True, True)]
    await vc.set_self_mute(True)  # unchanged: nothing more is sent
    assert len(world.guild.op4) == 2
    await vc.disconnect()


async def test_set_flags_while_connected_sends_op4(world: World) -> None:
    vc = await connect_detached(world)
    await vc.set_self_deaf(True)
    assert world.guild.op4[-1] == Op4(CHANNEL_ID, False, True)
    assert vc.status.self_deaf
    await vc.disconnect()


async def test_timeout_cleans_up_and_leaves(world: World) -> None:
    world.guild.send_server_update = False
    with pytest.raises(ConnectTimeout) as ei:
        await world.channel.connect(cls=client_factory(), timeout=0.2, reconnect=True)
    e = ei.value
    assert isinstance(e, asyncio.TimeoutError)
    assert (e.reason, e.code, e.timeout) == ("timeout", None, 0.2)
    assert registered(world) is None
    await world.guild.settle()
    await wait_for(lambda: Op4(None, False, False) in world.guild.op4)
    # A second connect does not raise "Already connected".
    world.guild.send_server_update = True
    vc = await connect_detached(world)
    assert vc.is_connected()
    await vc.disconnect()


async def test_stale_leave_never_follows_the_new_join(world: World) -> None:
    world.guild.send_server_update = False
    world.guild.leave_delay = 1.0  # the best-effort leave of the aborted connect is slow
    with pytest.raises(ConnectTimeout):
        await world.channel.connect(cls=client_factory(), timeout=0.1, reconnect=True)
    world.guild.send_server_update = True
    world.guild.leave_delay = 0.0
    vc = await connect_detached(world)
    channels = [op.channel_id for op in world.guild.op4]
    assert channels == [CHANNEL_ID, None, CHANNEL_ID]
    await vc.disconnect()


async def test_disconnect_during_connect(world: World) -> None:
    world.guild.send_server_update = False
    task = asyncio.create_task(world.channel.connect(cls=client_factory(), timeout=5, reconnect=True))
    await wait_for(lambda: registered(world) is not None and registered(world)._conn is not None)
    vc: HarnessClient = registered(world)
    await vc.disconnect()
    with pytest.raises(ConnectFailed) as ei:
        await task
    assert ei.value.reason == "requested"
    await world.guild.settle()
    await asyncio.sleep(0.05)
    assert [op.channel_id for op in world.guild.op4].count(None) == 1
    assert registered(world) is None


async def test_cancelled_connect_cleans_up(world: World) -> None:
    world.guild.send_server_update = False
    task = asyncio.create_task(world.channel.connect(cls=client_factory(), timeout=5, reconnect=True))
    await wait_for(lambda: registered(world) is not None and registered(world)._conn is not None)
    vc: HarnessClient = registered(world)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    assert registered(world) is None
    assert vc._conn is not None and vc._conn.closed
    assert vc._conn.status().close_reason == "connect_aborted"
    await world.guild.settle()
    await wait_for(lambda: Op4(None, False, False) in world.guild.op4)


async def test_terminal_failure_raises_connect_failed(world: World, testing: Any) -> None:
    async def fail() -> None:
        await wait_for(lambda: registered(world) is not None and registered(world)._conn is not None)
        vc = registered(world)
        await wait_for(lambda: vc._conn.status().state == "connecting")
        testing.inject_driver_event(vc._conn, "connect_failed")

    t = asyncio.create_task(fail())
    with pytest.raises(ConnectFailed) as ei:
        await world.channel.connect(cls=client_factory(), timeout=5, reconnect=False)
    await t
    assert ei.value.reason == "connect_failed"
    assert not isinstance(ei.value, ConnectTimeout)
    assert registered(world) is None


async def test_disconnect_is_idempotent_and_survives_op4_errors(world: World) -> None:
    vc = await connect_detached(world)
    world.guild.fail_next = RuntimeError("gateway down")
    await vc.disconnect()  # never raises for gateway errors
    assert registered(world) is None
    assert vc._conn is not None and vc._conn.closed
    await vc.disconnect()
    await vc.disconnect(force=True)
    vc.cleanup()


async def test_move_to_waits_for_the_new_channel(world: World, testing: Any) -> None:
    vc = await connect_detached(world)
    gen = vc.status.generation

    async def finish_move() -> None:
        await wait_for(lambda: vc.status.state == "connecting" and vc.status.target_channel_id == OTHER_CHANNEL_ID)
        testing.inject_driver_event(vc._conn, "connect", ssrc=SSRC + 1)

    t = asyncio.create_task(finish_move())
    await vc.move_to(world.other)
    await t
    st = vc.status
    assert (st.state, st.channel_id) == ("connected", OTHER_CHANNEL_ID)
    assert st.generation > gen
    assert vc.channel is world.other
    assert world.guild.op4[-1] == Op4(OTHER_CHANNEL_ID, False, False)
    await vc.move_to(world.other)  # already there: no-op
    await vc.move_to(None)  # == disconnect()
    assert registered(world) is None


async def test_move_to_timeout(world: World) -> None:
    vc = await connect_detached(world)
    world.guild.send_server_update = False
    with pytest.raises(ConnectTimeout):
        await vc.move_to(world.other, timeout=0.1)
    assert vc.status.state == "joining"  # it keeps trying
    await vc.disconnect()


async def test_leaving_the_channel_finalizes(world: World) -> None:
    vc = await connect_detached(world)
    async with vc.events() as events:
        await vc.on_voice_state_update({"channel_id": None, "session_id": "s"})  # type: ignore
        await pump_settle()
        await wait_for(lambda: registered(world) is None)
        seen = [ev async for ev in events]
    assert vc._conn is not None and vc.status.state == "closed"
    assert vc.status.close_reason == "left_channel"
    assert isinstance(seen[-1], VoiceEvent.Disconnected) and seen[-1].will_retry is False


async def test_kick_after_4014(world: World, testing: Any) -> None:
    vc = await connect_detached(world)
    testing.inject_driver_event(vc._conn, "disconnect", reason="ws_closed", code=4014)
    assert vc.status.state == "reconnecting"
    await vc.on_voice_state_update({"channel_id": None, "session_id": "s"})  # type: ignore
    await wait_for(lambda: registered(world) is None)
    assert vc.status.close_reason == "kicked"


async def test_4014_then_external_move_keeps_the_connection(world: World, testing: Any) -> None:
    vc = await connect_detached(world)
    testing.inject_driver_event(vc._conn, "disconnect", reason="ws_closed", code=4014)
    assert vc.status.state == "reconnecting"
    world.guild.session_seq += 1
    await vc.on_voice_state_update({"channel_id": str(OTHER_CHANNEL_ID), "session_id": "moved"})  # type: ignore
    await vc.on_voice_server_update({"token": "t2", "endpoint": "wss://other.discord.media", "guild_id": str(GUILD_ID)})
    await wait_for(lambda: vc.status.state == "connecting")
    testing.inject_driver_event(vc._conn, "connect", ssrc=SSRC)
    await wait_for(lambda: vc.is_connected())
    await pump_settle()
    assert vc.status.channel_id == OTHER_CHANNEL_ID
    assert vc.channel is world.other
    assert registered(world) is vc
    await vc.disconnect()


async def test_rejoin_control_sends_leave_then_join(world: World, testing: Any) -> None:
    vc = await connect_detached(world)
    world.guild.respond = False
    testing.inject_driver_event(vc._conn, "disconnect", reason="io")
    assert vc.status.state == "reconnecting"
    vc.harness.step(2_000)  # backoff elapses → Rejoining → Control.Rejoin
    await wait_for(lambda: len(world.guild.op4) >= 3)
    assert world.guild.op4[1:] == [Op4(None, False, False), Op4(CHANNEL_ID, False, False)]
    await vc.disconnect()


async def test_reconnect_forces_a_fresh_session(world: World, testing: Any) -> None:
    vc = await connect_detached(world)
    gen = vc.status.generation

    async def finish() -> None:
        await wait_for(lambda: vc.status.state == "connecting" and vc.status.generation > gen)
        testing.inject_driver_event(vc._conn, "connect", ssrc=SSRC)

    t = asyncio.create_task(finish())
    await vc.reconnect(timeout=5)
    await t
    assert vc.is_connected()
    assert vc.status.generation > gen
    assert [op.channel_id for op in world.guild.op4] == [CHANNEL_ID, None, CHANNEL_ID]
    await vc.disconnect()


async def test_wait_connected(world: World) -> None:
    vc = await connect_detached(world)
    await vc.wait_connected(timeout=1)
    await vc.disconnect()
    with pytest.raises(ConnectionClosed):
        await vc.wait_connected(timeout=1)


async def test_with_config_and_helper(world: World) -> None:
    vc = await connect_detached(world, config=VoiceConfig(receive=False))
    with pytest.raises(ReceiveDisabled):
        vc.listen()
    await vc.disconnect()
    factory = SongbirdClient.with_config(VoiceConfig(), receive=False)
    client = factory(world.client, world.channel)
    assert isinstance(client, SongbirdClient) and client._config.receive is False


async def test_not_connected_before_connect(world: World) -> None:
    vc = SongbirdClient(world.client, world.channel)
    with pytest.raises(NotConnected):
        vc.listen()
    with pytest.raises(NotConnected):
        _ = vc.status
    with pytest.raises(NotConnected):
        vc.open_playback()
    assert not vc.is_connected()
    assert vc.ssrc is None
    await vc.set_self_mute(True)  # never raises NotConnected
    await vc.disconnect()  # nothing to tear down
    assert world.guild.op4 == []


async def test_closed_connection_raises(world: World) -> None:
    vc = await connect_detached(world)
    await vc.disconnect()
    with pytest.raises(ConnectionClosed):
        vc.listen()
    with pytest.raises(ConnectionClosed):
        vc.open_playback()
    assert vc.status.state == "closed"


def test_guild_channels_only(world: World) -> None:
    class DM:
        def _get_voice_client_key(self) -> tuple[int, str]:
            return 1, "self_id"

    with pytest.raises(TypeError):
        SongbirdClient(world.client, DM())  # type: ignore


async def test_helper_connect(world: World, testing: Any, monkeypatch: pytest.MonkeyPatch) -> None:
    async def drive() -> None:
        await wait_for(lambda: registered(world) is not None and registered(world)._conn is not None)
        vc = registered(world)
        await wait_for(lambda: vc._conn.status().state == "connecting")
        testing.inject_driver_event(vc._conn, "connect", ssrc=SSRC)

    # songbird.connect() builds the factory itself; swap the class for the detached test client.
    monkeypatch.setattr(SongbirdClient, "with_config", staticmethod(HarnessClient.with_config))
    t = asyncio.create_task(drive())
    vc = await songbird.connect(world.channel, config=VoiceConfig.low_latency(), timeout=5)
    await t
    assert isinstance(vc, HarnessClient) and vc._config.mix_mode == "mono"
    await vc.disconnect()


async def test_cancelled_connect_keeps_the_owed_leave(world: World) -> None:
    # An aborted connect owes a (slow) leave. A second connect() that is cancelled while it waits
    # for that leave must not cancel it: the bot would otherwise stay in the channel.
    world.guild.send_server_update = False
    world.guild.leave_delay = 0.3
    with pytest.raises(ConnectTimeout):
        await world.channel.connect(cls=client_factory(), timeout=0.05, reconnect=True)
    task = asyncio.create_task(world.channel.connect(cls=client_factory(), timeout=5, reconnect=True))
    await asyncio.sleep(0.05)  # the new connect is waiting for the owed leave
    assert world.guild.op4 == [Op4(CHANNEL_ID, False, False)]
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    assert registered(world) is None
    await wait_for(lambda: Op4(None, False, False) in world.guild.op4, timeout=2)
    # The next connect still orders its join after that leave.
    world.guild.send_server_update = True
    world.guild.leave_delay = 0.0
    vc = await connect_detached(world)
    assert [op.channel_id for op in world.guild.op4] == [CHANNEL_ID, None, CHANNEL_ID]
    await vc.disconnect()


async def test_owed_leave_is_still_awaited_after_a_cancelled_connect(world: World) -> None:
    world.guild.send_server_update = False
    world.guild.leave_delay = 0.3
    with pytest.raises(ConnectTimeout):
        await world.channel.connect(cls=client_factory(), timeout=0.05, reconnect=True)
    task = asyncio.create_task(world.channel.connect(cls=client_factory(), timeout=5, reconnect=True))
    await asyncio.sleep(0.02)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    # Connect again at once: the join must still wait for the (re-registered) owed leave.
    world.guild.send_server_update = True
    vc = await connect_detached(world)
    assert [op.channel_id for op in world.guild.op4] == [CHANNEL_ID, None, CHANNEL_ID]
    await vc.disconnect()


async def test_disconnect_during_a_rejoin_never_rejoins(world: World, testing: Any) -> None:
    vc = await connect_detached(world)
    world.guild.respond = False
    world.guild.leave_delay = 0.2  # the rejoin's leave op 4 is slow
    testing.inject_driver_event(vc._conn, "disconnect", reason="io")
    vc.harness.step(2_000)  # backoff elapses → Rejoining → Control.Rejoin → _rejoin() task
    await pump_settle()
    assert any(t.get_name().startswith("songbird-rejoin") for t in vc._tasks)
    await vc.disconnect()
    await asyncio.sleep(0.4)
    await world.guild.settle()
    # Nothing that puts the bot back in a channel is sent once disconnect() began.
    assert world.guild.op4[0] == Op4(CHANNEL_ID, False, False)
    assert all(op.channel_id is None for op in world.guild.op4[1:]), world.guild.op4
    assert registered(world) is None


async def test_flag_change_during_disconnect_sends_no_join(world: World) -> None:
    vc = await connect_detached(world)
    world.guild.leave_delay = 0.1
    leaving = asyncio.create_task(vc.disconnect())
    await asyncio.sleep(0.01)  # disconnect() is sending its leave op 4
    await vc.set_self_mute(True)  # must not re-join the channel with the new flags
    await leaving
    assert world.guild.op4 == [Op4(CHANNEL_ID, False, False), Op4(None, False, False)]


async def test_reconnect_and_move_after_disconnect_raise(world: World) -> None:
    vc = await connect_detached(world)
    await vc.disconnect()
    with pytest.raises(ConnectionClosed):
        await vc.reconnect(timeout=1)
    with pytest.raises(ConnectionClosed):
        await vc.move_to(world.other, timeout=1)
    assert world.guild.op4 == [Op4(CHANNEL_ID, False, False), Op4(None, False, False)]


async def test_terminal_loss_sends_a_best_effort_leave(world: World, testing: Any) -> None:
    # Closed(lost) with reconnect=False: the last op 4 was our join, so the gateway still lists the
    # bot in the channel. A leave is sent (once) and the next connect orders its join after it.
    vc = await connect_detached(world, reconnect=False)
    world.guild.respond = False
    testing.inject_driver_event(vc._conn, "disconnect", reason="io")
    await wait_for(lambda: registered(world) is None)
    assert vc.status.close_reason == "lost"
    await wait_for(lambda: len(world.guild.op4) == 2)
    assert world.guild.op4 == [Op4(CHANNEL_ID, False, False), Op4(None, False, False)]
    await vc.disconnect()  # already finalized: nothing more is sent
    await asyncio.sleep(0.01)
    assert len(world.guild.op4) == 2


async def test_kick_sends_no_leave(world: World) -> None:
    vc = await connect_detached(world)
    await vc.on_voice_state_update({"channel_id": None, "session_id": "s"})  # type: ignore
    await wait_for(lambda: registered(world) is None)
    await asyncio.sleep(0.01)
    assert [op.channel_id for op in world.guild.op4] == [CHANNEL_ID]  # already out of the channel


async def test_pending_leave_of_another_event_loop_is_ignored(world: World) -> None:
    # A leave left behind by an earlier event loop (e.g. a previous asyncio.run) can be neither
    # awaited nor cancelled from this loop: connect() must not wait 5 s for it or touch it.
    import threading

    from discord.ext.songbird import client as client_mod

    other = asyncio.new_event_loop()
    ready = threading.Event()
    box: dict[str, Any] = {}

    def run_other() -> None:
        asyncio.set_event_loop(other)
        box["task"] = other.create_task(asyncio.sleep(30))
        ready.set()
        other.run_forever()
        box["task"].cancel()
        other.run_until_complete(asyncio.gather(box["task"], return_exceptions=True))
        other.close()

    thread = threading.Thread(target=run_other)
    thread.start()
    assert ready.wait(2)
    client_mod._pending_leaves[GUILD_ID] = box["task"]
    try:
        async with asyncio.timeout(2):  # well under the 5 s owed-leave wait
            vc = await connect_detached(world)
        assert not box["task"].done()  # untouched
        assert GUILD_ID not in client_mod._pending_leaves
        await vc.disconnect()
    finally:
        client_mod._pending_leaves.pop(GUILD_ID, None)
        other.call_soon_threadsafe(other.stop)
        thread.join(5)
