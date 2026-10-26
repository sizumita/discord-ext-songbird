"""Release smoke test for a freshly installed wheel or sdist build (design §13.7).

Run as ``python scripts/smoke.py``. It needs no network, no Discord token and no running loop; it
fails loudly (``AssertionError``) on any packaging problem: a missing native module, a test-hooks
build, stale 0.5 names, a missing codec, or a free-threaded build that re-enabled the GIL.
"""

from __future__ import annotations

import io
import os
import subprocess
import sys
import sysconfig
import wave

import discord.ext.songbird as sb

GIL_CHECK = (
    "import sys, discord.ext.songbird\n"
    "assert not sys._is_gil_enabled(), 'importing discord.ext.songbird re-enabled the GIL'\n"
)


def check_gil() -> None:
    """G7: on a free-threaded build, importing the package leaves the GIL disabled.

    ``PYTHON_GIL=0`` / ``-X gil=0`` keep the GIL off even when a module without ``Py_mod_gil`` asks
    for it (and ``PYTHON_GIL=1`` keeps it on), so a forced interpreter re-checks in a child that
    inherits neither."""
    if not sysconfig.get_config_var("Py_GIL_DISABLED"):
        return
    if sys.flags.gil is None:  # not forced either way
        assert not sys._is_gil_enabled(), "importing discord.ext.songbird re-enabled the GIL"
        return
    env = {k: v for k, v in os.environ.items() if k != "PYTHON_GIL"}
    child = subprocess.run([sys.executable, "-c", GIL_CHECK], env=env, capture_output=True, text=True)
    assert child.returncode == 0, child.stderr
    assert "has been enabled to load module" not in child.stderr, child.stderr


def main() -> None:
    f = sb.AudioFormat.parse("pcm16_24k")
    assert f.bytes_per_ms == 48 and sb.GPT_LIVE == f
    assert sb.__version__ == sb.native.VERSION
    sb.RawPcm(bytes(960), sb.PCM16_24K)
    sb.OpusPackets([b"\xf8\xff\xfe"])
    buf = io.BytesIO()
    w = wave.open(buf, "wb")
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(24000)
    w.writeframes(bytes(4800))
    w.close()
    sb.AudioFile(buf.getvalue())
    assert "opus" in sb.supported_codecs() and not hasattr(sb, "SupportedCodec")
    assert not hasattr(sb.native, "_testing"), "release wheels must not contain the test hooks"
    sb.native._debug_live_counts()
    check_gil()
    print(f"discord-ext-songbird {sb.__version__} OK ({', '.join(sb.supported_codecs())})")


if __name__ == "__main__":
    main()
