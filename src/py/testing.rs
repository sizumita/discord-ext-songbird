//! `native._testing` (feature `test-hooks`, §13.2): detached connections, input injection, RTP/Opus
//! helpers and the mixer harness. Never in release wheels; no stubs are generated for it.
//!
//! Pyclass fields hold only `Arc<sbcore::…>` (I1). Numeric arguments are taken as Python `int`
//! and range-checked before narrowing (`LimitExceeded`, never `OverflowError`).

use std::num::NonZeroU64;
use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use sbcore::conn::gateway::{DiscKind, DiscReason};
use sbcore::testing::{self as hooks, DriverEvent, RtpOpts};

use crate::convert::int_arg;
use crate::errors::{CoreResultExt, ensure_not_forked};
use crate::py::connection::{Connection, PyVoiceSettings};
use crate::py::playback::Playback;
use crate::py::tracks::NativeTrack;
use crate::py::wake::WakeBell;

fn snowflake(what: &str, v: u64) -> PyResult<NonZeroU64> {
    NonZeroU64::new(v).ok_or_else(|| PyValueError::new_err(format!("{what} must be non-zero")))
}

fn ssrc_arg(py: Python<'_>, v: i64) -> PyResult<u32> {
    int_arg(py, "ssrc", v, 0..=u32::MAX)
}

fn user_arg(py: Python<'_>, v: i64) -> PyResult<u64> {
    int_arg(py, "user_id", v, 0..=u64::MAX)
}

/// The worker and manual clock of a detached connection.
///
/// Dropping it disposes of every parked songbird input of the connection (their ``DoneGuard``
/// paths run, as on a real ``SetTrack(None)``).
#[pyclass(module = "discord.ext.songbird.native._testing", frozen)]
pub struct Harness {
    h: Arc<hooks::Harness>,
}

#[pymethods]
impl Harness {
    /// Runs one ConnWorker iteration now, then advances the manual clock by ``advance_ms`` in
    /// steps of at most 20 ms, running one iteration after each (``0 <= advance_ms <= 3_600_000``).
    #[pyo3(signature = (advance_ms = 0))]
    fn step(&self, py: Python<'_>, advance_ms: i64) -> PyResult<()> {
        let ms = int_arg(
            py,
            "advance_ms",
            advance_ms,
            0..=hooks::harness::MAX_STEP_MS,
        )?;
        // Detached: the worker runs on the RX runtime in production, never attached to Python, so
        // a step (Opus decoding, mixing, resampling) must not hold the GIL on a GIL build.
        let h = &self.h;
        py.detach(|| h.step(ms)).py(py)
    }

    /// Manual time elapsed since the connection was created, in ms.
    #[getter]
    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.h.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Songbird inputs currently parked for this connection.
    #[getter]
    fn parked(&self) -> usize {
        self.h.parked()
    }
}

/// A full connection with no songbird Driver and a manual clock.
///
/// ``bell=None`` creates an internal doorbell drained by ``Harness.step`` (poll
/// ``Connection.take_ready()`` directly); pass a ``WakeBell`` to drive a real ``_Pump``.
#[pyfunction]
#[pyo3(signature = (guild_id, user_id, settings, bell = None, *, reconnect = true))]
fn detached_connection(
    py: Python<'_>,
    guild_id: i64,
    user_id: i64,
    settings: PyVoiceSettings,
    bell: Option<PyRef<'_, WakeBell>>,
    reconnect: bool,
) -> PyResult<(Connection, Harness)> {
    ensure_not_forked(py)?;
    let guild = snowflake("guild_id", int_arg(py, "guild_id", guild_id, 0..=u64::MAX)?)?;
    let user = snowflake("user_id", int_arg(py, "user_id", user_id, 0..=u64::MAX)?)?;
    let doorbell = match bell {
        Some(b) => Some(
            b.take()
                .ok_or_else(|| PyValueError::new_err("this WakeBell was already taken"))?,
        ),
        None => None,
    };
    let (shared, h) =
        hooks::Harness::detached(guild, user, settings.settings, doorbell, reconnect).py(py)?;
    Ok((Connection::from_shared(shared), Harness { h: Arc::new(h) }))
}

/// One tick: ``voices = [(ssrc, opus | None), …]`` (``None`` = lost), ``silent = [ssrc, …]``.
#[pyfunction]
#[pyo3(signature = (conn, voices, silent = Vec::new()))]
fn inject_tick(
    py: Python<'_>,
    conn: PyRef<'_, Connection>,
    voices: Vec<(i64, Option<Vec<u8>>)>,
    silent: Vec<i64>,
) -> PyResult<()> {
    let voices = voices
        .into_iter()
        .map(|(s, p)| Ok((ssrc_arg(py, s)?, p)))
        .collect::<PyResult<Vec<_>>>()?;
    let silent = silent
        .into_iter()
        .map(|s| ssrc_arg(py, s))
        .collect::<PyResult<Vec<_>>>()?;
    hooks::inject_tick(conn.shared(), voices, silent).py(py)
}

