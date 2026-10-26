"""Shared fixtures (§13.3).

Native module selection (``SONGBIRD_TEST_NATIVE``):

* ``auto`` (default): the real extension if one is built next to the package being imported,
  otherwise the pure-Python fake (``tests/fakes/native_fake.py``);
* ``real``: the real extension (built with ``--features test-hooks``), or fail;
* ``fake``: always the fake.

Tests marked ``real_only`` need the Rust implementation (zero-copy buffers, real Opus, allocation
counting, leak counters, subprocess bounds); tests marked ``fake_only`` rely on the fake's
simplifications. Tests that need ``native._testing`` are skipped on a build without test hooks.

discord.py objects are faked (``FakeClient``, ``FakeGuild``, ``FakeChannel``) closely enough that
discord.py's own ``Connectable.connect`` (``abc.py:2137-2161``) drives :class:`SongbirdClient`.
"""

from __future__ import annotations

import asyncio
import importlib.util
import os
import pathlib
import sys
from collections.abc import AsyncIterator, Callable
from dataclasses import dataclass, field
from typing import Any

import pytest

ROOT = pathlib.Path(__file__).resolve().parent.parent
MODE = os.environ.get("SONGBIRD_TEST_NATIVE", "auto").lower()


def _songbird_dir() -> pathlib.Path | None:
    import discord.ext

    for p in discord.ext.__path__:
        d = pathlib.Path(p) / "songbird"
        if (d / "__init__.py").exists():
            return d
    return None


def _has_real_extension(d: pathlib.Path | None) -> bool:
    if d is None:
        return False
    return any(d.glob("native.*.so")) or any(d.glob("native.*.pyd")) or any(d.glob("native.so"))


def _install_fake() -> None:
    spec = importlib.util.spec_from_file_location("songbird_native_fake", ROOT / "tests" / "fakes" / "native_fake.py")
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    sys.modules["songbird_native_fake"] = mod
    spec.loader.exec_module(mod)
    mod.install()


if MODE == "fake" or (MODE == "auto" and not _has_real_extension(_songbird_dir())):
    _install_fake()
elif MODE not in ("real", "auto"):
    raise RuntimeError(f"SONGBIRD_TEST_NATIVE must be auto, real or fake (got {MODE!r})")

import discord  # noqa: E402
from discord.ext import songbird  # noqa: E402
from discord.ext.songbird import native  # noqa: E402

FAKE: bool = bool(getattr(native, "FAKE", False))
TESTING: Any = getattr(native, "_testing", None)

if MODE == "real" and (FAKE or TESTING is None):
    raise RuntimeError("SONGBIRD_TEST_NATIVE=real needs the extension built with --features test-hooks")


def pytest_configure(config: pytest.Config) -> None:
    config.addinivalue_line("markers", "real_only: needs the Rust implementation (skipped on the fake)")
    config.addinivalue_line("markers", "fake_only: relies on the fake's simplifications")


def pytest_collection_modifyitems(config: pytest.Config, items: list[pytest.Item]) -> None:
    skip_real = pytest.mark.skip(reason="needs the real native module (SONGBIRD_TEST_NATIVE=real)")
    skip_fake = pytest.mark.skip(reason="relies on the pure-Python fake")
    skip_hooks = pytest.mark.skip(reason="native._testing is missing (build with --features test-hooks)")
    for item in items:
        if FAKE and item.get_closest_marker("real_only"):
            item.add_marker(skip_real)
        if not FAKE and item.get_closest_marker("fake_only"):
            item.add_marker(skip_fake)
        if TESTING is None and "testing" in getattr(item, "fixturenames", ()):
            item.add_marker(skip_hooks)


def pytest_report_header(config: pytest.Config) -> str:
    return f"songbird native: {'fake' if FAKE else 'real'} (test hooks: {TESTING is not None})"


# ── fake discord.py objects ────────────────────────────────────────────────────────────────────────

