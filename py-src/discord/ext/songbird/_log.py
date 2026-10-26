"""The ``songbird-log`` forwarder thread (§11.2).

Rust never attaches to the interpreter from its own threads (I2). Log records are pushed into a
bounded Rust ring buffer instead, and this **Python-created** daemon thread pulls them with
``native._log_wait`` (which parks *detached* from the interpreter) and re-emits them through
:mod:`logging`:

* records of this library → ``discord.ext.songbird.native.<module>``;
* records of songbird → ``discord.ext.songbird.songbird.<module>``;
* anything else → ``discord.ext.songbird.native.<crate>.<module>``.

Delivery works without any connection. Rust-side filtering is set with :func:`set_log_levels`.
"""

from __future__ import annotations

import logging
import threading

from . import native

__all__ = ["set_log_levels", "start", "stop"]

_ROOT = "discord.ext.songbird"
_NATIVE = f"{_ROOT}.native"
_SONGBIRD = f"{_ROOT}.songbird"
# Rust crate names whose records belong to this library.
_OWN_CRATES = ("sbcore", "discord_ext_songbird_native")
_BATCH = 256
_WAIT_S = 0.25

_lock = threading.Lock()
_thread: threading.Thread | None = None
_stopping = threading.Event()
_names: dict[str, str] = {}


def logger_name(target: str) -> str:
    """The :mod:`logging` logger name for a Rust log ``target`` (``crate::module::path``)."""
    name = _names.get(target)
    if name is not None:
        return name
    crate, _, rest = target.partition("::")
    tail = rest.replace("::", ".")
    if crate in _OWN_CRATES:
        name = f"{_NATIVE}.{tail}" if tail else _NATIVE
    elif crate == "songbird":
        name = f"{_SONGBIRD}.{tail}" if tail else _SONGBIRD
    else:
        name = f"{_NATIVE}.{crate}.{tail}" if tail else f"{_NATIVE}.{crate}"
    if len(_names) < 4096:
        _names[target] = name
    return name


def _emit(level: int, target: str, message: str, unix_ts: float) -> None:
    logger = logging.getLogger(logger_name(target))
    if not logger.isEnabledFor(level):
        return
    record = logger.makeRecord(
        logger.name,
        level,
        "(rust)",
        0,
        message,
        (),
        None,
        func=None,
        extra={"rust_target": target},
    )
    record.created = unix_ts
    record.msecs = (unix_ts - int(unix_ts)) * 1000.0
    logger.handle(record)


def _drain_once(timeout_s: float) -> None:
    for level, target, message, unix_ts in native._log_wait(_BATCH, timeout_s):
        try:
            _emit(level, target, message, unix_ts)
        except Exception:  # a broken handler must not kill the forwarder
            logging.getLogger(_ROOT).debug("failed to emit a native log record", exc_info=True)
    dropped = native._log_dropped()
    if dropped:
        logging.getLogger(_NATIVE).warning("%d log records dropped", dropped)


def _run() -> None:
    while not _stopping.is_set():
        try:
            _drain_once(_WAIT_S)
        except Exception:
            logging.getLogger(_ROOT).exception("songbird-log forwarder failed; stopping")
            return
    # Final drain after shutdown was requested (records logged during teardown).
    try:
        _drain_once(0.0)
    except Exception:
        pass


def start() -> None:
    """Start the ``songbird-log`` daemon thread (idempotent)."""
    global _thread
    with _lock:
        if _thread is not None and _thread.is_alive():
            return
        _stopping.clear()
        _thread = threading.Thread(target=_run, name="songbird-log", daemon=True)
        _thread.start()


def stop(timeout: float = 0.5) -> None:
    """Ask the forwarder to finish (after a final drain) and join it for at most ``timeout`` s."""
    global _thread
    with _lock:
        thread, _thread = _thread, None
    _stopping.set()
    if thread is not None and thread is not threading.current_thread():
        thread.join(timeout)


def is_running() -> bool:
    """Whether the forwarder thread is alive."""
    thread = _thread
    return thread is not None and thread.is_alive()


def _after_fork_in_child() -> None:
    # Threads do not survive fork(); the native module is unusable in the child anyway (§4.10).
    global _thread
    _thread = None


def _level(value: int | str) -> int | str:
    if isinstance(value, bool) or not isinstance(value, int | str):
        raise TypeError(f"log level must be an int or a level name, got {type(value).__name__}")
    return value


def set_log_levels(*, library: int | str = "INFO", songbird: int | str = "WARNING") -> None:
    """Set Rust-side log filtering.

    Parameters
    ----------
    library : int or str, default "INFO"
        Level of this library's native records (``discord.ext.songbird.native.*``).
    songbird : int or str, default "WARNING"
        Level of songbird's records (``discord.ext.songbird.songbird.*``).

    Notes
    -----
    Filtering happens before a record is formatted, so disabled levels cost nothing. ``logging``
    levels and handlers still apply afterwards. ``"CRITICAL"`` disables a source.
    """
    lib = _level(library)
    sb = _level(songbird)
    for crate in _OWN_CRATES:
        native.set_log_level(crate, lib)
    native.set_log_level("songbird", sb)