/// ``SpeakingStateUpdate``: maps ``ssrc`` to ``user_id``.
#[pyfunction]
fn inject_map(
    py: Python<'_>,
    conn: PyRef<'_, Connection>,
    ssrc: i64,
    user_id: i64,
) -> PyResult<()> {
    let ssrc = ssrc_arg(py, ssrc)?;
    let user = user_arg(py, user_id)?;
    hooks::inject_map(conn.shared(), ssrc, user).py(py)
}

/// ``ClientDisconnect``.
#[pyfunction]
fn inject_user_left(py: Python<'_>, conn: PyRef<'_, Connection>, user_id: i64) -> PyResult<()> {
    let user = user_arg(py, user_id)?;
    hooks::inject_user_left(conn.shared(), user).py(py)
}

fn disc_kind(kind: &str) -> PyResult<DiscKind> {
    match kind {
        "connect" => Ok(DiscKind::Connect),
        "reconnect" => Ok(DiscKind::Reconnect),
        "runtime" => Ok(DiscKind::Runtime),
        other => Err(PyValueError::new_err(format!(
            "unknown disconnect kind {other:?}"
        ))),
    }
}

fn disc_reason(reason: &str, code: Option<u16>) -> PyResult<DiscReason> {
    match reason {
        "attempt_discarded" => Ok(DiscReason::AttemptDiscarded),
        "internal" => Ok(DiscReason::Internal),
        "io" => Ok(DiscReason::Io),
        "protocol_violation" => Ok(DiscReason::ProtocolViolation),
        "timed_out" => Ok(DiscReason::TimedOut),
        "requested" => Ok(DiscReason::Requested),
        "ws_closed" => Ok(DiscReason::WsClosed(code)),
        "other" => Ok(DiscReason::Other),
        other => Err(PyValueError::new_err(format!(
            "unknown disconnect reason {other:?}"
        ))),
    }
}

/// A driver event through the same ``apply()`` path ``Dispatch`` uses.
///
/// ``kind``:
///
/// * ``"connect"`` (with ``ssrc``): ``DriverConnect``, then — while ``connecting`` — the connect
///   future's successful result, so the state becomes ``connected``;
/// * ``"connect_failed"`` / ``"connect_timeout"``: the connect future fails;
/// * ``"reconnect"`` (with ``ssrc``): ``DriverReconnect``;
/// * ``"disconnect"`` / ``"disconnect:connect"`` / ``"disconnect:reconnect"`` /
///   ``"disconnect:runtime"`` (with ``reason`` and, for ``"ws_closed"``, ``code``).
#[pyfunction]
#[pyo3(signature = (conn, kind, reason = None, code = None, ssrc = 0))]
fn inject_driver_event(
    py: Python<'_>,
    conn: PyRef<'_, Connection>,
    kind: &str,
    reason: Option<&str>,
    code: Option<i64>,
    ssrc: i64,
) -> PyResult<()> {
    let ssrc = ssrc_arg(py, ssrc)?;
    let code = code
        .map(|c| int_arg(py, "code", c, 0..=u16::MAX))
        .transpose()?;
    let ev = match kind {
        "connect" => DriverEvent::Connect { ssrc },
        "connect_failed" => DriverEvent::ConnectFailed { timed_out: false },
        "connect_timeout" => DriverEvent::ConnectFailed { timed_out: true },
        "reconnect" => DriverEvent::Reconnect { ssrc },
        other => {
            let dkind = match other.strip_prefix("disconnect") {
                Some("") => DiscKind::Runtime,
                Some(rest) if rest.starts_with(':') => disc_kind(rest.trim_start_matches(':'))?,
                _ => {
                    return Err(PyValueError::new_err(format!(
                        "unknown driver event {other:?}"
                    )));
                }
            };
            DriverEvent::Disconnect {
                kind: dkind,
                reason: reason.map(|r| disc_reason(r, code)).transpose()?,
            }
        }
    };
    hooks::inject_driver_event(conn.shared(), ev).py(py)
}

/// One tick of full RTP packets ``[(ssrc, packet), …]`` as ``rtp_wrap`` builds them (exercises
/// ``extract_opus``).
#[pyfunction]
fn inject_voice_tick_rtp(
    py: Python<'_>,
    conn: PyRef<'_, Connection>,
    packets: Vec<(i64, Vec<u8>)>,
) -> PyResult<()> {
    let packets = packets
        .into_iter()
        .map(|(s, p)| Ok((ssrc_arg(py, s)?, p)))
        .collect::<PyResult<Vec<_>>>()?;
    hooks::inject_voice_tick_rtp(conn.shared(), packets).py(py)
}

