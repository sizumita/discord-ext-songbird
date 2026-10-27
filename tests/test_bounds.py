"""Bounds (§10, §13.5 ``test_bounds.py``): every user-controlled size is validated before any
arithmetic or allocation. Each case runs in a subprocess; an abort (exit code 134) or an RSS spike
fails it."""

from __future__ import annotations

import pytest
from conftest import run_isolated

PRELUDE = """
import resource, sys
from discord.ext import songbird
from discord.ext.songbird import native
from discord.ext.songbird.streams import ALL_EVENT_KINDS
T = native._testing
S = songbird.VoiceConfig().to_native()
def conn():
    c, h = T.detached_connection(1, 2, S)
    return c
def rss():
    r = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return r if sys.platform == "darwin" else r * 1024
"""

LIMIT_CASES = {
    "max_queue_ms": "conn().subscribe_mixed(songbird.PCM16_24K, 20, 10**15)",
    "max_buffer_ms": "conn().open_playback(songbird.PCM16_24K, 10**12)",
    "chunk_ms_0": "conn().subscribe_mixed(songbird.PCM16_24K, 0)",
    "chunk_ms_1001": "conn().subscribe_mixed(songbird.PCM16_24K, 1001)",
    "max_packets": "conn().open_opus(10**9)",
    "max_buffer_bytes": "conn().open_encoded(2**62)",
    "max_decoded_users": "songbird.VoiceConfig(max_decoded_users=10**9)",
    "silence": "songbird.PCM16_48K.silence(10**12)",
    "silence_stereo": "songbird.F32_48K_STEREO.silence(10**10)",
    "silence_negative": "songbird.PCM16_48K.silence(-1)",
    "prebuffer": "conn().open_playback(songbird.PCM16_24K, 100, 1000, 250, True, 0)",
    "max_gap": "conn().open_playback(songbird.PCM16_24K, 100, 0, 250, True, 10_000)",
    "rate_low": "songbird.RawPcm.from_samples(b'', 7_999, 2)",
    "rate_high": "songbird.RawPcm.from_samples(b'', 192_001, 2)",
    "driver_retries": "songbird.VoiceConfig(driver_retries=0)",
    "event_queue": "conn().subscribe_events(10**9, ALL_EVENT_KINDS)",
    "subscriptions": "c = conn(); subs = [c.subscribe_mixed(songbird.MULAW_8K) for _ in range(65)]",
    "live_sources": "c = conn(); srcs = [c.open_playback(songbird.PCM16_24K) for _ in range(17)]",
    "playing_tracks": "c = conn(); src = songbird.OpusPackets([b'\\xf8\\xff\\xfe']); ts = [c.play(src) for _ in range(33)]",
    "queue_entries": "c = conn(); src = songbird.OpusPackets([b'\\xf8\\xff\\xfe']); ts = [c.enqueue(src) for _ in range(10_001)]",
    "receive_budget": (
        "c = conn(); subs = [c.subscribe_mixed(songbird.F32_48K_STEREO, 1000, 60_000) for _ in range(10)]"
    ),
}

SEEK_CASES = ["-1.0", "float('nan')", "float('inf')", "1e300"]


def run_case(body: str) -> str:
    proc = run_isolated(
        PRELUDE
        + f"""
try:
    {body}
except ValueError as e:
    print("REJECTED", type(e).__name__, e)
else:
    print("ACCEPTED")
"""
    )
    assert proc.returncode != 134, proc.stderr
    assert proc.returncode == 0, proc.stderr
    return proc.stdout


@pytest.mark.parametrize("case", LIMIT_CASES)
def test_limits_are_rejected(case: str) -> None:
    out = run_case(LIMIT_CASES[case])
    assert "REJECTED" in out, out
    assert "LimitExceeded" in out or "ValueError" in out, out


@pytest.mark.parametrize("seconds", SEEK_CASES)
def test_seek_limits(seconds: str) -> None:
    out = run_case(
        f"c = conn(); t = c.play(songbird.OpusPackets([b'\\xf8\\xff\\xfe'])); c.start_track_seek(t, {seconds}, 1000)"
    )
    assert "REJECTED LimitExceeded" in out, out


def test_mulaw_16k_is_invalid() -> None:
    out = run_case("songbird.AudioFormat(16_000, 1, 'mulaw')")
    assert "REJECTED InvalidAudio" in out, out


#: 200 MiB payloads built without a transient copy (``ru_maxrss`` is a peak).
HUGE_PAYLOADS = {
    "bytes": 'b"\\x01" * (200 << 20)',
    "bytearray": 'bytearray(b"\\x01") * (200 << 20)',
    "memoryview": 'memoryview(bytearray(b"\\x01") * (200 << 20))',
}


