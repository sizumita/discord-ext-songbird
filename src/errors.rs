//! `CoreError` → Python exception mapping (§11.1).
//!
//! The exception classes live in `discord/ext/songbird/errors.py` (pure Python, so they can inherit
//! from both `TimeoutError`/`ValueError`) and are imported lazily into [`EXC_TYPES`]: the single I1
//! allow-listed static holding Python objects (process-lifetime type objects, no cycles).
//!
//! Constructor contract with `errors.py` (positional arguments; the documented attributes are also
//! set on the instance after construction, so a class without a custom `__init__` works too):
//!
//! | class | args | attributes |
//! |---|---|---|
//! | `SongbirdError`, `InvalidAudio`, `ReceiveDisabled`, `SourceError`, `TrackError` | `(message,)` | — |
//! | `ConnectFailed` | `(reason, code)` | `reason`, `code` |
//! | `ConnectionClosed`, `StreamClosed`, `StreamOverflow` | `(reason,)` | `reason` |
//! | `PlaybackFull` | `(needed_ms, free_ms)` | `needed_ms`, `free_ms` |
//! | `LimitExceeded` | `(what, value, minimum, maximum)` | `what`, `value`, `minimum`, `maximum` |
//!
//! If `discord.ext.songbird.errors` cannot be imported, the closest builtin exception is raised
//! (`ValueError`, `RuntimeError`, `TimeoutError`).

use pyo3::BoundObject;
use pyo3::exceptions::{PyMemoryError, PyRuntimeError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyTuple, PyType};
use sbcore::{CoreError, LimitError, LimitNum};

/// The Python exception classes of `discord.ext.songbird.errors`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Exc {
    SongbirdError,
    NotConnected,
    ConnectFailed,
    ConnectTimeout,
    ConnectionClosed,
    StreamClosed,
    StreamOverflow,
    PlaybackFull,
    ReceiveDisabled,
    InvalidAudio,
    LimitExceeded,
    SourceError,
    TrackError,
}

const EXC_COUNT: usize = 13;
const ERRORS_MODULE: &str = "discord.ext.songbird.errors";

impl Exc {
    const ALL: [Exc; EXC_COUNT] = [
        Exc::SongbirdError,
        Exc::NotConnected,
        Exc::ConnectFailed,
        Exc::ConnectTimeout,
        Exc::ConnectionClosed,
        Exc::StreamClosed,
        Exc::StreamOverflow,
        Exc::PlaybackFull,
        Exc::ReceiveDisabled,
        Exc::InvalidAudio,
        Exc::LimitExceeded,
        Exc::SourceError,
        Exc::TrackError,
    ];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Exc::SongbirdError => "SongbirdError",
            Exc::NotConnected => "NotConnected",
            Exc::ConnectFailed => "ConnectFailed",
            Exc::ConnectTimeout => "ConnectTimeout",
            Exc::ConnectionClosed => "ConnectionClosed",
            Exc::StreamClosed => "StreamClosed",
            Exc::StreamOverflow => "StreamOverflow",
            Exc::PlaybackFull => "PlaybackFull",
            Exc::ReceiveDisabled => "ReceiveDisabled",
            Exc::InvalidAudio => "InvalidAudio",
            Exc::LimitExceeded => "LimitExceeded",
            Exc::SourceError => "SourceError",
            Exc::TrackError => "TrackError",
        }
    }

    fn index(self) -> usize {
        Exc::ALL.iter().position(|e| *e == self).unwrap_or(0)
    }

    fn fallback(self, msg: String) -> PyErr {
        match self {
            Exc::InvalidAudio | Exc::LimitExceeded => PyValueError::new_err(msg),
            Exc::ConnectTimeout => PyTimeoutError::new_err(msg),
            _ => PyRuntimeError::new_err(msg),
        }
    }
}

/// I1 allow-list: cached exception classes (process-lifetime type objects).
static EXC_TYPES: [PyOnceLock<Py<PyType>>; EXC_COUNT] = [const { PyOnceLock::new() }; EXC_COUNT];

/// The exception class, imported once from `discord.ext.songbird.errors`.
pub(crate) fn exc_type<'py>(py: Python<'py>, exc: Exc) -> Option<Bound<'py, PyType>> {
    let cell = EXC_TYPES.get(exc.index())?;
    cell.import(py, ERRORS_MODULE, exc.name()).ok().cloned()
}

/// Instantiates `exc(*args)`, sets `attrs`, and returns it as a `PyErr`. Falls back to a builtin
/// exception carrying `msg` if the class is unavailable or rejects the arguments.
pub(crate) fn new_exc<'py>(
    py: Python<'py>,
    exc: Exc,
    args: Vec<Bound<'py, PyAny>>,
    attrs: &[(&'static str, Bound<'py, PyAny>)],
    msg: &str,
) -> PyErr {
    if let Some(ty) = exc_type(py, exc)
        && let Ok(tuple) = PyTuple::new(py, args)
        && let Ok(obj) = ty.call1(tuple)
    {
        for (k, v) in attrs {
            let _ = obj.setattr(*k, v);
        }
        return PyErr::from_value(obj);
    }
    exc.fallback(msg.to_owned())
}