/// 48 kHz stereo PCM16LE → 20 ms Opus packets (the last one zero-padded; at most 60 s).
#[pyfunction]
fn encode_opus<'py>(py: Python<'py>, pcm48_stereo: &[u8]) -> PyResult<Vec<Bound<'py, PyBytes>>> {
    let packets = hooks::encode_opus(pcm48_stereo).py(py)?;
    Ok(packets.iter().map(|p| PyBytes::new(py, p)).collect())
}

/// Wraps an Opus payload in an RTP packet as songbird's Decrypt mode delivers it (header
/// extension, padding and the DAVE supplemental block unstripped; 20-byte crypto suffix).
#[pyfunction]
#[pyo3(signature = (opus, seq, ssrc, *, extension = false, padding = 0, dave_trailer = false))]
fn rtp_wrap<'py>(
    py: Python<'py>,
    opus: &[u8],
    seq: i64,
    ssrc: i64,
    extension: bool,
    padding: i64,
    dave_trailer: bool,
) -> PyResult<Bound<'py, PyBytes>> {
    let opts = RtpOpts {
        extension,
        padding: int_arg(py, "padding", padding, 0..=u8::MAX)?,
        dave_trailer,
    };
    let seq = int_arg(py, "seq", seq, 0..=u16::MAX)?;
    let pkt = hooks::rtp_wrap(opus, seq, ssrc_arg(py, ssrc)?, opts).py(py)?;
    Ok(PyBytes::new(py, &pkt))
}

/// Drives a detached playback's reader/decoder pair exactly as songbird's mixer does.
///
/// A paused track is not pulled (live sources start paused); ``pull`` then returns silence.
#[pyclass(module = "discord.ext.songbird.native._testing", frozen)]
pub struct MixerHarness {
    m: Arc<hooks::MixerHarness>,
}

#[pymethods]
impl MixerHarness {
    #[new]
    fn py_new(py: Python<'_>, playback: PyRef<'_, Playback>) -> PyResult<Self> {
        let m = hooks::MixerHarness::take(playback.queue()).py(py)?;
        Ok(MixerHarness { m })
    }

    /// Pulls ``ticks`` ticks (``0..=3000``); returns interleaved 48 kHz ``f32`` little-endian
    /// bytes (``960 × channels`` samples per tick).
    fn pull<'py>(&self, py: Python<'py>, ticks: i64) -> PyResult<Bound<'py, PyBytes>> {
        let ticks = int_arg(py, "ticks", ticks, 0..=hooks::mixer::MAX_PULL_TICKS)?;
        let mut out = Vec::new();
        // Detached, like songbird's mixer thread, which never attaches to Python.
        let m = &self.m;
        py.detach(|| m.pull(ticks, &mut out)).py(py)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(out.len().saturating_mul(4))
            .map_err(|_| pyo3::exceptions::PyMemoryError::new_err("MixerHarness.pull"))?;
        for s in &out {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        Ok(PyBytes::new(py, &bytes))
    }

    /// Pulls that reached the reader (paused and ended ticks are not pulls).
    #[getter]
    fn pulls(&self) -> u64 {
        self.m.pulls()
    }

    /// Whether the track ended (its reader/decoder pair was dropped).
    #[getter]
    fn ended(&self) -> bool {
        self.m.ended()
    }

    /// Output channels (1 or 2).
    #[getter]
    fn channels(&self) -> usize {
        self.m.channels()
    }

    /// Drops the reader/decoder pair now, as songbird's ``SetTrack(None)`` would.
    fn drop_track(&self) {
        self.m.drop_track();
    }
}

/// The ``FakeTrackControl`` call log of a detached connection's track.
#[pyfunction]
fn track_log(track: PyRef<'_, NativeTrack>) -> Vec<&'static str> {
    hooks::track_log(track.track_ref())
}

/// Allocations on the calling thread while the counting guard is active (the mixer callbacks of
/// ``MixerHarness.pull``).
#[pyfunction]
fn alloc_count() -> u64 {
    hooks::alloc_count()
}

/// The `native._testing` submodule.
#[pymodule(submodule, name = "_testing", gil_used = false)]
pub mod testing_module {
    #[pymodule_export]
    use super::{
        Harness, MixerHarness, alloc_count, detached_connection, encode_opus, inject_driver_event,
        inject_map, inject_tick, inject_user_left, inject_voice_tick_rtp, rtp_wrap, track_log,
    };
}
