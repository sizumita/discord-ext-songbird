//! `native._testing` (feature `test-hooks`, §13.2): detached connections, input injection, RTP/Opus
//! helpers and the mixer harness. Never in release wheels; no stubs are generated for it.

use std::num::NonZeroU64;
use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use sbcore::conn::gateway::{DiscKind, DiscReason};
use sbcore::testing::{self as hooks, DriverEvent, RtpOpts};

use crate::errors::{CoreResultExt, ensure_not_forked};
use crate::py::connection::{Connection, PyVoiceSettings};
use crate::py::playback::Playback;
use crate::py::tracks::NativeTrack;
use crate::py::wake::WakeBell;

fn snowflake(what: &str, v: u64) -> PyResult<NonZeroU64> {
    NonZeroU64::new(v).ok_or_else(|| PyValueError::new_err(format!("{what} must be non-zero")))
}

/// The worker and manual clock of a detached connection.
#[pyclass(module = "discord.ext.songbird.native._testing", frozen)]
pub struct Harness {
    h: Arc<hooks::Harness>,
}

#[pymethods]
impl Harness {
    /// Advances the manual clock by ``advance_ms`` and runs one ConnWorker iteration.
    fn step(&self, py: Python<'_>, advance_ms: u64) -> PyResult<()> {
        self.h.step(advance_ms).py(py)
    }
}

/// A full connection with no songbird Driver and a manual clock.
#[pyfunction]
#[pyo3(signature = (guild_id, user_id, settings, bell = None))]
fn detached_connection(
    py: Python<'_>,
    guild_id: u64,
    user_id: u64,
    settings: PyVoiceSettings,
    bell: Option<PyRef<'_, WakeBell>>,
) -> PyResult<(Connection, Harness)> {
    ensure_not_forked(py)?;
    let doorbell = match bell {
        Some(b) => Some(
            b.take()
                .ok_or_else(|| PyValueError::new_err("this WakeBell was already taken"))?,
        ),
        None => None,
    };
    let (shared, h) = hooks::Harness::detached(
        snowflake("guild_id", guild_id)?,
        snowflake("user_id", user_id)?,
        settings.settings,
        doorbell,
    )
    .py(py)?;
    Ok((Connection::from_shared(shared), Harness { h: Arc::new(h) }))
}

/// One tick: ``voices = [(ssrc, opus | None), …]`` (``None`` = lost), ``silent = [ssrc, …]``.
#[pyfunction]
fn inject_tick(
    py: Python<'_>,
    conn: PyRef<'_, Connection>,
    voices: Vec<(u32, Option<Vec<u8>>)>,
    silent: Vec<u32>,
) -> PyResult<()> {
    hooks::inject_tick(conn.shared(), voices, silent).py(py)
}

/// ``SpeakingStateUpdate``: maps ``ssrc`` to ``user_id``.
#[pyfunction]
fn inject_map(
    py: Python<'_>,
    conn: PyRef<'_, Connection>,
    ssrc: u32,
    user_id: u64,
) -> PyResult<()> {
    hooks::inject_map(conn.shared(), ssrc, user_id).py(py)
}

/// ``ClientDisconnect``.
#[pyfunction]
fn inject_user_left(py: Python<'_>, conn: PyRef<'_, Connection>, user_id: u64) -> PyResult<()> {
    hooks::inject_user_left(conn.shared(), user_id).py(py)
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
/// ``kind``: ``"connect"`` / ``"reconnect"`` (with ``ssrc``) or ``"disconnect"`` /
/// ``"disconnect:connect"`` / ``"disconnect:reconnect"`` / ``"disconnect:runtime"`` (with
/// ``reason`` and, for ``"ws_closed"``, ``code``).
#[pyfunction]
#[pyo3(signature = (conn, kind, reason = None, code = None, ssrc = 0))]
fn inject_driver_event(
    py: Python<'_>,
    conn: PyRef<'_, Connection>,
    kind: &str,
    reason: Option<&str>,
    code: Option<u16>,
    ssrc: u32,
) -> PyResult<()> {
    let ev = match kind {
        "connect" => DriverEvent::Connect { ssrc },
        "reconnect" => DriverEvent::Reconnect { ssrc },
        other => {
            let dkind = match other.strip_prefix("disconnect") {
                Some("") => DiscKind::Runtime,
                Some(rest) => disc_kind(rest.trim_start_matches(':'))?,
                None => {
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

/// One tick of full RTP packets ``[(ssrc, packet), …]`` (exercises ``extract_opus``).
#[pyfunction]
fn inject_voice_tick_rtp(
    py: Python<'_>,
    conn: PyRef<'_, Connection>,
    packets: Vec<(u32, Vec<u8>)>,
) -> PyResult<()> {
    hooks::inject_voice_tick_rtp(conn.shared(), packets).py(py)
}

/// 48 kHz stereo PCM16LE → 20 ms Opus packets.
#[pyfunction]
fn encode_opus<'py>(py: Python<'py>, pcm48_stereo: &[u8]) -> PyResult<Vec<Bound<'py, PyBytes>>> {
    let packets = hooks::encode_opus(pcm48_stereo).py(py)?;
    Ok(packets.iter().map(|p| PyBytes::new(py, p)).collect())
}

/// Wraps an Opus payload in an RTP packet as songbird's Decrypt mode delivers it.
#[pyfunction]
#[pyo3(signature = (opus, seq, ssrc, *, extension = false, padding = 0, dave_trailer = false))]
fn rtp_wrap<'py>(
    py: Python<'py>,
    opus: &[u8],
    seq: u16,
    ssrc: u32,
    extension: bool,
    padding: u8,
    dave_trailer: bool,
) -> PyResult<Bound<'py, PyBytes>> {
    let opts = RtpOpts {
        extension,
        padding,
        dave_trailer,
    };
    let pkt = hooks::rtp_wrap(opus, seq, ssrc, opts).py(py)?;
    Ok(PyBytes::new(py, &pkt))
}

/// Drives a detached playback's reader/decoder pair exactly as songbird's mixer does.
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

    /// Pulls ``ticks`` ticks; returns interleaved 48 kHz ``f32`` little-endian bytes.
    fn pull<'py>(&self, py: Python<'py>, ticks: usize) -> PyResult<Bound<'py, PyBytes>> {
        let mut out = Vec::new();
        self.m.pull(ticks, &mut out).py(py)?;
        let bytes: Vec<u8> = out.iter().flat_map(|s| s.to_le_bytes()).collect();
        Ok(PyBytes::new(py, &bytes))
    }
}

/// The ``FakeTrackControl`` call log of a detached connection's track.
#[pyfunction]
fn track_log(track: PyRef<'_, NativeTrack>) -> Vec<&'static str> {
    hooks::track_log(track.track_ref())
}

/// Allocations on the calling thread while the counting guard is active.
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