/// `exc(message)`.
pub(crate) fn msg_exc(py: Python<'_>, exc: Exc, msg: &str) -> PyErr {
    match any(py, msg) {
        Some(arg) => new_exc(py, exc, vec![arg], &[], msg),
        None => exc.fallback(msg.to_owned()),
    }
}

fn any<'py, T>(py: Python<'py>, v: T) -> Option<Bound<'py, PyAny>>
where
    T: IntoPyObject<'py>,
{
    v.into_pyobject(py).ok().map(|b| b.into_any().into_bound())
}

fn limit_num<'py>(py: Python<'py>, n: LimitNum) -> Option<Bound<'py, PyAny>> {
    match n {
        LimitNum::Int(i) => any(py, i),
        LimitNum::Float(f) => any(py, f),
    }
}

/// `LimitExceeded(what, value, minimum, maximum)`.
pub(crate) fn limit_exc(py: Python<'_>, l: &LimitError) -> PyErr {
    let msg = l.to_string();
    let parts = (
        any(py, l.what),
        limit_num(py, l.value),
        limit_num(py, l.min),
        limit_num(py, l.max),
    );
    match parts {
        (Some(what), Some(value), Some(min), Some(max)) => new_exc(
            py,
            Exc::LimitExceeded,
            vec![what.clone(), value.clone(), min.clone(), max.clone()],
            &[
                ("what", what),
                ("value", value),
                ("minimum", min),
                ("maximum", max),
            ],
            &msg,
        ),
        _ => Exc::LimitExceeded.fallback(msg),
    }
}

fn reason_exc(py: Python<'_>, exc: Exc, reason: &'static str, msg: &str) -> PyErr {
    match any(py, reason) {
        Some(r) => new_exc(py, exc, vec![r.clone()], &[("reason", r)], msg),
        None => exc.fallback(msg.to_owned()),
    }
}

/// Converts a core error while attached.
pub(crate) fn core_err(py: Python<'_>, e: CoreError) -> PyErr {
    let msg = e.to_string();
    match e {
        CoreError::Limit(l) => limit_exc(py, &l),
        CoreError::InvalidAudio(m) => msg_exc(py, Exc::InvalidAudio, &m),
        CoreError::Alloc => PyMemoryError::new_err(msg),
        CoreError::ConnectionClosed(r) => reason_exc(py, Exc::ConnectionClosed, r.as_str(), &msg),
        CoreError::StreamClosed(r) => reason_exc(py, Exc::StreamClosed, r.as_str(), &msg),
        CoreError::StreamOverflow => reason_exc(py, Exc::StreamOverflow, "overflow", &msg),
        CoreError::PlaybackFull { needed_ms, free_ms } => {
            match (any(py, needed_ms), any(py, free_ms)) {
                (Some(n), Some(f)) => new_exc(
                    py,
                    Exc::PlaybackFull,
                    vec![n.clone(), f.clone()],
                    &[("needed_ms", n), ("free_ms", f)],
                    &msg,
                ),
                _ => Exc::PlaybackFull.fallback(msg),
            }
        }
        CoreError::ReceiveDisabled => msg_exc(py, Exc::ReceiveDisabled, &msg),
        CoreError::ConnectFailed(reason) => {
            let code = py.None().into_bound(py);
            match any(py, reason.as_ref()) {
                Some(r) => new_exc(
                    py,
                    Exc::ConnectFailed,
                    vec![r.clone(), code.clone()],
                    &[("reason", r), ("code", code)],
                    &msg,
                ),
                None => Exc::ConnectFailed.fallback(msg),
            }
        }
        CoreError::Source(m) => msg_exc(py, Exc::SourceError, &m),
        // `FileNotFoundError`, `PermissionError`, … (pyo3's `io::Error` → `OSError` mapping).
        CoreError::Io(io) => PyErr::from(io),
        CoreError::Track(m) => msg_exc(py, Exc::TrackError, &m),
        CoreError::RuntimeConfigured => PyRuntimeError::new_err(
            "runtime already configured: configure_runtime() must be called before the first connection",
        ),
        CoreError::Forked => msg_exc(
            py,
            Exc::SongbirdError,
            "discord.ext.songbird is unusable after fork()",
        ),
        CoreError::FeatureMissing(f) => msg_exc(
            py,
            Exc::SongbirdError,
            &format!("built without the '{f}' feature"),
        ),
        CoreError::Internal(site) => msg_exc(
            py,
            Exc::SongbirdError,
            &format!("internal error at {site}; please report this bug"),
        ),
    }
}

/// `Result<T, CoreError>` → `PyResult<T>`.
pub(crate) trait CoreResultExt<T> {
    fn py(self, py: Python<'_>) -> PyResult<T>;
}

impl<T> CoreResultExt<T> for Result<T, CoreError> {
    fn py(self, py: Python<'_>) -> PyResult<T> {
        self.map_err(|e| core_err(py, e))
    }
}

impl<T> CoreResultExt<T> for Result<T, LimitError> {
    fn py(self, py: Python<'_>) -> PyResult<T> {
        self.map_err(|e| limit_exc(py, &e))
    }
}

/// `SongbirdError("… unusable after fork()")` in a forked child (§4.10).
pub(crate) fn ensure_not_forked(py: Python<'_>) -> PyResult<()> {
    sbcore::runtime::check_not_forked().py(py)
}
