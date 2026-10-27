"""``SongbirdClient``: a pure-Python ``discord.VoiceProtocol`` composing ``native.Connection`` (§6).

discord.py 2.7.1 contract (``abc.py:2137-2161``, ``state.py:1663-1705``, ``voice_client.py:98-188``,
``guild.py:4539``):

* ``Connectable.connect`` constructs ``cls(client, channel)``, registers it with
  ``state._add_voice_client`` **before** awaiting ``voice.connect(...)``, and calls
  ``voice.disconnect(force=True)`` only on ``asyncio.TimeoutError``;
* ``on_voice_state_update`` (own user only) and ``on_voice_server_update`` run as separate tasks;
* ``VoiceProtocol.cleanup()`` removes the client from ``state._voice_clients``.

Rust never calls Python: op 4 (voice state) is always sent from here, either directly or when the
native state machine asks for it through a control slot (§6.4).
"""

from __future__ import annotations

import asyncio
import contextlib
import functools
import logging
import socket
import weakref
from collections.abc import Callable, Coroutine, Iterable
from typing import TYPE_CHECKING, Any, Literal, Self

import discord
from discord import utils

from . import native
from ._pump import _Pump, cancel_tasks
from .config import VoiceConfig
from .errors import ConnectFailed, ConnectionClosed, ConnectTimeout, NotConnected
from .formats import PCM16_48K_STEREO, FormatLike, as_format
from .playback import EncodedStream, OpusPacketStream, PlaybackStream
from .streams import AudioStream, EventStream, MixPolicy, Overflow, UserStreams, kind_mask, user_id, user_ids
from .tracks import Source, TrackHandle, TrackQueue

if TYPE_CHECKING:
    from discord.channel import VocalGuildChannel
    from discord.types.voice import GuildVoiceState as GuildVoiceStatePayload
    from discord.types.voice import VoiceServerUpdate as VoiceServerUpdatePayload

__all__ = ["SongbirdClient", "connect"]

_log = logging.getLogger("discord.ext.songbird")

#: ``(bot user id, guild id)`` → the leave (op 4 ``None``) still owed for that voice state: an aborted
#: connect's, a terminal close's, or a ``disconnect()`` whose op 4 is still in flight (§6.2). The next
#: ``connect()`` for the same voice state awaits it. Held **weakly**: the task is kept alive by a timer
#: handle on its own event loop, so a loop closed without running it releases the task and the
#: ``discord.Client`` it references instead of this module pinning them for the process lifetime.
_pending_leaves: dict[tuple[int, int], weakref.ref[asyncio.Task[None]]] = {}

#: How long ``connect()``/``disconnect()`` wait for a leave before going on without it.
_LEAVE_TIMEOUT_S = 5.0
#: Bound of a background leave: discord.py's gateway rate limiter (110 sends per 60 s) can hold an
#: op 4 for up to a minute, and a leave given up on is a bot left listed in the channel.
_LEAVE_SEND_TIMEOUT_S = 120.0
#: Terminal close reasons after which the gateway may still list the bot in a channel.
_LEAVE_AFTER = frozenset({"connect_failed", "rejoin_failed", "lost", "session_invalid", "internal_error"})
_PUMP_CLOSE_TIMEOUT_S = 2.0


