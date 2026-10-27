"""discord.py VoiceProtocol behaviour (§6, §13.3 ``test_connect.py``).

Every test drives discord.py's own ``Connectable.connect`` against a detached native connection; the
fake guild records each op 4 and plays the gateway.
"""

from __future__ import annotations

import asyncio
import weakref
from typing import Any

import pytest
from conftest import (
    CHANNEL_ID,
    GUILD_ID,
    OTHER_CHANNEL_ID,
    SSRC,
    USER_ID,
    FakeClient,
    FakeGuild,
    FakeUser,
    HarnessClient,
    Op4,
    World,
    client_factory,
    connect_detached,
    drain,
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
from discord.ext.songbird import client as client_mod

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
    key = (USER_ID, GUILD_ID)
    client_mod._pending_leaves[key] = weakref.ref(box["task"])
    try:
        async with asyncio.timeout(2):  # well under the 5 s owed-leave wait
            vc = await connect_detached(world)
        assert not box["task"].done()  # untouched
        assert key not in client_mod._pending_leaves
        await vc.disconnect()
    finally:
        client_mod._pending_leaves.pop(key, None)
        other.call_soon_threadsafe(other.stop)
        thread.join(5)


# ── review findings (discord.py integration) ──────────────────────────────────────────────────────


async def test_disconnect_while_connect_waits_for_an_owed_leave(world: World) -> None:
    # A retried connect() waits for the owed leave of an aborted one. A disconnect() in that window
    # used to return while connect() went on to send its join op 4 afterwards (the bot sat in the
    # channel the user had just left until the timeout, unregistered).
    world.guild.send_server_update = False
    world.guild.leave_delay = 0.5
    with pytest.raises(ConnectTimeout):
        await world.channel.connect(cls=client_factory(), timeout=0.05, reconnect=True)
    task = asyncio.create_task(world.channel.connect(cls=client_factory(), timeout=5, reconnect=True))
    await wait_for(lambda: registered(world) is not None)
    vc: HarnessClient = registered(world)
    await asyncio.sleep(0.02)
    assert vc._conn is None  # still waiting for the owed leave
    await vc.disconnect()
    with pytest.raises(ConnectFailed) as ei:
        async with asyncio.timeout(0.2):  # at once, not after the owed leave
            await task
    assert ei.value.reason == "requested"
    assert registered(world) is None
    # The leave is still owed: an immediate connect orders its join after it.
    world.guild.send_server_update = True
    vc2 = await connect_detached(world)
    assert [op.channel_id for op in world.guild.op4] == [CHANNEL_ID, None, CHANNEL_ID]
    await vc2.disconnect()


async def test_owed_leave_of_another_bot_is_not_awaited(world: World) -> None:
    # Owed leaves are keyed by voice state (bot user, guild): a second bot of the same process
    # connecting in the guild neither waits for nor cancels the first bot's leave.
    world.guild.send_server_update = False
    world.guild.leave_delay = 1.0
    with pytest.raises(ConnectTimeout):
        await world.channel.connect(cls=client_factory(), timeout=0.05, reconnect=True)
    client_b = FakeClient()
    client_b.user = FakeUser(USER_ID + 1, bot=True, name="other-bot")
    guild_b = FakeGuild(client_b)
    world_b = World(client_b, guild_b, guild_b.add_channel(CHANNEL_ID), guild_b.add_channel(OTHER_CHANNEL_ID))
    async with asyncio.timeout(0.5):
        vc_b = await connect_detached(world_b)
    ref = client_mod._pending_leaves.get((USER_ID, GUILD_ID))
    leave_a = ref() if ref is not None else None
    assert leave_a is not None and not leave_a.done()
    await wait_for(lambda: Op4(None, False, False) in world.guild.op4, timeout=3)
    assert not leave_a.cancelled()
    await vc_b.disconnect()


async def test_stale_client_after_a_reidentify_never_kicks_the_new_one(world: World, testing: Any) -> None:
    # discord.py's non-sharded READY (a re-IDENTIFY) replaces state._voice_clients without calling
    # cleanup(). The old client (still referenced by the app) used to rejoin when its voice WS
    # dropped, and its op 4 None kicked the client the app had connected in on_ready.
    vc1 = await connect_detached(world)
    world.client._connection._voice_clients = {}  # ConnectionState.clear()
    vc2 = await connect_detached(world)
    assert registered(world) is vc2
    sent = list(world.guild.op4)
    testing.inject_driver_event(vc1._conn, "disconnect", reason="ws_closed", code=4014)
    vc1.harness.step(5_100)  # grace → AwaitingRejoin → Control.Rejoin
    conn1 = vc1._conn
    assert conn1 is not None
    await wait_for(lambda: conn1.closed)
    await pump_settle()
    assert world.guild.op4 == sent  # no leave + join from the stale client
    assert vc1.status.close_reason == "cleanup"
    # Its disconnect() and flag changes send nothing either (a leave would disconnect vc2).
    await vc1.disconnect()
    await vc1.set_self_mute(True)
    await asyncio.sleep(0.01)
    assert world.guild.op4 == sent
    assert vc2.is_connected() and registered(world) is vc2
    await vc2.disconnect()


async def test_unregistered_client_closes_instead_of_sending_op4(world: World, testing: Any) -> None:
    vc = await connect_detached(world)
    world.client._connection._voice_clients = {}
    with pytest.raises(ConnectionClosed):
        await vc.move_to(world.other, timeout=1)
    assert vc.status.close_reason == "cleanup"
    vc2 = await connect_detached(world)
    world.client._connection._voice_clients = {}
    world.guild.respond = False
    testing.inject_driver_event(vc2._conn, "disconnect", reason="io")
    vc2.harness.step(2_000)  # backoff → Control.Rejoin
    conn2 = vc2._conn
    assert conn2 is not None
    await wait_for(lambda: conn2.closed)
    await pump_settle()
    assert [op.channel_id for op in world.guild.op4] == [CHANNEL_ID, CHANNEL_ID]  # the two joins only


async def test_move_the_gateway_never_confirms_returns_to_the_old_channel(world: World) -> None:
    # Discord ignores an op 4 for a channel the bot may not join (no CONNECT permission, full).
    # The 3 s server fallback used to "connect" to that channel id with the old session and token,
    # and move_to() returned normally while the bot was still in the old channel.
    vc = await connect_detached(world)
    world.guild.respond = False
    task = asyncio.create_task(vc.move_to(world.other, timeout=30))
    await wait_for(lambda: world.guild.op4[-1] == Op4(OTHER_CHANNEL_ID, False, False))
    vc.harness.step(3_100)
    await pump_settle()
    assert vc.status.state == "joining" and not task.done()
    vc.harness.step(2_000)  # unconfirmed for 5 s: back to the old channel
    with pytest.raises(ConnectFailed) as ei:
        await task
    assert ei.value.reason == "move_failed"
    st = vc.status
    assert (st.state, st.channel_id, st.target_channel_id) == ("connected", CHANNEL_ID, CHANNEL_ID)
    assert st.generation == 1  # no Connect for the unconfirmed channel
    assert vc.channel is world.channel
    await vc.disconnect()


async def test_failed_move_op4_never_reports_the_new_channel(world: World) -> None:
    vc = await connect_detached(world)
    world.guild.fail_next = RuntimeError("gateway reconnecting")
    with pytest.raises(RuntimeError):
        await vc.move_to(world.other)
    vc.harness.step(5_100)
    await pump_settle()
    st = vc.status
    assert (st.state, st.channel_id, st.target_channel_id) == ("connected", CHANNEL_ID, CHANNEL_ID)
    assert vc.channel is world.channel
    await vc.disconnect()


async def test_voice_server_change_4022_reconnects_without_rejoin(world: World, testing: Any) -> None:
    # 4022 (call terminated, e.g. the voice server changed) used to be a generic loss: closed as
    # "lost" with reconnect=False, or an audible op 4 leave + join with reconnect=True.
    for reconnect in (False, True):
        vc = await connect_detached(world, reconnect=reconnect)
        before = len(world.guild.op4)
        testing.inject_driver_event(vc._conn, "disconnect", reason="ws_closed", code=4022)
        assert vc.status.state == "reconnecting"
        await vc.on_voice_server_update(
            {"token": "t2", "endpoint": "wss://other.discord.media", "guild_id": str(GUILD_ID)}
        )
        assert vc.status.state == "connecting"
        testing.inject_driver_event(vc._conn, "connect", ssrc=SSRC)
        await wait_for(vc.is_connected)
        assert len(world.guild.op4) == before  # no leave + join
        assert vc.stats().get("gw_unlisted_inputs", 0) == 0
        await vc.disconnect()


async def test_rate_limited_4021_closes_without_rejoin(world: World, testing: Any) -> None:
    vc = await connect_detached(world)
    world.guild.respond = False
    testing.inject_driver_event(vc._conn, "disconnect", reason="ws_closed", code=4021)
    await wait_for(lambda: registered(world) is None)
    st = vc.status
    assert (st.close_reason, st.close_code) == ("lost", 4021)
    await wait_for(lambda: len(world.guild.op4) == 2)
    await asyncio.sleep(0.01)
    assert [op.channel_id for op in world.guild.op4] == [CHANNEL_ID, None]  # one best-effort leave


async def test_moderator_move_while_awaiting_rejoin_is_followed(world: World, testing: Any) -> None:
    # The update used to be dropped in AwaitingRejoin, and the rejoin dragged the bot back.
    vc = await connect_detached(world)
    world.guild.respond = False
    testing.inject_driver_event(vc._conn, "disconnect", reason="io")
    assert vc.status.state == "reconnecting"
    await vc.on_voice_state_update({"channel_id": str(OTHER_CHANNEL_ID), "session_id": "moved"})  # type: ignore
    assert vc.status.target_channel_id == OTHER_CHANNEL_ID
    assert vc.channel is world.other
    vc.harness.step(2_000)  # backoff → Control.Rejoin
    await wait_for(lambda: len(world.guild.op4) >= 3)
    assert world.guild.op4[1:] == [Op4(None, False, False), Op4(OTHER_CHANNEL_ID, False, False)]
    await vc.disconnect()


async def test_slow_leave_is_finished_in_the_background(world: World, monkeypatch: pytest.MonkeyPatch) -> None:
    # A leave held by discord.py's gateway rate limiter (up to 60 s) used to be cancelled after
    # 5 s and never sent: the bot stayed listed in the channel.
    monkeypatch.setattr(client_mod, "_LEAVE_TIMEOUT_S", 0.05)
    vc = await connect_detached(world)
    world.guild.leave_delay = 0.3
    async with asyncio.timeout(0.25):
        await vc.disconnect()  # tears down locally after 0.05 s
    assert registered(world) is None and vc._conn is not None and vc._conn.closed
    assert Op4(None, False, False) not in world.guild.op4
    await wait_for(lambda: Op4(None, False, False) in world.guild.op4, timeout=2)


async def test_forced_disconnect_does_not_wait_for_the_leave(world: World) -> None:
    # discord.py's Client.close() disconnects every client in turn with force=True: no client may
    # hold the shutdown for its leave. The leave is still sent, and ordered before the next join.
    vc = await connect_detached(world)
    world.guild.leave_delay = 1.0
    async with asyncio.timeout(0.5):
        await vc.disconnect(force=True)
    assert registered(world) is None
    vc2 = await connect_detached(world)
    assert [op.channel_id for op in world.guild.op4] == [CHANNEL_ID, None, CHANNEL_ID]
    await vc2.disconnect()


async def test_connect_timeout_covers_the_join_op4(world: World) -> None:
    # The timeout used to start only after the join op 4 was sent, so a rate-limited op 4 made
    # connect(timeout=0.2) take as long as the limiter (up to 60 s).
    send = world.guild.change_voice_state

    async def rate_limited(**kw: Any) -> None:
        if kw.get("channel") is not None:
            await asyncio.sleep(2.0)
        await send(**kw)

    world.guild.change_voice_state = rate_limited
    with pytest.raises(ConnectTimeout):
        async with asyncio.timeout(1.0):
            await world.channel.connect(cls=client_factory(), timeout=0.2, reconnect=True)
    assert registered(world) is None
    await wait_for(lambda: Op4(None, False, False) in world.guild.op4)  # the best-effort leave


async def test_move_and_reconnect_timeouts_cover_the_op4(world: World) -> None:
    vc = await connect_detached(world)
    send = world.guild.change_voice_state

    async def rate_limited(**kw: Any) -> None:
        await asyncio.sleep(2.0)
        await send(**kw)

    world.guild.change_voice_state = rate_limited
    loop = asyncio.get_running_loop()
    t0 = loop.time()
    with pytest.raises(ConnectTimeout):
        await vc.move_to(world.other, timeout=0.2)
    with pytest.raises(ConnectTimeout):
        await vc.reconnect(timeout=0.2)
    assert loop.time() - t0 < 1.0
    del world.guild.change_voice_state
    await vc.disconnect()


async def test_server_reallocation_that_never_completes_rejoins(world: World, testing: Any) -> None:
    # VOICE_SERVER_UPDATE(endpoint=None) without a follow-up used to leave the connection in
    # "joining" forever, with no event.
    vc = await connect_detached(world)
    world.guild.respond = False
    async with vc.events() as events:
        await vc.on_voice_server_update({"token": "t", "endpoint": None, "guild_id": str(GUILD_ID)})
        assert vc.status.state == "joining"
        seen = drain(events)
        assert any(
            isinstance(e, VoiceEvent.Disconnected) and e.reason == "server_reallocating" and e.will_retry for e in seen
        ), seen
        vc.harness.step(15_100)  # no new server: rejoin
        await wait_for(lambda: len(world.guild.op4) >= 3)
        assert world.guild.op4[1:] == [Op4(None, False, False), Op4(CHANNEL_ID, False, False)]
    await vc.disconnect()
