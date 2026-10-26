//! Pull-based log forwarding (§11.2): the Python `songbird-log` daemon thread calls `_log_wait`,
//! which parks **detached** and then drains the Rust ring while attached. No Rust-created thread
//! ever attaches (I2).

use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3_stub_gen::derive::gen_stub_pyfunction;
use sbcore::limits::LOG_RING_CAP;
use sbcore::log::{Level, LevelFilter, LogRecord};

use crate::errors::{Exc, ensure_not_forked, msg_exc};

/// A Python `logging` level (`int`) or level name (`str`).
#[derive(FromPyObject)]
pub(crate) enum LevelArg {
    Int(i64),
    Name(String),
}

/// Python `logging` level → Rust filter: ≤5 TRACE, ≤10 DEBUG, ≤20 INFO, ≤30 WARN, ≤40 ERROR, else OFF.
pub(crate) fn level_filter(level: &LevelArg) -> PyResult<LevelFilter> {
    Ok(match level {
        LevelArg::Int(n) if *n <= 5 => LevelFilter::Trace,
        LevelArg::Int(n) if *n <= 10 => LevelFilter::Debug,
        LevelArg::Int(n) if *n <= 20 => LevelFilter::Info,
        LevelArg::Int(n) if *n <= 30 => LevelFilter::Warn,
        LevelArg::Int(n) if *n <= 40 => LevelFilter::Error,
        LevelArg::Int(_) => LevelFilter::Off,
        LevelArg::Name(s) => match s.trim().to_ascii_uppercase().as_str() {
            "TRACE" | "NOTSET" => LevelFilter::Trace,
            "DEBUG" => LevelFilter::Debug,
            "INFO" => LevelFilter::Info,
            "WARN" | "WARNING" => LevelFilter::Warn,
            "ERROR" => LevelFilter::Error,
            "CRITICAL" | "FATAL" | "OFF" => LevelFilter::Off,
            other => {
                return Err(PyValueError::new_err(format!(
                    "unknown log level {other:?}"
                )));
            }
        },
    })
}

/// Rust level → Python `logging` level.
pub(crate) fn py_level(level: Level) -> i32 {
    match level {
        Level::Error => 40,
        Level::Warn => 30,
        Level::Info => 20,
        Level::Debug => 10,
        Level::Trace => 5,
    }
}

/// Set Rust-side log filtering for a target prefix.
///
/// Parameters
/// ----------
/// prefix : str
///     Rust target prefix, module-path aware: ``"songbird"`` matches ``"songbird::driver"``.
///     The longest matching prefix wins; ``""`` sets the level of every other target.
/// level : int | str
///     A ``logging`` level (``logging.DEBUG``) or name (``"WARNING"``). ``CRITICAL`` disables.
///
/// Notes
/// -----
/// Defaults: this library INFO, everything else (songbird, rubato, …) WARNING.
#[gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn set_log_level(
    py: Python<'_>,
    prefix: &str,
    #[gen_stub(override_type(type_repr = "int | str"))] level: LevelArg,
) -> PyResult<()> {
    ensure_not_forked(py)?;
    let filter = level_filter(&level)?;
    if !sbcore::log::set_level(prefix, filter) {
        return Err(msg_exc(
            py,
            Exc::SongbirdError,
            "too many distinct log level prefixes",
        ));
    }
    Ok(())
}

/// Wait for Rust log records and drain them (used by the ``songbird-log`` thread).
///
/// Parameters
/// ----------
/// max : int
///     Maximum number of records to return (clamped to 1..=1024).
/// timeout_s : float
///     Longest time to park, detached from the interpreter, when no record is queued
///     (clamped to 0..=60 s).
///
/// Returns
/// -------
/// list[tuple[int, str, str, float]]
///     ``(logging level, rust target, message, unix timestamp)`` per record.
#[gen_stub_pyfunction]
#[pyfunction]
#[pyo3(name = "_log_wait")]
pub(crate) fn log_wait(
    py: Python<'_>,
    max: usize,
    timeout_s: f64,
) -> Vec<(i32, String, String, f64)> {
    let timeout = if timeout_s.is_finite() {
        Duration::from_secs_f64(timeout_s.clamp(0.0, 60.0))
    } else {
        Duration::ZERO
    };
    py.detach(|| sbcore::log::wait(timeout));
    let mut recs: Vec<LogRecord> = Vec::new();
    sbcore::log::drain(max.clamp(1, LOG_RING_CAP), &mut recs);
    recs.into_iter()
        .map(|r| {
            (
                py_level(r.level),
                r.target.into(),
                r.message.into(),
                r.unix_ts,
            )
        })
        .collect()
}

/// Number of Rust log records dropped (rate limit, contention, full ring) since the last call.
///
/// Returns
/// -------
/// int
///     The dropped-record count; the counter is reset.
#[gen_stub_pyfunction]
#[pyfunction]
#[pyo3(name = "_log_dropped")]
pub(crate) fn log_dropped() -> u64 {
    sbcore::log::take_dropped()
}