class SongbirdClient(discord.VoiceProtocol):
    """discord.py ``VoiceProtocol`` backed by songbird.

    Use ``await channel.connect(cls=SongbirdClient)``; pass settings with
    ``cls=SongbirdClient.with_config(VoiceConfig(...))`` or :func:`discord.ext.songbird.connect`.

    Parameters
    ----------
    client : discord.Client
    channel : discord.abc.Connectable
        A guild voice or stage channel (``TypeError`` otherwise).
    config : VoiceConfig or None
        Per-connection settings (defaults when ``None``).

    Notes
    -----
    Receive, send and track methods work in every state except ``"closed"``; before ``connect()``
    they raise :class:`NotConnected`. Every stream, playback and event stream is an async context
    manager, and every ``close()`` is synchronous, idempotent and safe from any thread.
    """

    channel: VocalGuildChannel  # ty: ignore[invalid-mutable-override]  # narrowed like discord.VoiceClient

    def __init__(
        self,
        client: discord.Client,
        channel: discord.abc.Connectable,
        *,
        config: VoiceConfig | None = None,
    ) -> None:
        super().__init__(client, channel)
        _key, kind = channel._get_voice_client_key()
        if kind != "guild_id":
            raise TypeError("SongbirdClient supports guild voice and stage channels only")
        self._config = config if config is not None else VoiceConfig()
        self._loop: asyncio.AbstractEventLoop | None = None
        self._conn: native.Connection | None = None
        self._pump: _Pump | None = None
        self._rsock: socket.socket | None = None
        self._finalized = False
        self._join_sent = False
        self._leave_sent = False
        self._in_channel = False  # a VOICE_STATE_UPDATE with a channel has arrived for this connection
        self._explicit_mute: bool | None = None
        self._explicit_deaf: bool | None = None
        self._self_mute = False
        self._self_deaf = False
        self._sent_flags: tuple[bool, bool] | None = None
        self._gateway_self_mute = False
        self._gateway_self_deaf = False
        self._children: weakref.WeakSet[Any] = weakref.WeakSet()
        self._queue: TrackQueue | None = None
        self._tasks: set[asyncio.Task[Any]] = set()
        # Resolved by cleanup(): wakes a connect() that waits for an owed leave, so a disconnect()
        # in that window stops the join instead of returning while the join is still to come.
        self._join_abort: asyncio.Future[None] | None = None

    @classmethod
    def with_config(
        cls, config: VoiceConfig | None = None, /, **overrides: Any
    ) -> Callable[[discord.Client, discord.abc.Connectable], Self]:
        """Factory for ``channel.connect(cls=...)`` carrying a :class:`VoiceConfig`.

        Parameters
        ----------
        config : VoiceConfig or None
            Base settings (defaults when ``None``).
        **overrides
            Field overrides applied with :meth:`VoiceConfig.replace`.
        """
        cfg = config if config is not None else VoiceConfig()
        if overrides:
            cfg = cfg.replace(**overrides)
        return functools.partial(cls, config=cfg)

    def __repr__(self) -> str:
        state = self._conn.status().state if self._conn is not None else "new"
        return f"<SongbirdClient guild={self.guild.id} channel={self.channel.id} state={state}>"

    # ── VoiceProtocol hooks (called by discord.py) ───────────────────────────────────────────────

    async def connect(
        self,
        *,
        timeout: float,
        reconnect: bool,
        self_deaf: bool = False,
        self_mute: bool = False,
    ) -> None:
        """Send one op 4 carrying the flags and wait for the media connection.

        Raises
        ------
        ConnectTimeout
            Not connected within ``timeout`` seconds (a ``TimeoutError``). The timeout covers the
            whole operation: waiting for an owed leave, sending the join op 4 (which discord.py's
            gateway rate limiter can delay) and the handshake.
        ConnectFailed
            The connection ended terminally while connecting, or ``disconnect()`` was called
            meanwhile (``reason == "requested"``).

        Notes
        -----
        On **any** failure, including cancellation, the connection is torn down, a best-effort
        leave is sent and :meth:`cleanup` runs before the exception propagates.
        """
        loop = asyncio.get_running_loop()
        self._loop = loop
        # An explicit set_self_mute()/set_self_deaf() made before connect() wins (§6.7).
        self._self_mute = self_mute if self._explicit_mute is None else self._explicit_mute
        self._self_deaf = self_deaf if self._explicit_deaf is None else self._explicit_deaf
        key = _leave_key(self.client, self.guild.id)
        deadline = asyncio.timeout(timeout)
        try:
            try:
                async with deadline:
                    if (prev := _take_owed_leave(key)) is not None:
                        # An owed leave of this voice state must not land after our join op 4.
                        self._join_abort = loop.create_future()
                        try:
                            await _await_pending_leave(key, prev, self._join_abort)
                        finally:
                            self._join_abort = None
                    if self._finalized:
                        # disconnect() (or cleanup()) ran while we waited: never join after it.
                        raise ConnectFailed("requested")
                    rsock, wsock = socket.socketpair()
                    self._rsock = rsock
                    rsock.setblocking(False)
                    bell = native.WakeBell(wsock.detach())  # Rust owns the write end from here
                    self._conn = self._open_connection(bell, self._config.to_native(), reconnect)
                    self._pump = _Pump(loop, rsock, self._conn, self)
                    self._conn.request_join(self.channel.id, self._self_mute, self._self_deaf)
                    self._conn.set_users_bot([(m.id, m.bot) for m in self.channel.members])
                    # The only op 4 of a join: channel and flags together, nothing before it.
                    self._sent_flags = (self._self_mute, self._self_deaf)
                    self._join_sent = True
                    await self.guild.change_voice_state(
                        channel=self.channel, self_mute=self._self_mute, self_deaf=self._self_deaf
                    )
                    st = await self._pump.wait_status(lambda s: s.state in ("connected", "closed"))
            except TimeoutError:
                if deadline.expired():
                    raise ConnectTimeout(timeout) from None
                raise
            if st.state == "closed":
                raise ConnectFailed(st.close_reason or "closed", st.close_code)
        except BaseException:
            self._abort_connect()
            raise

    def _open_connection(
        self, bell: native.WakeBell, settings: native.VoiceSettings, reconnect: bool
    ) -> native.Connection:
        """Create the native connection (overridden by tests to build a detached one)."""
        user = self.client.user
        if user is None:
            raise NotConnected("the discord.Client is not logged in")
        return native.Connection(bell, self.guild.id, user.id, settings, reconnect)

    def _abort_connect(self) -> None:
        """Synchronous cleanup of a failed or cancelled ``connect()`` (§6.2)."""
        if self._conn is not None:
            self._conn.close("connect_aborted")
        elif self._rsock is not None:
            with contextlib.suppress(OSError):
                self._rsock.close()
        if self._join_sent and not self._leave_sent:
            self._send_leave()
        self.cleanup()

    def _send_leave(self, *, quiet: bool = False) -> asyncio.Task[None] | None:
        """Send op 4 ``None`` in a background task registered as this voice state's owed leave
        (§6.2), so the next ``connect()`` orders its join after it. Never joins again afterwards.

        Returns ``None`` (nothing sent) when another client is registered for the guild: the leave
        would disconnect *that* client.
        """
        self._leave_sent = True
        slot = self._slot()
        if slot is not None and slot is not self:
            return None
        guild = self.guild
        try:
            loop = self._loop or asyncio.get_running_loop()
            task = loop.create_task(_best_effort_leave(guild, quiet=quiet), name=f"songbird-leave-{guild.id}")
        except RuntimeError:  # no running loop (or it is closed): nothing can be sent
            return None
        _track_leave(_leave_key(self.client, guild.id), task)
        return task

    async def disconnect(self, *, force: bool = False) -> None:
        """Leave the channel and tear down every stream, playback and track.

        Always calls :meth:`cleanup`; idempotent. Never raises for gateway errors (they are logged
        unless ``force=True``).

        Notes
        -----
        The leave op 4 is awaited for at most 5 s (not at all with ``force=True``, which discord.py
        passes when the client shuts down). A leave that discord.py's gateway rate limiter holds
        longer is not given up: it keeps running in the background, and the next ``connect()`` in
        the guild orders its join after it. Nothing is sent when another client is registered for
        the guild (a stale client must not disconnect it).
        """
        conn = self._conn
        if conn is None or self._finalized:
            self.cleanup()  # also stops a connect() that is still waiting for an owed leave
            return
        conn.request_leave()  # the VoiceState(None) echo then closes with "requested", not "kicked"
        leave = self._send_leave(quiet=force)
        try:
            if leave is not None and not force:
                done, _ = await asyncio.wait({leave}, timeout=_LEAVE_TIMEOUT_S)
                if not done:
                    _log.warning(
                        "voice leave (op 4) not sent within %.0f s (gateway rate limit?); tearing down "
                        "locally, the leave is still sent in the background",
                        _LEAVE_TIMEOUT_S,
                    )
        finally:
            conn.close("requested")  # synchronous T0–T9 first: cancellation cannot skip it
            self.cleanup()
            if self._pump is not None:
                await self._pump.closed(timeout=_PUMP_CLOSE_TIMEOUT_S)

    async def on_voice_state_update(self, data: GuildVoiceStatePayload, /) -> None:
        conn = self._conn
        if conn is None:
            return
        ch = utils._get_as_snowflake(data, "channel_id")
        if ch is None and not self._in_channel and not self._leave_sent:
            # The gateway has not placed us in a channel for this join yet, so a channel-less
            # update can only be the echo of an earlier leave (e.g. the best-effort leave of an
            # aborted connect, §6.2); forwarding it would close the new connection.
            return
        if ch is not None:
            self._in_channel = True
        conn.on_voice_state(data["session_id"], ch)
        if ch is not None and ch != self.channel.id and (moved := _voice_channel(self.guild.get_channel(ch))):
            self.channel = moved  # moderator move or move_to
        self._gateway_self_mute = bool(data.get("self_mute", False))
        self._gateway_self_deaf = bool(data.get("self_deaf", False))

    async def on_voice_server_update(self, data: VoiceServerUpdatePayload, /) -> None:
        conn = self._conn
        if conn is None:
            return
        # endpoint None: the voice server is being reallocated; a new update follows.
        conn.on_voice_server(data["token"], data.get("endpoint"))

    def cleanup(self) -> None:
        """Idempotent: close every child stream, close the connection and unregister from
        discord.py."""
        if self._finalized:
            return
        self._finalized = True
        if (abort := self._join_abort) is not None:
            _resolve_soon(abort)
        for child in list(self._children):
            with contextlib.suppress(Exception):
                child.close()
        if self._conn is not None:
            self._conn.close("cleanup")
        self._cancel_tasks()
        if self._queue is not None:
            self._queue._close()
        key, _ = self.channel._get_voice_client_key()
        state = self.client._connection
        if state._get_voice_client(key) is self:
            super().cleanup()

    # ── state ────────────────────────────────────────────────────────────────────────────────────

    @property
    def guild(self) -> discord.Guild:
        """The guild of the voice channel."""
        return self.channel.guild

    @property
    def user(self) -> discord.ClientUser:
        """The bot user."""
        user = self.client.user
        if user is None:
            raise NotConnected("the discord.Client is not logged in")
        return user

    @property
    def status(self) -> native.ConnectionStatus:
        """A copy of the connection status."""
        return self._require_conn(allow_closed=True).status()

    @property
    def ssrc(self) -> int | None:
        """Our SSRC on the current media connection."""
        return None if self._conn is None else self._conn.status().ssrc

    @property
    def timeline_ms(self) -> int:
        """Receive timeline position (20 ms ticks since connect); aligns chunks across streams."""
        return self._require_conn(allow_closed=True).timeline_ms

    @property
    def self_mute(self) -> bool:
        return self._self_mute

    @property
    def self_deaf(self) -> bool:
        return self._self_deaf

    def is_connected(self) -> bool:
        """``status.state == "connected"``."""
        return self._conn is not None and self._conn.status().state == "connected"

    async def wait_connected(self, timeout: float | None = None) -> None:
        """Wait for state ``"connected"``.

        Raises
        ------
        ConnectionClosed
            The connection closed.
        TimeoutError
            Not connected within ``timeout``.
        """
        pump = self._require_pump()
        async with asyncio.timeout(timeout):
            st = await pump.wait_status(lambda s: s.state in ("connected", "closed"))
        if st.state == "closed":
            raise ConnectionClosed(st.close_reason or "closed")

    async def move_to(self, channel: discord.abc.Snowflake | None, *, timeout: float = 30.0) -> None:
        """Move and wait until connected on the new channel; ``None`` disconnects.

        Streams, playbacks, events and the receive timeline survive the move (synthetic silence
        covers the gap and is reported in ``chunk.gap_ms``).

        Raises
        ------
        ConnectTimeout
            Not connected on the new channel within ``timeout`` (the connection keeps trying). The
            timeout also covers sending the op 4.
        ConnectFailed
            ``reason == "move_failed"``: the gateway never confirmed the move (Discord ignores an
            op 4 for a channel the bot may not join, e.g. without CONNECT permission or when it is
            full), or another move superseded it. The connection stays on (or rejoins) the channel
            Discord has the bot in, and :attr:`channel` is unchanged.
        ConnectionClosed
            The connection closed meanwhile.
        """
        if channel is None:
            await self.disconnect()
            return
        conn = self._require_conn()
        pump = self._require_pump()
        if channel.id == self.channel.id and conn.status().state == "connected":
            return
        self._check_may_join()
        deadline = asyncio.timeout(timeout)
        try:
            async with deadline:
                conn.request_move(channel.id)
                self._sent_flags = (self._self_mute, self._self_deaf)
                await self.guild.change_voice_state(
                    channel=channel, self_mute=self._self_mute, self_deaf=self._self_deaf
                )
                # Done when connected there, or when the target changed: an unconfirmed move
                # returned to the old channel (§4.5), or another move superseded this one.
                st = await pump.wait_status(
                    lambda s: (
                        (s.state == "connected" and s.channel_id == channel.id) or s.target_channel_id != channel.id
                    )
                )
        except TimeoutError:
            if deadline.expired():
                raise ConnectTimeout(timeout) from None
            raise
        if st.state == "closed":
            raise ConnectionClosed(st.close_reason or "closed")
        if st.state != "connected" or st.channel_id != channel.id:
            raise ConnectFailed("move_failed")
        if (moved := _voice_channel(self.guild.get_channel(channel.id))) is not None:
            self.channel = moved

    async def reconnect(self, *, timeout: float = 30.0) -> None:
        """Force a fresh voice session (op 4 leave + join).

        The recovery for stalled receive or DAVE (songbird #310/#315); ``VoiceConfig.auto_recover``
        does this automatically after a 5 s receive stall.

        Raises
        ------
        ConnectTimeout
            Not reconnected within ``timeout`` (which also covers sending both op 4s).
        ConnectionClosed
            The connection closed meanwhile.
        """
        conn = self._require_conn()
        pump = self._require_pump()
        self._check_may_join()
        deadline = asyncio.timeout(timeout)
        try:
            async with deadline:
                generation = conn.status().generation
                conn.begin_rejoin()  # Rejoining: the channel=None echo is not a kick
                await self.guild.change_voice_state(channel=None)
                self._check_may_join()  # disconnect()/close while leaving: never join again
                self._sent_flags = (self._self_mute, self._self_deaf)
                await self.guild.change_voice_state(
                    channel=self.channel, self_mute=self._self_mute, self_deaf=self._self_deaf
                )
                st = await pump.wait_status(lambda s: s.state == "connected" and s.generation > generation)
        except TimeoutError:
            if deadline.expired():
                raise ConnectTimeout(timeout) from None
            raise
        if st.state == "closed":
            raise ConnectionClosed(st.close_reason or "closed")

    async def set_self_mute(self, muted: bool) -> None:
        """Stop or resume sending audio; live streams hold their position while muted.

        Sends op 4 only while connected. Otherwise the flag is stored; set before ``connect()`` it
        wins over ``connect()``'s argument, and it is flushed with one op 4 as soon as the state
        becomes ``"connected"`` (§6.7). Never raises :class:`NotConnected`.
        """
        self._explicit_mute = muted
        self._self_mute = muted
        await self._apply_flags()

    async def set_self_deaf(self, deafened: bool) -> None:
        """Gateway deaf flag; same sending rules as :meth:`set_self_mute`."""
        self._explicit_deaf = deafened
        self._self_deaf = deafened
        await self._apply_flags()

    async def _apply_flags(self) -> None:
        conn = self._conn
        if conn is None or conn.closed:
            return
        conn.set_self_flags(self._self_mute, self._self_deaf)
        if conn.status().state == "connected":
            await self._send_flags()

    async def _send_flags(self) -> None:
        flags = (self._self_mute, self._self_deaf)
        if flags == self._sent_flags or not self._may_join():
            return
        self._sent_flags = flags
        await self.guild.change_voice_state(channel=self.channel, self_mute=flags[0], self_deaf=flags[1])

    def set_bitrate(self, bps: int | None) -> None:
        """Opus bitrate, ``6000..=510000`` bps, or ``None`` for songbird's default."""
        self._require_conn().set_bitrate(bps)

    def stats(self) -> dict[str, int]:
        """Receive, control and health counters (``ticks_real``, ``bad_packets``, ``panics``, …)."""
        return dict(self._require_conn(allow_closed=True).stats())

    # ── receive ──────────────────────────────────────────────────────────────────────────────────

    def listen(self, format: FormatLike = PCM16_48K_STEREO, *, user: Any | None = None, **options: Any) -> AudioStream:
        """``listen_user(user, ...)`` if ``user`` is given, else ``listen_mixed(...)``.

        The default format is Discord's native 48 kHz stereo PCM16LE (no resampling)."""
        if user is not None:
            return self.listen_user(user, format, **options)
        return self.listen_mixed(format, **options)

    def listen_user(
        self,
        user: Any,
        format: FormatLike = PCM16_48K_STEREO,
        *,
        chunk_ms: int = 20,
        max_queue_ms: int = 2_000,
        overflow: Overflow = "drop_oldest",
    ) -> AudioStream:
        """Continuous stream of one user: silence while they are quiet or absent.

        Parameters
        ----------
        user : int or discord.abc.Snowflake
        format : AudioFormat or str, default PCM16_48K_STEREO
            Output format; audio is resampled, down/up-mixed and re-encoded to it. The default is
            Discord's native 48 kHz stereo PCM16LE (no conversion).
        chunk_ms : int, default 20
            ``10..=1000``.
        max_queue_ms : int, default 2000
            Bound of the queue (``chunk_ms..=60000``).
        overflow : {"drop_oldest", "drop_newest", "error"}
        """
        conn, pump = self._require()
        sub = conn.subscribe_user(user_id(user), as_format(format), chunk_ms, max_queue_ms, overflow)
        return self._adopt(AudioStream(sub, pump, kind="user"))

    def listen_mixed(
        self,
        format: FormatLike = PCM16_48K_STEREO,
        *,
        chunk_ms: int = 20,
        max_queue_ms: int = 2_000,
        overflow: Overflow = "drop_oldest",
        include: Iterable[Any] | None = None,
        exclude: Iterable[Any] = (),
        ignore_bots: bool = True,
        policy: MixPolicy = "mix",
        floor_release_ms: int = 600,
    ) -> AudioStream:
        """Continuous, silence-filled mix of the channel; one chunk every ``chunk_ms``.

        Parameters
        ----------
        format : AudioFormat or str, default PCM16_48K_STEREO
            Output format (Discord's native 48 kHz stereo PCM16LE by default).
        include : iterable of users or None
            Mix only these users (``None`` = everyone, plus unmapped audio).
        exclude : iterable of users
            Never mix these users.
        ignore_bots : bool, default True
            Skip users classified as bots.
        policy : {"mix", "floor"}
            ``"floor"``: dominant-speaker mix; the first voiced user holds the floor until silent
            for ``floor_release_ms``. For consumers that expect a single speaker at a time.
        """
        conn, pump = self._require()
        inc = None if include is None else user_ids(include)
        exc = user_ids(exclude)
        sub = conn.subscribe_mixed(
            as_format(format), chunk_ms, max_queue_ms, overflow, inc, exc, ignore_bots, policy, floor_release_ms
        )
        mix_filter = {
            "include": inc,
            "exclude": exc,
            "ignore_bots": ignore_bots,
            "policy": policy,
            "floor_release_ms": floor_release_ms,
        }
        return self._adopt(AudioStream(sub, pump, kind="mixed", mix_filter=mix_filter))

    def listen_users(
        self,
        format: FormatLike = PCM16_48K_STEREO,
        *,
        chunk_ms: int = 20,
        max_queue_ms: int = 2_000,
        overflow: Overflow = "drop_oldest",
        include: Iterable[Any] | None = None,
        ignore_bots: bool = True,
        close_on_leave: bool = True,
    ) -> UserStreams:
        """Router yielding one :class:`AudioStream` per user when they first speak (each in
        ``format``, Discord's native 48 kHz stereo PCM16LE by default)."""
        conn, pump = self._require()
        inc = None if include is None else user_ids(include)
        router = conn.subscribe_router(
            as_format(format), chunk_ms, max_queue_ms, overflow, inc, ignore_bots, close_on_leave
        )
        return self._adopt(UserStreams(router, pump, register=self._children.add))

    def events(self, *, max_queue: int = 256, kinds: Iterable[type[Any]] | None = None) -> EventStream:
        """A bounded event stream (drop-oldest, with ``VoiceEvent.Lagged``).

        Parameters
        ----------
        max_queue : int, default 256
            ``1..=4096``.
        kinds : iterable of VoiceEvent variant classes or None
            Only these kinds (``None`` = all).

        Notes
        -----
        With ``VoiceConfig(receive=False)``, ``UserMapped`` and ``UserLeft`` are still reported (they
        come from the voice WebSocket); ``SpeakingStarted``/``SpeakingStopped`` need received audio
        and never fire.
        """
        conn, pump = self._require()
        return self._adopt(EventStream(conn.subscribe_events(max_queue, kind_mask(kinds)), pump))

    # ── send ─────────────────────────────────────────────────────────────────────────────────────

    def open_playback(
        self,
        format: FormatLike = PCM16_48K_STEREO,
        *,
        max_buffer_ms: int = 60_000,
        prebuffer_ms: int = 0,
        idle_ms: int = 250,
        auto_pause: bool = True,
        max_gap_ms: int | None = None,
        output_latency_ms: int = 40,
        volume: float = 1.0,
        when_disconnected: Literal["buffer", "discard"] = "buffer",
    ) -> PlaybackStream:
        """Realtime PCM output fed by ``write()``. Several may be open at once; songbird mixes them.

        Starts paused (nothing is sent until the first write). ``prebuffer_ms`` and ``max_gap_ms``
        must be ``<= max_buffer_ms / 2``.

        Parameters
        ----------
        format : AudioFormat or str, default PCM16_48K_STEREO
            Format of every write; other formats are resampled to Discord's 48 kHz stereo. The
            default is Discord's native format (no conversion).
        max_buffer_ms : int, default 60000
            Buffer bound (``100..=120000``); a full buffer raises ``PlaybackFull`` / makes
            ``write()`` wait.
        prebuffer_ms : int, default 0
            After an idle period or ``clear()``, hold playback until this much is buffered (or it
            times out), to absorb network jitter.
        idle_ms : int, default 250
            Empty this long after real audio → idle (``PlaybackIdle``; auto-pause).
        auto_pause : bool, default True
            Pause the track while idle so no silence is sent.
        max_gap_ms : int or None, default None
            Longest silence inserted from ``at_ms`` timestamps; ``None`` means
            ``min(2000, max_buffer_ms // 2)``.
        output_latency_ms : int, default 40
            Assumed delay after the encoder, for ``heard_ms``.
        volume : float, default 1.0
            ``0.0..=4.0``.
        when_disconnected : {"buffer", "discard"}, default "buffer"
            Keep or drop buffered audio while the voice link is down.
        """
        conn, pump = self._require()
        fmt = as_format(format)
        if max_gap_ms is None:
            max_gap_ms = min(2_000, max_buffer_ms // 2) if isinstance(max_buffer_ms, int) else 2_000
        pb = conn.open_playback(
            fmt,
            max_buffer_ms,
            prebuffer_ms,
            idle_ms,
            auto_pause,
            max_gap_ms,
            output_latency_ms,
            volume,
            when_disconnected,
        )
        return self._adopt(PlaybackStream(pb, pump, fmt))

    def open_encoded_stream(self, *, max_buffer_bytes: int = 1 << 20, volume: float = 1.0) -> EncodedStream:
        """Push-based encoded input (mp3/ogg/…); starts when the first decoded audio is ready."""
        conn, pump = self._require()
        return self._adopt(EncodedStream(conn.open_encoded(max_buffer_bytes, volume), pump))

    def open_opus_stream(self, *, max_packets: int = 250, volume: float = 1.0) -> OpusPacketStream:
        """Push-based 20 ms Opus packets (passthrough when it is the only track)."""
        conn, pump = self._require()
        return self._adopt(OpusPacketStream(conn.open_opus(max_packets, volume), pump))

    def play(self, source: Source, *, volume: float = 1.0, loops: int | None = 0, paused: bool = False) -> TrackHandle:
        """Start a track now, mixed with the others. Synchronous and O(1).

        ``loops=None`` loops forever. Raises ``LimitExceeded`` beyond 32 playing tracks, or beyond
        48 tracks held by songbird (``resident_tracks``: while the voice link is down, stopped tracks
        and closed streams stay in its mixer until the link is up).
        """
        conn, pump = self._require()
        return TrackHandle(conn.play(source, volume, loops, paused), pump)

    def enqueue(
        self, source: Source, *, volume: float = 1.0, loops: int | None = 0, paused: bool = False
    ) -> TrackHandle:
        """``self.queue.enqueue(...)``."""
        return self.queue.enqueue(source, volume=volume, loops=loops, paused=paused)

    @property
    def queue(self) -> TrackQueue:
        """The sequential track queue."""
        if self._queue is None:
            _conn, pump = self._require()
            self._queue = TrackQueue(pump)
        return self._queue

    def stop_all(self) -> None:
        """Stop every track, queue entry and live stream (they close); the connection stays up."""
        self._require_conn().stop_all()

    # ── internals ────────────────────────────────────────────────────────────────────────────────

    def _require_conn(self, *, allow_closed: bool = False) -> native.Connection:
        conn = self._conn
        if conn is None:
            raise NotConnected("connect() has not been called")
        if not allow_closed and conn.closed:
            raise ConnectionClosed(conn.status().close_reason or "closed")
        return conn

    def _require_pump(self) -> _Pump:
        if self._pump is None:
            raise NotConnected("connect() has not been called")
        return self._pump

    def _require(self) -> tuple[native.Connection, _Pump]:
        return self._require_conn(), self._require_pump()

    def _slot(self) -> Any:
        """The voice client discord.py currently routes this guild's gateway updates to."""
        key, _ = self.channel._get_voice_client_key()
        return self.client._connection._get_voice_client(key)

    def _may_join(self) -> bool:
        """Whether an op 4 that puts the bot in a channel may still be sent.

        Never after a leave was sent (``disconnect()``, an aborted connect) or the connection closed:
        such an op 4 would put the bot back in the channel with nothing registered to serve it.

        Never by a client that discord.py no longer routes gateway updates to: a non-sharded
        re-IDENTIFY's ``READY`` replaces ``_voice_clients`` without calling ``cleanup()``, and the
        app may have connected a new client since. Every op 4 acts on the bot's one voice state in
        the guild, so a stale client's rejoin would kick the registered one. Such a client is
        cleaned up here instead, without gateway traffic.
        """
        conn = self._conn
        if conn is None or conn.closed or self._finalized or self._leave_sent:
            return False
        if self._slot() is not self:
            _log.warning(
                "songbird client of guild %s is no longer discord.py's registered voice client "
                "(the gateway re-identified, or another client connected); closing it locally",
                self.guild.id,
            )
            self.cleanup()
            return False
        return True

    def _check_may_join(self) -> None:
        if not self._may_join():
            conn = self._conn
            reason = conn.status().close_reason if conn is not None else None
            raise ConnectionClosed(reason or "requested")

    def _adopt[T](self, child: T) -> T:
        self._children.add(child)
        return child

    def _spawn(self, coro: Coroutine[Any, Any, Any], *, name: str | None = None) -> asyncio.Task[Any]:
        loop = self._loop or asyncio.get_running_loop()
        task = loop.create_task(coro, name=name)
        self._tasks.add(task)
        task.add_done_callback(self._task_done)
        return task

    def _task_done(self, task: asyncio.Task[Any]) -> None:
        self._tasks.discard(task)
        if not task.cancelled() and (exc := task.exception()) is not None:
            _log.error("songbird background task %s failed", task.get_name(), exc_info=exc)

    def _cancel_tasks(self) -> None:
        """Cancel the gateway helper tasks (rejoin, flag flush): nothing is sent after a close."""
        cancel_tasks(self._tasks)

    # Pump callbacks (loop thread).

    def _on_status(self, st: native.ConnectionStatus) -> None:
        # Flush on connect (§6.7): a flag changed while not connected is sent once, here.
        if st.state == "connected" and (self._self_mute, self._self_deaf) != self._sent_flags:
            self._spawn(self._send_flags(), name="songbird-flags")

    def _on_control(self, ctrl: Any) -> None:
        match ctrl:
            case native.Control.ChannelMoved(channel_id=ch):
                if (moved := _voice_channel(self.guild.get_channel(ch))) is not None:
                    self.channel = moved
            case native.Control.ClassifyUsers():
                self._classify_users()
            case native.Control.Rejoin(attempt=attempt, channel_id=ch):
                self._spawn(self._rejoin(attempt, ch), name=f"songbird-rejoin-{attempt}")
            case native.Control.Closed(reason=reason):
                self._finalize(reason)
            case _:
                _log.debug("ignoring unknown control %r", ctrl)

    def _classify_users(self) -> None:
        conn = self._conn
        if conn is None:
            return
        users = conn.take_unclassified_users()
        if not users:
            return
        guild = self.guild
        pairs = []
        for uid in users:
            member = guild.get_member(uid)
            pairs.append((uid, bool(member is not None and member.bot)))
        conn.set_users_bot(pairs)

    async def _rejoin(self, attempt: int, channel_id: int) -> None:
        if not self._may_join():
            return
        _log.info("voice rejoin attempt %d (channel %d)", attempt, channel_id)
        await self.guild.change_voice_state(channel=None)
        if not self._may_join():  # disconnect() or a close happened while leaving: stay out
            return
        self._sent_flags = (self._self_mute, self._self_deaf)
        await self.guild.change_voice_state(
            channel=discord.Object(id=channel_id), self_mute=self._self_mute, self_deaf=self._self_deaf
        )

    def _on_tracks(self) -> None:
        if self._queue is not None:
            self._queue._on_tracks()

    def _finalize(self, reason: str) -> None:
        """The connection closed (control ``Closed`` or pump EOF): close children, clean up.

        After a terminal failure the gateway may still list the bot in the channel (the last op 4
        we sent was a join), so a best-effort leave is sent, exactly as for an aborted connect
        (§6.2): otherwise the bot would stay visible in the channel with nothing serving it.
        """
        if not self._finalized:
            _log.debug("songbird connection in guild %s closed: %s", self.guild.id, reason)
            if reason in _LEAVE_AFTER and self._join_sent and not self._leave_sent:
                self._send_leave()
        self.cleanup()


def _voice_channel(ch: Any) -> VocalGuildChannel | None:
    """``ch`` if it is a connectable guild channel (voice or stage), else ``None``."""
    if ch is not None and callable(getattr(ch, "_get_voice_client_key", None)):
        return ch
    return None


def _leave_key(client: discord.Client, guild_id: int) -> tuple[int, int]:
    """The voice state an op 4 acts on: one per bot user per guild (separate bots never wait on,
    or cancel, each other's leaves)."""
    user = client.user
    return (user.id if user is not None else 0, guild_id)


def _keep_alive(_task: asyncio.Task[None]) -> None:
    """Timer callback that only holds a leave task (see :func:`_track_leave`)."""


def _prune_owed_leaves() -> None:
    """Drop entries whose leave finished, was collected, or belongs to a closed event loop."""
    for key, ref in list(_pending_leaves.items()):
        task = ref()
        if task is None or task.done() or task.get_loop().is_closed():
            if _pending_leaves.get(key) is ref:
                _pending_leaves.pop(key, None)


def _track_leave(key: tuple[int, int], task: asyncio.Task[None]) -> None:
    """Register ``task`` as the owed leave of ``key`` (replacing an older one).

    The strong reference that keeps the running task alive is a timer handle on the task's own
    loop: ``loop.close()`` drops it, so a loop closed without running the leave (e.g.
    ``run_until_complete`` interrupted, then ``close()``) lets the task, its guild and the old
    ``discord.Client`` be collected. The module dict holds only a weak reference.
    """
    keep = task.get_loop().call_later(_LEAVE_SEND_TIMEOUT_S + _LEAVE_TIMEOUT_S, _keep_alive, task)
    _prune_owed_leaves()
    ref = weakref.ref(task)
    _pending_leaves[key] = ref

    def _done(t: asyncio.Task[None]) -> None:
        keep.cancel()
        cur = _pending_leaves.get(key)
        if cur is not None and cur() is t:
            _pending_leaves.pop(key, None)

    task.add_done_callback(_done)


def _take_owed_leave(key: tuple[int, int]) -> asyncio.Task[None] | None:
    """Pop the owed leave of ``key`` if it is still running."""
    _prune_owed_leaves()
    ref = _pending_leaves.pop(key, None)
    task = ref() if ref is not None else None
    return task if task is not None and not task.done() else None


def _owe_again(key: tuple[int, int], task: asyncio.Task[None]) -> None:
    """Put back a leave that is still owed (unless a newer one was registered meanwhile)."""
    if task.done():
        return
    cur = _pending_leaves.get(key)
    if cur is None or (t := cur()) is None or t.done():
        _pending_leaves[key] = weakref.ref(task)


def _resolve_soon(fut: asyncio.Future[None]) -> None:
    """Resolve ``fut`` from any thread (cleanup() may run off its loop)."""

    def _set() -> None:
        if not fut.done():
            fut.set_result(None)

    loop = fut.get_loop()
    try:
        running = asyncio.get_running_loop()
    except RuntimeError:
        running = None
    if running is loop:
        _set()
    elif not loop.is_closed():
        with contextlib.suppress(RuntimeError):
            loop.call_soon_threadsafe(_set)


async def _await_pending_leave(key: tuple[int, int], prev: asyncio.Task[None], abort: asyncio.Future[None]) -> None:
    """Wait (at most 5 s) for an owed leave of this voice state before joining (§6.2).

    On timeout the leave is cancelled, so it can never be sent after the new join. If *this* wait is
    cancelled (the new ``connect()`` was cancelled or timed out) or ``abort`` resolves (its client
    was disconnected), the leave keeps running and stays registered for the next ``connect()``: it
    is still owed, and ``asyncio.wait_for`` would have cancelled it. A leave scheduled on another
    event loop (an earlier ``asyncio.run``) is ignored: it can neither be awaited nor cancelled from
    here, and its gateway session is not ours.
    """
    if prev.get_loop() is not asyncio.get_running_loop():
        return
    try:
        done, _ = await asyncio.wait({prev, abort}, timeout=_LEAVE_TIMEOUT_S, return_when=asyncio.FIRST_COMPLETED)
    except asyncio.CancelledError:
        _owe_again(key, prev)
        raise
    if prev in done:
        return
    if abort.done():
        _owe_again(key, prev)
        return
    prev.cancel()


async def _best_effort_leave(guild: discord.Guild, *, quiet: bool = False) -> None:
    try:
        async with asyncio.timeout(_LEAVE_SEND_TIMEOUT_S):
            await guild.change_voice_state(channel=None)
    except Exception:
        if not quiet:
            _log.warning("voice leave (op 4) did not complete", exc_info=True)


async def connect(
    channel: discord.abc.Connectable,
    *,
    config: VoiceConfig | None = None,
    timeout: float = 30.0,
    reconnect: bool = True,
    self_deaf: bool = False,
    self_mute: bool = False,
) -> SongbirdClient:
    """Shortcut for ``await channel.connect(cls=SongbirdClient.with_config(config), ...)``."""
    return await channel.connect(
        cls=SongbirdClient.with_config(config),
        timeout=timeout,
        reconnect=reconnect,
        self_deaf=self_deaf,
        self_mute=self_mute,
    )
