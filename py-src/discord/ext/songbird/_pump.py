"""The doorbell pump: the only place where Rust readiness reaches asyncio (§5.4, §6.4).

Rust never calls Python. It sets bits in a ``WakeHub`` and writes at most one byte to a socket per
notification burst. One ``_Pump`` task per connection awaits ``loop.sock_recv`` on the read end,
takes the ready wake ids (``Connection.take_ready``) and resolves the futures waiting on them. Every
wait is *pure*: a consumer arms the native object while observing "empty", awaits the wake id,
then pops synchronously again — so cancellation can never lose or duplicate data (§5.5).

Reserved ids: ``WAKE_CONN`` (status changed), ``WAKE_OPS`` (op results), ``WAKE_CTRL`` (control
slots), ``WAKE_TRACKS`` (a track's ``ended`` changed, level-triggered). Ids 4–255 belong to streams.

The pump holds the client only through a weak reference. If the client is collected without
``disconnect()``, the weakref callback closes the native connection (``"dropped"``); that shuts the
doorbell down, ``sock_recv`` returns ``b""`` and the pump exits. The task's ``finally`` closes the
connection too (``"loop_closed"``), so a loop that dies always tears it down. Helper tasks the library
starts for a connection (a queued stream's feed) run through :meth:`_Pump.spawn` and are cancelled when
the pump shuts down, so they cannot keep a collected client's connection alive.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import socket
import weakref
from collections.abc import Callable, Coroutine, Iterable
from typing import TYPE_CHECKING, Any

from . import native
from .errors import ConnectionClosed

if TYPE_CHECKING:
    from .client import SongbirdClient

__all__ = ["_Pump", "cancel_tasks"]

_log = logging.getLogger("discord.ext.songbird")

_RECV_SIZE = 256


class _Pump:
    """Wake-up dispatcher of one connection (internal)."""

    __slots__ = (
        "_loop",
        "_rsock",
        "_conn",
        "_owner",
        "_waiters",
        "_status_waiters",
        "_op_waiters",
        "_track_waiters",
        "_closed",
        "_tasks",
        "_task",
        "__weakref__",
    )

    def __init__(
        self,
        loop: asyncio.AbstractEventLoop,
        rsock: socket.socket,
        conn: native.Connection,
        owner: SongbirdClient,
    ) -> None:
        self._loop = loop
        self._rsock = rsock
        self._conn = conn
        self._owner: weakref.ref[SongbirdClient] = weakref.ref(owner, self._owner_gone)
        self._waiters: dict[int, list[asyncio.Future[None]]] = {}
        self._status_waiters: list[asyncio.Future[None]] = []
        self._op_waiters: dict[int, asyncio.Future[Any]] = {}
        self._track_waiters: dict[native.NativeTrack, list[asyncio.Future[None]]] = {}
        self._closed = False
        self._tasks: set[asyncio.Task[Any]] = set()
        self._task: asyncio.Task[None] = loop.create_task(self._run(), name="songbird-pump")

    # ── lifecycle ────────────────────────────────────────────────────────────────────────────────

    @property
    def conn(self) -> native.Connection:
        """The native connection."""
        return self._conn

    @property
    def loop(self) -> asyncio.AbstractEventLoop:
        """The event loop every wait is bound to."""
        return self._loop

    @property
    def is_closed(self) -> bool:
        """Whether the pump has seen the end of the doorbell (or was stopped)."""
        return self._closed

    def _owner_gone(self, _ref: weakref.ref[Any]) -> None:
        # Runs in the GC, on any thread: only thread-safe, synchronous native calls here.
        try:
            self._conn.close("dropped")
        except Exception:  # never raise from a weakref callback
            pass

    async def _run(self) -> None:
        try:
            while True:
                data = await self._loop.sock_recv(self._rsock, _RECV_SIZE)
                self._dispatch()
                if not data:
                    break
        except asyncio.CancelledError:
            raise
        except OSError:
            _log.debug("songbird pump socket failed", exc_info=True)
        finally:
            self._shutdown()

    def _shutdown(self) -> None:
        """T10: close, final dispatch, resolve every waiter, finalize the client."""
        if self._closed:
            return
        try:
            self._conn.close("loop_closed")  # idempotent: the first close's reason wins
        except Exception:
            _log.debug("closing the native connection failed", exc_info=True)
        try:
            self._dispatch()
        except Exception:
            _log.debug("final pump dispatch failed", exc_info=True)
        self._closed = True
        self._wake_everyone()
        # Even when the client was collected (nothing will call its cleanup()): a helper waiting on
        # its source must not hold the connection until that source yields.
        cancel_tasks(self._tasks)
        try:
            self._rsock.close()
        except OSError:
            pass
        owner = self._owner()
        if owner is not None:
            try:
                owner._finalize(self._close_reason())
            except Exception:
                _log.exception("songbird client finalisation failed")

    def _close_reason(self) -> str:
        try:
            return self._conn.status().close_reason or "closed"
        except Exception:
            return "closed"

    def _wake_everyone(self) -> None:
        for futs in self._waiters.values():
            for f in futs:
                if not f.done():
                    f.set_result(None)
        self._waiters.clear()
        self._resolve_status()
        for f in self._op_waiters.values():
            if not f.done():
                f.set_exception(ConnectionClosed(self._close_reason()))
        self._op_waiters.clear()
        self._resolve_tracks(force=True)

    async def closed(self, timeout: float | None = None) -> bool:
        """Wait (cancellation-safely) until the pump task has finished; ``False`` on timeout."""
        if self._task.done():
            return True
        done, _ = await asyncio.wait({self._task}, timeout=timeout)
        return bool(done)

    def spawn(self, coro: Coroutine[Any, Any, Any], *, name: str | None = None) -> asyncio.Task[Any]:
        """Run ``coro`` on the pump's loop as a helper task of this connection; it is cancelled when
        the pump shuts down (loop thread only)."""
        task = self._loop.create_task(coro, name=name)
        if self._closed:
            task.cancel()
            return task
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)
        return task

    def stop(self) -> None:
        """Cancel the pump task (its ``finally`` closes the connection)."""
        if not self._task.done():
            self._task.cancel()

    # ── dispatch ─────────────────────────────────────────────────────────────────────────────────

    def _dispatch(self) -> None:
        for wake_id in self._conn.take_ready():
            if wake_id == native.WAKE_CONN:
                self._on_status()
            elif wake_id == native.WAKE_OPS:
                self._on_ops()
            elif wake_id == native.WAKE_CTRL:
                self._on_ctrl()
            elif wake_id == native.WAKE_TRACKS:
                self._on_tracks()
            else:
                futs = self._waiters.pop(wake_id, None)
                if futs:
                    for f in futs:
                        if not f.done():
                            f.set_result(None)

    def _on_status(self) -> None:
        self._resolve_status()
        owner = self._owner()
        if owner is not None:
            try:
                owner._on_status(self._conn.status())
            except Exception:
                _log.exception("songbird status handler failed")

    def _resolve_status(self) -> None:
        waiters, self._status_waiters = self._status_waiters, []
        for f in waiters:
            if not f.done():
                f.set_result(None)

    def _on_ops(self) -> None:
        for op_id, result in self._conn.take_op_results():
            fut = self._op_waiters.pop(op_id, None)
            if fut is None or fut.done():
                continue  # the waiter was cancelled: drop the result
            if isinstance(result, BaseException):
                fut.set_exception(result)
            else:
                fut.set_result(result)

    def _on_ctrl(self) -> None:
        controls = self._conn.take_controls()
        owner = self._owner()
        if owner is None:
            # The client was collected: nothing can act on the controls any more.
            self._conn.close("dropped")
            return
        for ctrl in controls:
            try:
                owner._on_control(ctrl)
            except Exception:
                _log.exception("songbird control handler failed for %r", ctrl)

    def _on_tracks(self) -> None:
        self._resolve_tracks(force=False)
        owner = self._owner()
        if owner is not None:
            try:
                owner._on_tracks()
            except Exception:
                _log.exception("songbird track handler failed")

    def _resolve_tracks(self, *, force: bool) -> None:
        # Level-triggered (§6.4): re-check `ended` for every track that has waiters.
        for track in list(self._track_waiters):
            if force or track.ended:
                for f in self._track_waiters.pop(track):
                    if not f.done():
                        f.set_result(None)

    # ── waits (loop thread only) ─────────────────────────────────────────────────────────────────

    def check_loop(self) -> None:
        """Raise ``RuntimeError`` unless called on the pump's event loop. Every awaitable of the
        library calls this first, before any fast path, so awaiting from another loop fails
        deterministically rather than only when it would have to wait (§9.10 rule 2)."""
        try:
            running = asyncio.get_running_loop()
        except RuntimeError:
            running = None
        if running is not self._loop:
            raise RuntimeError(
                "discord.ext.songbird objects can only be awaited on the event loop that created the "
                "connection; use the *_nowait methods from other threads"
            )

    async def wait(self, wake_id: int) -> None:
        """Wait until ``wake_id`` is rung (or the connection closes). The caller re-polls."""
        self.check_loop()
        if self._closed:
            return
        fut: asyncio.Future[None] = self._loop.create_future()
        futs = self._waiters.setdefault(wake_id, [])
        futs.append(fut)
        try:
            await fut
        finally:
            futs = self._waiters.get(wake_id)
            if futs is not None:
                try:
                    futs.remove(fut)
                except ValueError:
                    pass
                if not futs:
                    del self._waiters[wake_id]

    async def wait_status(self, predicate: Callable[[native.ConnectionStatus], bool]) -> native.ConnectionStatus:
        """Wait until ``predicate(status)`` holds or the connection is closed; returns the status."""
        self.check_loop()
        while True:
            st = self._conn.status()
            if predicate(st) or st.state == "closed" or self._closed:
                return st
            fut: asyncio.Future[None] = self._loop.create_future()
            self._status_waiters.append(fut)
            try:
                await fut
            finally:
                try:
                    self._status_waiters.remove(fut)
                except ValueError:
                    pass

    async def wait_op(self, op_id: int) -> Any:
        """Wait for the result of a native op (``TrackInfo``, ``float``) or raise its exception."""
        self.check_loop()
        if self._closed:
            raise ConnectionClosed(self._close_reason())
        fut: asyncio.Future[Any] = self._loop.create_future()
        self._op_waiters[op_id] = fut
        try:
            return await fut
        finally:
            if self._op_waiters.get(op_id) is fut:
                del self._op_waiters[op_id]

    async def wait_track(self, track: native.NativeTrack) -> None:
        """Wait until ``track.ended`` (level-triggered on ``WAKE_TRACKS``)."""
        self.check_loop()
        while not track.ended and not self._closed:
            fut: asyncio.Future[None] = self._loop.create_future()
            futs = self._track_waiters.setdefault(track, [])
            futs.append(fut)
            try:
                await fut
            finally:
                futs = self._track_waiters.get(track)
                if futs is not None:
                    try:
                        futs.remove(fut)
                    except ValueError:
                        pass
                    if not futs:
                        del self._track_waiters[track]


def cancel_tasks(tasks: Iterable[asyncio.Task[Any]]) -> None:
    """Cancel every pending task of ``tasks`` except the running one; thread-safe."""
    try:
        current = asyncio.current_task()
        running = asyncio.get_running_loop()
    except RuntimeError:
        current = running = None
    for task in list(tasks):
        if task is current or task.done():
            continue
        loop = task.get_loop()
        if loop is running:
            task.cancel()
        elif not loop.is_closed():
            with contextlib.suppress(RuntimeError):
                loop.call_soon_threadsafe(task.cancel)
