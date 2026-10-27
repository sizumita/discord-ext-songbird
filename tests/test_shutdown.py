"""Process exit with live connections (§4.10, §13.5 ``test_shutdown.py``)."""

from __future__ import annotations

import os

import pytest
from conftest import run_isolated


def test_exit_without_disconnect() -> None:
    proc = run_isolated(
        """
        import asyncio
        from conftest import FakeClient, FakeGuild, World, connect_detached, inject_ticks
        from discord.ext.songbird import native

        keep = []

        async def main():
            for gid in range(3):
                client = FakeClient(); guild = FakeGuild(client); guild.id += gid
                world = World(client, guild, guild.add_channel(10), guild.add_channel(11))
                vc = await connect_detached(world)
                s = vc.listen_mixed(); p = vc.open_playback(); e = vc.events()
                p.write_nowait(bytes(4_800))
                inject_ticks(vc, native._testing, 3, voiced=False)
                keep.append((vc, s, p, e))
            # also an idle detached connection that no pump owns
            keep.append(native._testing.detached_connection(5, 6, native.VoiceSettings()))

        asyncio.run(main())
        print("EXITING")
        """
    )
    assert proc.returncode == 0, proc.stderr
    assert "EXITING" in proc.stdout
    assert "Fatal Python error" not in proc.stderr
    assert "Future exception was never retrieved" not in proc.stderr
    assert "Task was destroyed but it is pending" not in proc.stderr


def test_exit_inside_a_running_loop() -> None:
    proc = run_isolated(
        """
        import asyncio, os, sys
        from conftest import FakeClient, FakeGuild, World, connect_detached

        async def main():
            client = FakeClient(); guild = FakeGuild(client)
            world = World(client, guild, guild.add_channel(10), guild.add_channel(11))
            vc = await connect_detached(world)
            vc.listen_mixed()
            sys.stdout.write("EXITING\\n"); sys.stdout.flush()
            sys.exit(0)  # atexit runs native._shutdown_all() with the loop still alive

        asyncio.run(main())
        """
    )
    assert proc.returncode == 0, proc.stderr
    assert "EXITING" in proc.stdout
    assert "Fatal Python error" not in proc.stderr


@pytest.mark.real_only
@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs os.fork")
def test_forked_child_exits_while_a_vanished_thread_held_a_global_lock() -> None:
    """A child forked while another thread held the connection registry lock still exits: the
    inherited ``atexit`` hook takes no process-global lock after fork (§4.10)."""
    proc = run_isolated(
        """
        import os, sys, time, warnings
        from discord.ext.songbird import native

        native._testing.hold_registry_lock(1.0)  # held by a thread the child will not have
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", DeprecationWarning)  # fork() with threads running
            pid = os.fork()
        if pid == 0:
            sys.exit(0)  # atexit: native._shutdown_all()
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                print("CHILD EXIT", os.waitstatus_to_exitcode(status))
                break
            time.sleep(0.02)
        else:
            os.kill(pid, 9)
            os.waitpid(pid, 0)
            print("CHILD HUNG")
        """
    )
    assert proc.returncode == 0, proc.stderr
    assert "CHILD EXIT 0" in proc.stdout, proc.stdout + proc.stderr