@pytest.mark.parametrize("kind", HUGE_PAYLOADS)
def test_huge_write_is_rejected_before_parsing(kind: str) -> None:
    # bytes is borrowed; every other buffer would be copied, so its size is checked first too.
    proc = run_isolated(
        PRELUDE
        + f"""
c = conn()
pb = c.open_playback(songbird.PCM16_24K, 100, 0, 250, True, 0)
payload = {HUGE_PAYLOADS[kind]}
before = rss()
try:
    pb.write(payload)
except songbird.LimitExceeded as e:
    print("REJECTED", e.what)
print("GROWTH", rss() - before)
"""
    )
    assert proc.returncode == 0, proc.stderr
    assert "REJECTED write_bytes" in proc.stdout, proc.stdout
    growth = int(proc.stdout.split("GROWTH")[1])
    assert growth < 50 << 20, growth


#: Oversized raw-byte buffers. ``bytes`` is borrowed and every other buffer would be copied, so each
#: is size-checked first; an ``mmap`` is only backed by memory once its pages are touched, so a copy
#: of one shows up as RSS growth in full (§10, defect 6).
OVERSIZED_KINDS = {
    "bytes": "bytes({n})",
    "bytearray": "bytearray({n})",
    "memoryview": "memoryview(bytearray({n}))",
    "mmap": "mmap.mmap(-1, {n})",
}

#: Every entry point taking encoded bytes: ``(setup, call, expected rejection, payload size)``.
OVERSIZED_CALLS = {
    "encoded_write": (
        "c = conn(); s = c.open_encoded(1 << 20)",
        "s.write(payload)",
        "LimitExceeded write_bytes",
        200 << 20,
    ),
    "opus_send": ("c = conn(); s = c.open_opus()", "s.send(payload)", "InvalidAudio", 200 << 20),
    "opus_packets": ("", "songbird.OpusPackets([b'\\xf8\\xff\\xfe', payload])", "InvalidAudio", 200 << 20),
    # Above MAX_SOURCE_BYTES; only lazily backed buffers (and bytes) to keep the test's RSS small.
    "audio_file": ("", "songbird.AudioFile(payload)", "LimitExceeded source_bytes", (1 << 30) + 1),
}

OVERSIZED_PARAMS = [
    (call, kind)
    for call in OVERSIZED_CALLS
    for kind in OVERSIZED_KINDS
    if call != "audio_file" or kind in ("bytes", "mmap")
]


@pytest.mark.parametrize(("call", "kind"), OVERSIZED_PARAMS)
def test_oversized_encoded_bytes_are_rejected_before_copying(call: str, kind: str) -> None:
    setup, body, expected, n = OVERSIZED_CALLS[call]
    proc = run_isolated(
        PRELUDE
        + f"""
import mmap
{setup}
payload = {OVERSIZED_KINDS[kind].format(n=n)}
before = rss()
try:
    {body}
except (songbird.LimitExceeded, songbird.InvalidAudio) as e:
    print("REJECTED", type(e).__name__, getattr(e, "what", ""))
print("GROWTH", rss() - before)
"""
    )
    assert proc.returncode == 0, proc.stderr
    assert f"REJECTED {expected}" in proc.stdout, proc.stdout
    growth = int(proc.stdout.split("GROWTH")[1])
    assert growth < 50 << 20, growth


def growth_of(setup: str, body: str) -> int:
    proc = run_isolated(PRELUDE + f"\n{setup}\nbefore = rss()\n{body}\nprint('GROWTH', rss() - before)\n")
    assert proc.returncode == 0, proc.stderr
    return int(proc.stdout.split("GROWTH")[1])


@pytest.mark.parametrize(
    "call",
    ["songbird.RawPcm(payload, songbird.PCM16_48K)", "songbird.RawPcm.from_samples(payload, 48_000, 2, 'pcm16le')"],
)
def test_raw_pcm_parses_borrowed_bytes(call: str) -> None:
    # 200 MiB of pcm16 parse into 200 MiB of samples; an owned copy of the bytes first would double it.
    growth = growth_of("payload = b'\\x01' * (200 << 20)", f"src = {call}")
    assert growth < 300 << 20, growth


def test_opus_packets_copy_each_packet_once() -> None:
    # 100k packets of 1000 bytes (one shared object, so the input costs nothing): ~95 MiB of frames.
    # A per-packet copy concatenated again afterwards would need twice that.
    growth = growth_of("packets = [b'\\xf8' + bytes(999)] * 100_000", "src = songbird.OpusPackets(packets)")
    assert growth < 143 << 20, growth