GUILD_ID = 1_000_000_000_000_001
USER_ID = 2_000_000_000_000_002
APPLICATION_ID = 3_000_000_000_000_003
CHANNEL_ID = 4_000_000_000_000_004
OTHER_CHANNEL_ID = 4_000_000_000_000_005
SSRC = 4242


@dataclass(eq=False)
class FakeUser:
    id: int
    bot: bool = False
    name: str = "user"


@dataclass(eq=False)
class FakeState:
    """The parts of ``discord.state.ConnectionState`` that ``Connectable.connect`` uses."""

    client: Any = None
    _voice_clients: dict[int, Any] = field(default_factory=dict)

    def _get_voice_client(self, guild_id: int | None) -> Any:
        return self._voice_clients.get(guild_id) if guild_id is not None else None

    def _add_voice_client(self, guild_id: int, voice: Any) -> None:
        self._voice_clients[guild_id] = voice

    def _remove_voice_client(self, guild_id: int) -> None:
        self._voice_clients.pop(guild_id, None)

    def _get_client(self) -> Any:
        return self.client


class FakeClient:
    """``discord.Client`` stand-in: ``user`` (whose id differs from ``application_id``) and state."""

    def __init__(self) -> None:
        self.user = FakeUser(USER_ID, bot=True, name="songbird-bot")
        self.application_id = APPLICATION_ID
        self._connection = FakeState()
        self._connection.client = self


class FakeChannel:
    """A guild voice channel: ``_get_voice_client_key``, ``members``, and discord.py's own
    ``Connectable.connect``."""

    def __init__(self, guild: FakeGuild, channel_id: int, name: str = "voice") -> None:
        self.guild = guild
        self.id = channel_id
        self.name = name
        self.members: list[FakeUser] = []
        self._state = guild.state

    def _get_voice_client_key(self) -> tuple[int, str]:
        return self.guild.id, "guild_id"

    async def connect(self, **kwargs: Any) -> Any:
        # The real implementation (abc.py:2137-2161) with our fake state.
        return await discord.abc.Connectable.connect(self, **kwargs)  # type: ignore


@dataclass
class Op4:
    channel_id: int | None
    self_mute: bool
    self_deaf: bool


class FakeGuild:
    """Records every op 4 and, optionally, plays the gateway: answers a join with
    ``VOICE_STATE_UPDATE`` + ``VOICE_SERVER_UPDATE`` and a leave with ``VOICE_STATE_UPDATE(None)``,
    each as a separate task (``state.py:1663-1705``)."""

    def __init__(self, client: FakeClient) -> None:
        self.id = GUILD_ID
        self.client = client
        self.state = client._connection
        self.op4: list[Op4] = []
        self.channels: dict[int, FakeChannel] = {}
        self.members: dict[int, FakeUser] = {}
        self.respond = True
        self.send_server_update = True
        self.leave_delay = 0.0
        self.fail_next: BaseException | None = None
        self.session_seq = 0
        self.endpoint: str | None = "wss://fake-voice.discord.media/"
        self._tasks: set[asyncio.Task[Any]] = set()

    def add_channel(self, channel_id: int) -> FakeChannel:
        ch = FakeChannel(self, channel_id)
        self.channels[channel_id] = ch
        return ch

    def get_channel(self, channel_id: int) -> FakeChannel | None:
        return self.channels.get(channel_id)

    def get_member(self, user_id: int) -> FakeUser | None:
        return self.members.get(user_id)

    async def change_voice_state(self, *, channel: Any, self_mute: bool = False, self_deaf: bool = False) -> None:
        ch_id = channel.id if channel is not None else None
        if ch_id is None and self.leave_delay:
            await asyncio.sleep(self.leave_delay)
        if self.fail_next is not None:
            exc, self.fail_next = self.fail_next, None
            raise exc
        self.op4.append(Op4(ch_id, self_mute, self_deaf))
        if self.respond:
            self._spawn(self._respond(ch_id, self_mute, self_deaf))

    def _spawn(self, coro: Any) -> None:
        t = asyncio.get_running_loop().create_task(coro)
        self._tasks.add(t)
        t.add_done_callback(self._tasks.discard)

    async def _respond(self, ch_id: int | None, mute: bool, deaf: bool) -> None:
        await asyncio.sleep(0)
        vc = self.state._get_voice_client(self.id)
        if vc is None:
            return
        self.session_seq += 1
        await vc.on_voice_state_update(
            {
                "guild_id": str(self.id),
                "channel_id": None if ch_id is None else str(ch_id),
                "user_id": str(USER_ID),
                "session_id": f"session-{self.session_seq}",
                "self_mute": mute,
                "self_deaf": deaf,
                "deaf": False,
                "mute": False,
                "suppress": False,
                "self_video": False,
                "request_to_speak_timestamp": None,
            }
        )
        if ch_id is not None and self.send_server_update:
            await vc.on_voice_server_update(
                {"token": f"token-{self.session_seq}", "guild_id": str(self.id), "endpoint": self.endpoint}
            )

    async def settle(self) -> None:
        """Let pending gateway responses run."""
        for _ in range(5):
            await asyncio.sleep(0)
            if self._tasks:
                await asyncio.gather(*list(self._tasks), return_exceptions=True)


