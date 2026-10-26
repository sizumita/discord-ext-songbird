"""Pull-based logging (§11.2, §13.3 ``test_logging.py``)."""

from __future__ import annotations

import logging
import time
from typing import Any

import pytest
from conftest import flush_native_logs, run_isolated
from discord.ext import songbird
from discord.ext.songbird import _log, native


@pytest.mark.parametrize(
    ("target", "name"),
    [
        ("sbcore::rx::pipeline", "discord.ext.songbird.native.rx.pipeline"),
        ("discord_ext_songbird_native::py::connection", "discord.ext.songbird.native.py.connection"),
        ("songbird::driver::tasks::ws", "discord.ext.songbird.songbird.driver.tasks.ws"),
        ("songbird", "discord.ext.songbird.songbird"),
        ("rubato::sinc", "discord.ext.songbird.native.rubato.sinc"),
        ("sbcore", "discord.ext.songbird.native"),
    ],
)
def test_logger_names(target: str, name: str) -> None:
    assert _log.logger_name(target) == name


def test_forwarder_thread_is_running() -> None:
    assert _log.is_running()
    import threading

    assert any(t.name == "songbird-log" and t.daemon for t in threading.enumerate())


@pytest.mark.fake_only
def test_records_reach_logging_without_a_connection(caplog: pytest.LogCaptureFixture) -> None:
    caplog.set_level(logging.DEBUG, logger="discord.ext.songbird")
    assert native._emit_log(20, "sbcore::rx::pipeline", "hello from a rust thread")  # type: ignore
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        recs = [r for r in caplog.records if r.getMessage() == "hello from a rust thread"]
        if recs:
            break
        time.sleep(0.01)
    else:
        pytest.fail("the record was not forwarded")
    (rec,) = recs
    assert rec.name == "discord.ext.songbird.native.rx.pipeline"
    assert rec.levelno == logging.INFO
    assert rec.rust_target == "sbcore::rx::pipeline"  # type: ignore


@pytest.mark.fake_only
def test_rust_side_filtering() -> None:
    songbird.set_log_levels(library="WARNING")
    try:
        assert not native._emit_log(20, "sbcore::x", "filtered")  # type: ignore
        assert native._emit_log(30, "sbcore::x", "kept")  # type: ignore
    finally:
        songbird.set_log_levels()


def test_log_wait_contract() -> None:
    recs = native._log_wait(16, 0.0)
    assert isinstance(recs, list)
    for level, target, message, ts in recs:
        assert isinstance(level, int) and isinstance(target, str) and isinstance(message, str)
        assert isinstance(ts, float)
    assert native._log_dropped() >= 0


def _voice_session(testing: Any, channel_id: int, *, session: str = "s", token: str = "t") -> None:
    """A detached connection's join → voice state → voice server → close, which the native side
    logs at DEBUG (``sbcore::conn::shared``)."""
    conn, harness = testing.detached_connection(1, 2, songbird.VoiceConfig().to_native())
    conn.request_join(channel_id, False, False)
    conn.on_voice_state(session, channel_id)
    conn.on_voice_server(token, "wss://example.discord.media/")
    harness.step(100)
    conn.close("requested")


@pytest.mark.real_only
def test_token_never_logged(testing: Any, caplog: pytest.LogCaptureFixture) -> None:
    caplog.set_level(logging.DEBUG, logger="discord.ext.songbird")
    songbird.set_log_levels(library="DEBUG", songbird="DEBUG")
    try:
        _voice_session(testing, 3, session="session-SECRET-SESSION", token="SECRET-TOKEN-123")
    finally:
        songbird.set_log_levels()
    flush_native_logs()
    # Not vacuous: the session was logged, through the real forwarder, under the right names.
    recs = [r for r in caplog.records if r.name == "discord.ext.songbird.native.conn.shared"]
    msgs = [r.getMessage() for r in recs]
    assert any("voice server update" in m for m in msgs), msgs
    assert any("closed" in m for m in msgs), msgs
    assert all(r.rust_target == "sbcore::conn::shared" for r in recs)  # type: ignore
    assert all(r.levelno in (logging.DEBUG, logging.INFO) for r in recs)
    text = "\n".join(r.getMessage() for r in caplog.records)
    assert "SECRET-TOKEN-123" not in text
    assert "SECRET-SESSION" not in text


@pytest.mark.real_only
def test_rust_side_filtering_real(testing: Any, caplog: pytest.LogCaptureFixture) -> None:
    """``set_log_levels`` filters in Rust: a session logged while the library is at INFO leaves no
    DEBUG record, the same session at DEBUG does (and, the ring being FIFO, once the later
    session's records have been forwarded the earlier one's would have been too)."""
    caplog.set_level(logging.DEBUG, logger="discord.ext.songbird")
    songbird.set_log_levels()  # library="INFO"
    _voice_session(testing, 4)
    songbird.set_log_levels(library="DEBUG")
    try:
        _voice_session(testing, 3)
    finally:
        songbird.set_log_levels()
    flush_native_logs()
    msgs = [r.getMessage() for r in caplog.records if r.name.startswith("discord.ext.songbird.native")]
    assert any("Some(3)" in m for m in msgs), msgs
    assert not any("Some(4)" in m for m in msgs), msgs


def test_forwarder_exits_at_shutdown() -> None:
    proc = run_isolated(
        """
        import threading, time
        from discord.ext import songbird
        assert any(t.name == "songbird-log" for t in threading.enumerate())
        songbird._atexit()
        time.sleep(0.1)
        assert not any(t.name == "songbird-log" and t.is_alive() for t in threading.enumerate())
        print("OK")
        """
    )
    assert proc.returncode == 0, proc.stderr
    assert "OK" in proc.stdout
