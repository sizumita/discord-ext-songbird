"""Per-connection configuration and process-wide runtime settings (§9.3)."""

from __future__ import annotations

import dataclasses
from dataclasses import dataclass
from typing import Any, Literal, Self

from . import native

__all__ = ["VoiceConfig", "configure_runtime", "supported_codecs"]


@dataclass(frozen=True, slots=True, kw_only=True)
class VoiceConfig:
    """Per-connection settings, validated on construction.

    Parameters
    ----------
    receive : bool, default True
        ``False`` disables receive decoding entirely (songbird ``DecodeMode::Pass``); ``listen_*``
        then raise :class:`ReceiveDisabled`. ``events()`` still reports ``UserMapped`` and
        ``UserLeft``; ``SpeakingStarted``/``SpeakingStopped`` need received audio and never fire.
    playout_buffer_ms : int, default 60
        songbird jitter buffer, ``20..=200``, a multiple of 20.
    playout_spike_ms : int, default 60
        ``0..=200``, a multiple of 20.
    decode_state_timeout_s : float, default 60.0
        Idle SSRC decoder state is pruned after this long, ``1..=600``.
    speaking_hangover_ms : int, default 200
        Silence after which a speaker counts as stopped, ``20..=5000``.
    voice_threshold_dbfs : float or None, default None
        ``-100..=0``; ``None`` detects speech from packet activity only (no extra decoding).
    max_decoded_users : int, default 64
        Concurrently decoded SSRCs, ``1..=256`` (LRU beyond).
    max_receive_mb : int, default 64
        Sum of all receive-queue bounds per connection, ``1..=1024``.
    stall_timeout_ms : int, default 2000
        Receive watchdog, ``500..=30000``: reports ``ReceiveStalled`` when songbird's receive task
        stops delivering ticks. It cannot see a media path that died while that task keeps
        running; see ``media_timeout_ms``.
    media_timeout_ms : int or None, default None
        Opt-in media watchdog, ``5000..=3600000``; ``None`` disables it. When set, and while at
        least one receive consumer (``listen_*`` stream or router) exists, ``ReceiveStalled`` is
        reported once no valid voice packet has arrived for this long, and ``auto_recover``
        rejoins at once. See Notes.
    mix_mode : {"mono", "stereo"}, default "stereo"
        ``"mono"`` halves the Opus encode cost (voice bots).
    bitrate : int or None, default None
        ``6000..=510000`` bps; ``None`` keeps songbird's default (128 kbps).
    softclip : bool, default True
    driver_timeout_s : float, default 10.0
        Per transport attempt, ``1..=60``.
    driver_retries : int, default 5
        songbird transport retries with ``reconnect=True``, ``1..=10`` (never infinite).
    rejoin_attempts : int, default 3
        Gateway-level rejoins with ``reconnect=True``, ``0..=10``.
    auto_recover : bool, default True
        Rejoin after a receive stall of at least 5 s (DAVE / UDP-rx recovery), and as soon as the
        media watchdog (``media_timeout_ms``) reports one.

    Raises
    ------
    LimitExceeded
        A value is out of range.
    InvalidAudio
        A duration is not a multiple of 20 ms.
    ValueError
        ``mix_mode`` is not ``"mono"`` or ``"stereo"``.

    Notes
    -----
    ``media_timeout_ms`` exists because of upstream `serenity-rs/songbird#310
    <https://github.com/serenity-rs/songbird/issues/310>`_: with DAVE, a voice session can stop
    delivering decodable audio (after an MLS transition, or silently for every member) while the
    driver stays connected. songbird then reports nothing: it keeps emitting its 20 ms ticks from a
    timer, drops the undecryptable packets without an event, and a leave and rejoin is the only
    known recovery. From the library's side this is indistinguishable from a channel where nobody
    speaks, so the watchdog is a heuristic and stays off by default: with it enabled, a silent
    channel is also rejoined every ``media_timeout_ms`` (members hear the bot leave and join).
    Choose a value longer than the silences you expect, or keep ``auto_recover=False`` and decide
    in your own ``ReceiveStalled`` handler. ``stats()`` exposes the underlying signals
    (``ticks_media``, ``media_age_ms``, ``decrypt_failures``) for custom policies. The option may
    become unnecessary once the upstream issue is fixed.

    Examples
    --------
    >>> cfg = VoiceConfig.low_latency(receive=True)
    >>> vc = await channel.connect(cls=SongbirdClient.with_config(cfg))  # doctest: +SKIP
    """

    receive: bool = True
    playout_buffer_ms: int = 60
    playout_spike_ms: int = 60
    decode_state_timeout_s: float = 60.0
    speaking_hangover_ms: int = 200
    voice_threshold_dbfs: float | None = None
    max_decoded_users: int = 64
    max_receive_mb: int = 64
    stall_timeout_ms: int = 2_000
    media_timeout_ms: int | None = None
    mix_mode: Literal["mono", "stereo"] = "stereo"
    bitrate: int | None = None
    softclip: bool = True
    driver_timeout_s: float = 10.0
    driver_retries: int = 5
    rejoin_attempts: int = 3
    auto_recover: bool = True

    def __post_init__(self) -> None:
        self.to_native()

    @classmethod
    def low_latency(cls, **overrides: Any) -> Self:
        """Preset for interactive, low-latency audio: ``playout_buffer_ms=40``, ``mix_mode="mono"``.

        Parameters
        ----------
        **overrides
            Any other field (or an override of the preset's values).
        """
        fields: dict[str, Any] = {"playout_buffer_ms": 40, "mix_mode": "mono"}
        fields.update(overrides)
        return cls(**fields)

    def replace(self, **changes: Any) -> Self:
        """A copy with ``changes`` applied (validated, like ``dataclasses.replace``)."""
        return dataclasses.replace(self, **changes)

    def to_native(self) -> native.VoiceSettings:
        """The validated native settings object (``native.VoiceSettings``)."""
        return native.VoiceSettings(
            receive=self.receive,
            playout_buffer_ms=self.playout_buffer_ms,
            playout_spike_ms=self.playout_spike_ms,
            decode_state_timeout_s=self.decode_state_timeout_s,
            speaking_hangover_ms=self.speaking_hangover_ms,
            voice_threshold_dbfs=self.voice_threshold_dbfs,
            max_decoded_users=self.max_decoded_users,
            max_receive_mb=self.max_receive_mb,
            stall_timeout_ms=self.stall_timeout_ms,
            media_timeout_ms=self.media_timeout_ms,
            mix_mode=self.mix_mode,
            bitrate=self.bitrate,
            softclip=self.softclip,
            driver_timeout_s=self.driver_timeout_s,
            driver_retries=self.driver_retries,
            rejoin_attempts=self.rejoin_attempts,
            auto_recover=self.auto_recover,
        )


def configure_runtime(*, net_workers: int = 2, rx_workers: int = 1) -> None:
    """Size the two internal tokio runtimes (``1..=8`` workers each).

    Must be called before the first connection is created.

    Parameters
    ----------
    net_workers : int, default 2
        Workers running songbird's own tasks (gateway, UDP, events).
    rx_workers : int, default 1
        Workers running receive DSP and housekeeping.

    Raises
    ------
    RuntimeError
        If a runtime has already started.
    LimitExceeded
        If a count is out of range.
    """
    native.configure_runtime(net_workers, rx_workers)


def supported_codecs() -> list[str]:
    """Codec and container names compiled into this build (for ``AudioFile``/``EncodedStream``).

    Returns
    -------
    list[str]
        For example ``"opus"``, ``"mp3"``, ``"format:ogg"``.
    """
    return native.supported_codecs()