@dataclass
class World:
    """The fake discord.py objects of one test (typed ``Any``: they stand in for discord.py types)."""

    client: Any
    guild: Any
    channel: Any
    other: Any


@pytest.fixture
def world() -> World:
    client = FakeClient()
    guild = FakeGuild(client)
    ch = guild.add_channel(CHANNEL_ID)
    other = guild.add_channel(OTHER_CHANNEL_ID)
    return World(client, guild, ch, other)


@pytest.fixture
def testing() -> Any:
    """``native._testing`` (the test is skipped when the build has no test hooks)."""
    assert TESTING is not None
    return TESTING


class HarnessClient(songbird.SongbirdClient):
    """A ``SongbirdClient`` whose native connection is detached (§13.2): no songbird Driver, a
    manual clock, and the real doorbell + pump."""

    harness: Any = None

    def _open_connection(self, bell: Any, settings: Any, reconnect: bool) -> Any:
        assert TESTING is not None
        conn, harness = TESTING.detached_connection(self.guild.id, USER_ID, settings, bell, reconnect=reconnect)
        self.harness = harness
        return conn


def client_factory(config: songbird.VoiceConfig | None = None, **overrides: Any) -> Callable[..., HarnessClient]:
    return HarnessClient.with_config(config, **overrides)


async def wait_for(predicate: Callable[[], bool], timeout: float = 2.0) -> None:
    """Poll ``predicate`` on the loop until it holds (real-time; for pump/gateway tasks)."""
    async with asyncio.timeout(timeout):
        while not predicate():
            await asyncio.sleep(0.001)


async def connect_detached(
    world: World,
    *,
    config: songbird.VoiceConfig | None = None,
    self_mute: bool = False,
    self_deaf: bool = False,
    reconnect: bool = True,
    timeout: float = 5.0,
) -> HarnessClient:
    """``channel.connect(cls=HarnessClient)`` with the fake gateway answering and a successful
    ``DriverConnect`` injected once the state machine reaches ``connecting``."""
    assert TESTING is not None

    async def driver() -> None:
        vc = await _wait_vc(world)
        await wait_for(lambda: vc._conn is not None and vc._conn.status().state == "connecting", timeout)
        TESTING.inject_driver_event(vc._conn, "connect", ssrc=SSRC)

    task = asyncio.get_running_loop().create_task(driver())
    try:
        vc = await world.channel.connect(
            cls=client_factory(config),
            timeout=timeout,
            reconnect=reconnect,
            self_mute=self_mute,
            self_deaf=self_deaf,
        )
    finally:
        if not task.done():
            task.cancel()
    assert isinstance(vc, HarnessClient)
    return vc


