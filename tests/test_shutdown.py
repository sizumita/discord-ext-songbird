"""Process exit with live connections (§4.10, §13.5 ``test_shutdown.py``)."""

from __future__ import annotations

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
