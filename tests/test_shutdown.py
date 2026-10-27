"""Process exit with live connections (§4.10, §13.5 ``test_shutdown.py``)."""

from __future__ import annotations

import json
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


@pytest.mark.real_only
@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs os.fork")
def test_every_inherited_handle_method_raises_in_a_forked_child() -> None:
    """§4.10, PR #12 review: an inherited handle's methods raise ``SongbirdError`` in a forked child
    before touching state (``OpusSink.clear()`` and the arming methods used to take queue locks a
    vanished parent thread may hold); ``close()`` is a no-op there and ``repr()`` still works. Every
    public method and property is called, so a new one without the guard fails here."""
    proc = run_isolated(
        """
        import inspect, json, os, socket, sys, time, warnings
        from discord.ext import songbird
        from discord.ext.songbird import native

        f = songbird.PCM16_48K_STEREO
        conn, _harness = native._testing.detached_connection(5, 6, native.VoiceSettings())
        src = native.RawPcm(bytes(3_840), f)
        _r, w = socket.socketpair()
        track = conn.play(src)
        handles = {
            "Connection": conn,
            "Playback": conn.open_playback(f),
            "EncodedSink": conn.open_encoded(),
            "OpusSink": conn.open_opus(),
            "ReceiveSubscription": conn.subscribe_user(7, f),
            "RouterSubscription": conn.subscribe_router(f),
            "EventSubscription": conn.subscribe_events(),
            "NativeTrack": track,
            "WakeBell": native.WakeBell(w.detach()),
        }
        ARGS = {
            ("Connection", "request_join"): (1, False, False),
            ("Connection", "request_move"): (1,),
            ("Connection", "on_voice_state"): ("s", 1),
            ("Connection", "on_voice_server"): ("t", "e"),
            ("Connection", "set_self_flags"): (False, False),
            ("Connection", "set_users_bot"): ([],),
            ("Connection", "subscribe_user"): (7, f),
            ("Connection", "subscribe_mixed"): (f,),
            ("Connection", "subscribe_router"): (f,),
            ("Connection", "open_playback"): (f,),
            ("Connection", "play"): (src,),
            ("Connection", "enqueue"): (src,),
            ("Connection", "queue_take_stream"): (track,),
            ("Connection", "queue_dequeue"): (0,),
            ("Connection", "set_bitrate"): (None,),
            ("Connection", "start_track_info"): (track, 100),
            ("Connection", "start_track_seek"): (track, 0.0, 100),
            ("Playback", "write"): (bytes(4),),
            ("Playback", "arm_space"): (4,),
            ("Playback", "end_segment"): ("t",),
            ("Playback", "segment"): ("t",),
            ("EncodedSink", "write"): (b"x",),
            ("EncodedSink", "arm_space"): (4,),
            ("OpusSink", "send"): (b"\\xf8\\xff\\xfe",),
            ("ReceiveSubscription", "set_filter"): ([], []),
            ("NativeTrack", "set_volume"): (1.0,),
            ("NativeTrack", "set_loops"): (0,),
        }
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", DeprecationWarning)  # fork() with threads running
            pid = os.fork()
        if pid == 0:
            bad, calls = [], 0
            for cls, obj in handles.items():
                try:
                    repr(obj)
                except BaseException as e:
                    bad.append(f"repr({cls}): {e!r}")
                for name, attr in inspect.getmembers_static(type(obj)):
                    if name.startswith("_"):
                        continue
                    calls += 1
                    try:
                        if callable(attr):
                            result = getattr(obj, name)(*ARGS.get((cls, name), ()))
                        else:
                            result = getattr(obj, name)
                    except songbird.SongbirdError as e:
                        if name == "close" or "fork" not in str(e):
                            bad.append(f"{cls}.{name}: {e!r}")
                        continue
                    except BaseException as e:
                        bad.append(f"{cls}.{name}: {e!r}")
                        continue
                    if name != "close" or result is not None:
                        bad.append(f"{cls}.{name} returned {result!r} in the child")
            print("CHILD", json.dumps({"bad": bad, "calls": calls}), flush=True)
            os._exit(0)
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
    report = json.loads(proc.stdout.split("CHILD ", 1)[1].splitlines()[0])
    assert report["calls"] > 60, report
    assert not report["bad"], report["bad"]