async def _wait_vc(world: World) -> HarnessClient:
    async with asyncio.timeout(5):
        while (vc := world.client._connection._get_voice_client(world.guild.id)) is None:
            await asyncio.sleep(0.001)
    return vc


@pytest.fixture
async def connected_vc(world: World, testing: Any) -> AsyncIterator[HarnessClient]:
    """A connected client on a detached connection (§13.3 ``conftest.py``)."""
    vc = await connect_detached(world)
    try:
        yield vc
    finally:
        await vc.disconnect(force=True)


def step(vc: HarnessClient, ms: int = 20) -> None:
    """Advance the detached connection's manual clock by ``ms`` (one worker iteration per tick)."""
    vc.harness.step(ms)


async def pump_settle(vc: HarnessClient | None = None) -> None:
    """Give the pump task a few loop iterations to dispatch doorbell bytes."""
    for _ in range(10):
        await asyncio.sleep(0)
    await asyncio.sleep(0.005)


# ── audio helpers ──────────────────────────────────────────────────────────────────────────────────


def tone(ms: int, *, rate: int = 48_000, channels: int = 2, freq: float = 440.0, amp: float = 0.25) -> bytes:
    """``ms`` of a sine tone as interleaved PCM16LE."""
    import array
    import math

    n = rate * ms // 1000
    a = array.array("h")
    for i in range(n):
        s = round(math.sin(2 * math.pi * freq * i / rate) * amp * 32767)
        a.extend([s] * channels)
    if sys.byteorder == "big":
        a.byteswap()
    return a.tobytes()


def voice_packets(testing: Any, ms: int = 20, **kw: Any) -> list[bytes]:
    """20 ms Opus packets of a tone (``_testing.encode_opus``)."""
    return list(testing.encode_opus(tone(ms, **kw)))


def inject_ticks(vc: HarnessClient, testing: Any, ticks: int, *, ssrc: int = SSRC, voiced: bool = True) -> None:
    """``ticks`` real ticks (one per 20 ms of manual time) from ``ssrc``: voiced or silent."""
    packet = voice_packets(testing)[0] if voiced else None
    for _ in range(ticks):
        if voiced:
            testing.inject_tick(vc._conn, [(ssrc, packet)], [])
        else:
            testing.inject_tick(vc._conn, [], [ssrc])
        vc.harness.step(20)


def drain(stream: Any) -> list[Any]:
    """Every chunk/event currently queued (``read_nowait`` until ``None``)."""
    out = []
    while (item := stream.read_nowait()) is not None:
        out.append(item)
    return out


# ── subprocess isolation ───────────────────────────────────────────────────────────────────────────


def run_isolated(code: str, *, timeout: float = 120.0, env: dict[str, str | None] | None = None) -> Any:
    """Run ``code`` in a fresh interpreter that selects the native module exactly as this session
    did (it imports ``conftest`` first). ``env`` entries override this process's environment; a
    ``None`` value removes the variable. Returns the ``subprocess.CompletedProcess``."""
    import subprocess
    import textwrap

    tests_dir = str(ROOT / "tests")
    prelude = f"import sys\nsys.path.insert(0, {tests_dir!r})\nimport conftest\n"
    full_env = dict(os.environ)
    full_env["SONGBIRD_TEST_NATIVE"] = "fake" if FAKE else MODE
    paths = [str(ROOT / "py-src"), str(ROOT)]
    if full_env.get("PYTHONPATH"):
        paths.append(full_env["PYTHONPATH"])
    full_env["PYTHONPATH"] = os.pathsep.join(paths)
    for key, value in (env or {}).items():
        if value is None:
            full_env.pop(key, None)
        else:
            full_env[key] = value
    return subprocess.run(
        [sys.executable, "-X", "faulthandler", "-c", prelude + textwrap.dedent(code)],
        capture_output=True,
        text=True,
        timeout=timeout,
        env=full_env,
        cwd=str(ROOT),
    )
